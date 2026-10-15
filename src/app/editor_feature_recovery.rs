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
    /// Pure original-project attribution, including an unopened-only publisher
    /// whose source card has already closed. No displayed-path lookup occurs.
    pub(super) fn pending_publication_ticket_for_project(&self, project: crate::model::ProjectId, cards: &HashSet<CardId>) -> Option<u64> {
        self.editor_features.pending_publication_ticket_for_project(project, cards)
    }
    pub(super) fn publication_recovery_pending(&self) -> bool {
        self.editor_features.recovery.as_ref().is_some_and(|state| !state.authority.is_resolved())
    }
    /// Exact unresolved window fallback snapshot for a refusal-only logical
    /// project marker. It grants no root, retry, or new file-operation proof.
    pub(super) fn publication_recovery_authority(&self, worktree: &Path) -> Option<crate::editor::RecoveryAuthority> {
        self.editor_features.recovery.as_ref().filter(|state| !state.authority.is_resolved()
            && (state.unknown_scope || state.scopes.contains(worktree))).map(|state| state.authority.clone())
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
        // The project owner retires the exact pending ticket even on a result
        // with no recovery, before this helper's stale-owner/empty return.
        self.retain_relocated_publication_recovery(ticket, authority.filter(|authority| !authority.is_resolved()));
        let Some(authority) = authority.filter(|authority| !authority.is_resolved()) else { return; };
        let origin = self.editor_features.publication_origin(ticket);
        if let Some(CardRuntime::Repository(runtime)) = origin.and_then(|card| self.cards.get_mut(&card)) {
            runtime.retain_editor_recovery(authority.clone(), message.clone());
        }
        self.retain_window_file_recovery(authority, scope.map(PathBuf::as_path), message);
    }

    /// Every typed file completion is retained before routing can discard a
    /// stale card, UUID, selection, or a callback behind a modal/freeze barrier.
    /// Captured worktree names restrict refusals only; retry uses the authority.
    pub(super) fn retain_repository_action_recovery(&mut self, action: &RepositoryAction) {
        use crate::editor::{FileIoError, SaveOutcome};
        if self.editor_features.recovery.as_ref().is_some_and(|state|
            state.pending.is_none() && state.authority.is_resolved()) {
            self.editor_features.recovery = None;
        }
        let (identity, authority) = match action {
            RepositoryAction::EditorFilePrepared { identity, preparation } =>
                (identity, preparation.error().and_then(FileIoError::recovery_authority)),
            RepositoryAction::EditorFileAdmitted { identity, admission } =>
                (identity, admission.error().and_then(FileIoError::recovery_authority)),
            RepositoryAction::SaveFinished { identity, result, .. } => (identity, match result {
                Ok(SaveOutcome::Saved { durability, .. } | SaveOutcome::Absent { durability, .. }) => durability.recovery_authority(),
                Err(error) => error.recovery_authority(),
                _ => None,
            }),
            RepositoryAction::DiskLoaded { identity, result: Err(error), .. }
            | RepositoryAction::FileLoaded { identity, result: Err(error) }
            | RepositoryAction::ConfiguredFileLoaded { identity, result: Err(error), .. }
            | RepositoryAction::BinaryPageLoaded { identity, result: Err(error), .. } =>
                (identity, error.recovery_authority()),
            _ => return,
        };
        let Some(authority) = authority.filter(|authority| !authority.is_resolved()) else { return; };
        let scope = (!identity.worktree.as_os_str().is_empty()).then_some(identity.worktree.as_path());
        self.retain_window_file_recovery(authority, scope,
            format!("A file operation needs recovery before another disk mutation or closing this window. {}", authority.description()));
    }

    fn retain_window_file_recovery(&mut self, authority: &crate::editor::RecoveryAuthority,
        scope: Option<&Path>, message: String) {
        let state = self.editor_features.recovery.get_or_insert_with(|| State {
            authority: authority.clone(), scopes: HashSet::new(), unknown_scope: false, pending: None, message: String::new(),
        });
        state.authority = state.authority.clone().combine(authority.clone());
        state.message = message.chars().take(4096).collect();
        match scope {
            Some(scope) if state.scopes.len() < 32 || state.scopes.contains(scope) => { state.scopes.insert(scope.to_owned()); },
            _ => state.unknown_scope = true,
        }
        let cancel_all = state.unknown_scope;
        self.cancel_git_for_file_recovery(if cancel_all { None } else { scope });
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
        let cards = self.cards.iter().filter_map(|(card, runtime)| match runtime {
            CardRuntime::Repository(runtime) if !self.publication_recovery_blocks(&runtime.descriptor.worktree) => Some(*card),
            _ => None,
        }).collect::<Vec<_>>();
        let tasks = cards.into_iter().filter_map(|card| match self.cards.get_mut(&card) {
            Some(CardRuntime::Repository(runtime)) => Some(repository_task(runtime.auto_save_task(card))),
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
    async fn refused_actual_autosave_timer_is_retired_and_resumed_after_recovery() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap(); let card = CardId::new();
        let (mut runtime, _) = repository_widget::RepositoryRuntime::open_fixture(card, "note.rs", "baseline");
        runtime.descriptor.worktree = directory.path().into(); runtime.set_auto_save(true);
        drop(runtime.update(RepositoryAction::EditorAction { card, action: repository_widget::EditorAction::Edit(repository_widget::EditorEdit::Insert('x')) }));
        let mut timer = iced::runtime::task::into_stream(runtime.auto_save_task(card)).expect("actual autosave timer");
        let due = loop {
            match timer.next().await.expect("timer must deliver") {
                iced::runtime::Action::Output(action @ RepositoryAction::AutoSaveDue { .. }) => break action,
                _ => {},
            }
        };
        let RepositoryAction::AutoSaveDue { ticket: old_ticket, .. } = &due else { unreachable!() }; let old_ticket = *old_ticket;
        app.cards.insert(card, CardRuntime::Repository(Box::new(runtime)));
        let authority = crate::editor::RecoverySlot::reserve().unwrap().retain("another publisher's exact cleanup".into(), || Ok(()));
        let failure = crate::editor_workspace_edit::PublicationFailure { message: "cleanup pending".into(), recovery: Some(authority.clone()), worktree: Some(directory.path().into()) };
        drop(app.update(Message::EditorFeatures(Action::Published { ticket: 999, result: Err(failure) })));
        drop(app.update(Message::Repository(due)));
        assert!(matches!(&app.cards[&card], CardRuntime::Repository(runtime) if !runtime.is_saving() && runtime.is_dirty()));
        let retry = app.update(Message::EditorFeatures(Action::RetryPublicationRecovery(authority)));
        let mut worker = iced::runtime::task::into_stream(retry).expect("real retry worker");
        let resumed = loop {
            match worker.next().await.expect("worker completion must deliver") {
                iced::runtime::Action::Output(cosmic::Action::App(message @ Message::EditorFeatures(Action::PublicationRecoveryResolved { .. }))) => break app.update(message),
                _ => {},
            }
        };
        assert!(!app.publication_recovery_pending());
        let mut timer = iced::runtime::task::into_stream(resumed).expect("recovery must install a replacement timer");
        let due = loop {
            match timer.next().await.expect("replacement timer must deliver") {
                iced::runtime::Action::Output(cosmic::Action::App(Message::Repository(action @ RepositoryAction::AutoSaveDue { .. }))) => break action,
                _ => {},
            }
        };
        assert!(matches!(&due, RepositoryAction::AutoSaveDue { ticket, .. } if *ticket != old_ticket));
        drop(app.update(Message::Repository(due)));
        assert!(matches!(&app.cards[&card], CardRuntime::Repository(runtime) if runtime.is_saving()), "unchanged dirty revision must regain its ordinary save intent");
    }

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
        let original_marker = app.publication_recovery_authority(root.path()).expect("capture this exact unresolved window object");
        assert_eq!(original_marker, authority);
        assert!(app.publication_recovery_authority(other.path()).is_none());
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
        assert!(original_marker.is_resolved());
        assert!(app.publication_recovery_authority(root.path()).is_none());
        assert!(app.publication_recovery_view().is_none());
    }
}
