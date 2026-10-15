//! Window-owned fallback for unopened-file recovery, including stale publisher
//! receipts whose originating card no longer exists. Never serialize authority.
use super::*;
use super::editor_features::Action;

pub(super) struct State {
    authority: crate::editor::RecoveryAuthority,
    scopes: HashSet<PathBuf>,
    unknown_scope: bool,
    pending: Option<crate::editor::RecoveryAuthority>,
    message: String,
}
impl Cagents {
    pub(super) fn publication_recovery_pending(&self) -> bool {
        self.editor_features.recovery.as_ref().is_some_and(|state| !state.authority.is_resolved())
    }
    /// Cheap reducer-side scope guard. Concrete effects still use their held
    /// original roots; this predicate never resolves diagnostic paths.
    pub(super) fn publication_recovery_blocks(&self, worktree: &Path) -> bool {
        self.editor_features.recovery.as_ref().is_some_and(|state| !state.authority.is_resolved()
            && (state.unknown_scope || state.scopes.contains(worktree)))
            || self.cards.values().any(|runtime| match runtime {
                CardRuntime::Repository(runtime) => runtime.descriptor.worktree == worktree && runtime.has_editor_recovery(),
                CardRuntime::PullRequests(pane) => pane.runtime.descriptor.worktree == worktree && pane.runtime.has_editor_recovery(),
                _ => false,
            })
    }
    pub(super) fn retain_publication_result_recovery(&mut self, ticket: u64, result: &Result<crate::editor_workspace_edit::PublicationReport, crate::editor_workspace_edit::PublicationFailure>) {
        let (authority, scope, message) = match result {
            Ok(report) => (report.recovery.as_ref(), report.worktree.as_ref(), report.durability_warnings.join("\n")),
            Err(failure) => (failure.recovery.as_ref(), failure.worktree.as_ref(), failure.message.clone()),
        };
        let Some(authority) = authority.filter(|authority| !authority.is_resolved()) else { return; };
        let origin = self.editor_features.publication_origin(ticket);
        if let Some(CardRuntime::Repository(runtime)) = origin.and_then(|card| self.cards.get_mut(&card)) {
            runtime.retain_editor_recovery(authority.clone(), message.clone());
        }
        let state = self.editor_features.recovery.get_or_insert_with(|| State {
            authority: authority.clone(), scopes: HashSet::new(), unknown_scope: false, pending: None, message: String::new(),
        });
        state.authority = state.authority.clone().combine(authority.clone());
        state.message = message.chars().take(4096).collect();
        match scope {
            Some(scope) if state.scopes.len() < 32 || state.scopes.contains(scope) => { state.scopes.insert(scope.clone()); },
            _ => state.unknown_scope = true,
        }
    }
    pub(super) fn start_publication_recovery(&mut self, authority: crate::editor::RecoveryAuthority) -> Task<Message> {
        let Some(state) = &mut self.editor_features.recovery else { return Task::none(); };
        if state.authority != authority || state.pending.is_some() { return Task::none(); }
        state.pending = Some(authority.clone());
        cosmic::task::future(async move {
            let worker = authority.clone();
            let result = tokio::task::spawn_blocking(move || worker.retry()).await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::EditorFeatures(Action::PublicationRecoveryResolved { authority, result })
        })
    }
    pub(super) fn finish_publication_recovery(&mut self, authority: crate::editor::RecoveryAuthority, result: Result<(), String>) {
        let Some(state) = &mut self.editor_features.recovery else { return; };
        if state.pending.as_ref() != Some(&authority) { return; }
        state.pending = None;
        match result {
            Ok(()) if authority.is_resolved() && state.authority == authority => self.editor_features.recovery = None,
            Ok(()) => state.message = "An earlier operation recovered. Another retained file operation still needs recovery.".into(),
            Err(error) => state.message = format!("File recovery remains pending. Retained files were kept. {error}").chars().take(4096).collect(),
        }
    }
    pub(super) fn resume_recovered_editor_autosaves(&mut self) -> Task<Message> {
        let tasks = self.cards.iter_mut().filter_map(|(card, runtime)| match runtime {
            CardRuntime::Repository(runtime) => Some(repository_task(runtime.auto_save_task(*card))),
            _ => None,
        }).collect::<Vec<_>>();
        Task::batch(tasks)
    }
    pub(super) fn publication_recovery_view(&self) -> Option<Element<'_, Message>> {
        let state = self.editor_features.recovery.as_ref().filter(|state| !state.authority.is_resolved())?;
        Some(widget::container(widget::row::with_children(vec![
            design::body(&state.message).width(Length::Fill).into(),
            crate::ui_controls::button::secondary("Retry file recovery")
                .on_press_maybe(state.pending.is_none().then(|| Message::EditorFeatures(Action::RetryPublicationRecovery(state.authority.clone())))).into(),
        ]).spacing(design::space::S)).padding(design::space::M).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::futures::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[tokio::test]
    async fn orphaned_publication_receipt_keeps_scoped_recovery_and_real_retry_after_failed_attempt() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let root = tempfile::tempdir().unwrap(); let other = tempfile::tempdir().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0)); let captured = attempts.clone();
        let authority = crate::editor::RecoverySlot::reserve().unwrap().retain("exact original publisher".into(), move || {
            if captured.fetch_add(1, Ordering::AcqRel) == 0 { Err("foreign name was preserved".into()) } else { Ok(()) }
        });
        let failure = crate::editor_workspace_edit::PublicationFailure {
            message: "earlier live contents restored; cleanup still owns files".into(), recovery: Some(authority.clone()), worktree: Some(root.path().into()),
        };
        // No publication/card remains. Actual reducer dispatch must retain
        // before its stale-ticket return rather than stringify/drop the error.
        drop(app.update(Message::EditorFeatures(Action::Published { ticket: 999, result: Err(failure) })));
        assert!(app.publication_recovery_pending()); assert!(app.publication_recovery_blocks(root.path()));
        assert!(!app.publication_recovery_blocks(other.path()));
        assert!(app.publication_recovery_view().is_some());
        drop(app.exit_after_language_servers()); assert!(app.exit.is_none());
        for final_attempt in [false, true] {
            let task = app.update(Message::EditorFeatures(Action::RetryPublicationRecovery(authority.clone())));
            let mut stream = iced::runtime::task::into_stream(task).expect("real recovery worker");
            let mut completed = false;
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                    if matches!(&message, Message::EditorFeatures(Action::PublicationRecoveryResolved { .. })) { completed = true; }
                    drop(app.update(message));
                }
            }
            assert!(completed); assert_eq!(app.publication_recovery_pending(), !final_attempt);
        }
        assert!(authority.is_resolved()); assert_eq!(attempts.load(Ordering::Acquire), 2);
        assert!(app.publication_recovery_view().is_none());
    }
}
