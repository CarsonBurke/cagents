//! Retains live file recovery independently of document replacement and stale
//! worker receipts. A single flat authority contains at most32 reserved leaves.
use super::*;

pub(super) struct State {
    pub(super) authority: crate::editor::RecoveryAuthority,
    pub(super) message: String,
    pub(super) pending: Option<crate::editor::RecoveryAuthority>,
}
impl RepositoryRuntime {
    pub fn has_editor_recovery(&self) -> bool { self.recovery.is_some() || self.recovery_capture_ticket.is_some() }

    /// A publisher can hand an unopened-file recovery to its owning runtime.
    /// The authority itself retains the exact source descriptors and paths.
    pub fn retain_editor_recovery(&mut self, authority: crate::editor::RecoveryAuthority, message: String) {
        if authority.is_resolved() { return; }
        let message = message.chars().take(4096).collect();
        match &mut self.recovery {
            Some(state) => {
                state.authority = state.authority.clone().combine(authority);
                state.message = message;
            }
            None => self.recovery = Some(State { authority, message, pending: None }),
        }
    }
    pub(super) fn retain_action_recovery(&mut self, action: &RepositoryAction) {
        if let RepositoryAction::DiskLoaded { ticket, .. } = action
            && self.recovery_capture_ticket == Some(*ticket) {
            self.recovery_capture_ticket = None;
        }
        let authority = match action {
            RepositoryAction::SaveFinished { result, .. } => match result {
                Ok(SaveOutcome::Saved { durability, .. } | SaveOutcome::Absent { durability, .. }) => durability.recovery_authority(),
                Err(error) => error.recovery_authority(),
                _ => None,
            },
            RepositoryAction::DiskLoaded { result: Err(error), .. } => error.recovery_authority(),
            _ => None,
        };
        if let Some(authority) = authority {
            self.retain_editor_recovery(authority.clone(), format!("A file operation needs recovery before another disk mutation or closing this editor. {}", authority.description()));
        }
    }
    pub(super) fn start_editor_recovery(&mut self, card: CardId, authority: crate::editor::RecoveryAuthority) -> Task<RepositoryAction> {
        let Some(state) = &mut self.recovery else { return Task::none(); };
        if state.authority != authority || state.pending.is_some() { return Task::none(); }
        state.pending = Some(authority.clone());
        cosmic::task::future(async move {
            let worker = authority.clone();
            let result = tokio::task::spawn_blocking(move || worker.retry()).await.map_err(|error| error.to_string()).and_then(|result| result);
            RepositoryAction::EditorRecoveryResolved { card, authority, result }
        })
    }
    pub(super) fn finish_editor_recovery(&mut self, authority: crate::editor::RecoveryAuthority, result: Result<(), String>) {
        let Some(state) = &mut self.recovery else { return; };
        if state.pending.as_ref() != Some(&authority) { return; }
        state.pending = None;
        match result {
            Ok(()) if authority.is_resolved() && state.authority == authority => self.recovery = None,
            Ok(()) => state.message = "Earlier file recovery finished. Another retained operation still needs recovery.".into(),
            Err(error) => state.message = format!("File recovery is still pending; retained files were kept. {error}").chars().take(4096).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::futures::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn run_recovery(runtime: &mut RepositoryRuntime, card: CardId, authority: crate::editor::RecoveryAuthority) {
        let task = runtime.update(RepositoryAction::ResolveEditorRecovery { card, authority });
        let mut stream = iced::runtime::task::into_stream(task).expect("recovery worker admitted");
        let output = stream.next().await.expect("recovery receipt");
        let iced::runtime::Action::Output(action) = output else { panic!("recovery output"); };
        drop(runtime.update(action));
    }
    #[tokio::test]
    async fn editor_recovery_survives_save_receipt_and_blocks_disk_mutations_until_actual_retry() {
        let card = CardId::new();
        let (mut runtime, mut completion) = RepositoryRuntime::post_persist_failure_fixture(card);
        let attempts = Arc::new(AtomicUsize::new(0)); let captured = attempts.clone();
        let authority = crate::editor::RecoverySlot::reserve().unwrap().retain("exact held producer".into(), move || {
            if captured.fetch_add(1, Ordering::AcqRel) == 0 { Err("foreign target preserved".into()) } else { Ok(()) }
        });
        let RepositoryAction::SaveFinished { result, .. } = &mut completion else { unreachable!() };
        *result = Ok(SaveOutcome::Saved { content_hash: blake3::hash(b"B\n"), durability: SaveDurability::Recovery { error: "committed cleanup pending".into(), authority: authority.clone() } });
        drop(runtime.update(completion));
        assert!(runtime.has_editor_recovery()); assert!(runtime.blocks_navigation());
        assert!(!runtime.can_save()); assert!(!runtime.can_abandon_unsaved_changes());
        let before = runtime.editor_text().unwrap();
        for overwrite in [false, true] {
            drop(runtime.update(RepositoryAction::Save { card, overwrite }));
            assert!(!runtime.is_saving());
        }
        drop(runtime.update(RepositoryAction::Reload { card }));
        drop(runtime.update(RepositoryAction::KeepMine { card }));
        assert_eq!(runtime.editor_text().as_deref(), Some(before.as_str()));
        assert!(!runtime.workspace_edit_snapshot().unwrap().1.writable);
        run_recovery(&mut runtime, card, authority.clone()).await;
        assert!(runtime.has_editor_recovery()); assert!(!authority.is_resolved());
        run_recovery(&mut runtime, card, authority.clone()).await;
        assert!(!runtime.has_editor_recovery()); assert!(authority.is_resolved());
        assert_eq!(attempts.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn cross_presence_capture_owns_retirement_gate_until_real_worker_receipt() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("notes.txt"), "outside").unwrap();
        let card = CardId::new(); let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "");
        runtime.descriptor.worktree = directory.path().to_path_buf();
        let DocumentState::Editable(document) = &mut runtime.document else { unreachable!() };
        document.present = false; document.saved_present = false;
        let identity = runtime.current_identity(card, "notes.txt".into());
        let stamp = crate::editor_reload::stamp(directory.path(), Path::new("notes.txt"), Path::new("notes.txt")).unwrap().unwrap();
        runtime.disk_poll = Some((7, Instant::now()));
        let task = runtime.update(RepositoryAction::DiskMetadata { identity, ticket: 7, result: Ok(Some(stamp)) });
        assert!(runtime.has_editor_recovery()); assert!(runtime.blocks_navigation());
        assert!(runtime.recovery.is_none(), "this is an admitted worker, not an invented error");
        let mut stream = iced::runtime::task::into_stream(task).expect("real capture worker");
        let mut received = false;
        while let Some(iced::runtime::Action::Output(action)) = stream.next().await {
            if matches!(action, RepositoryAction::DiskLoaded { .. }) { received = true; }
            drop(runtime.update(action));
        }
        assert!(received); assert!(!runtime.has_editor_recovery());
        let DocumentState::Editable(document) = &runtime.document else { unreachable!() };
        assert!(document.external_version.as_ref().unwrap().restoration.is_some());
    }

    #[test]
    fn stale_disk_receipt_keeps_recovery_and_stale_retry_control_cannot_target_replacement_runtime() {
        let card = CardId::new(); let (mut runtime, completion) = RepositoryRuntime::post_persist_failure_fixture(card);
        let RepositoryAction::SaveFinished { mut identity, .. } = completion else { unreachable!() };
        identity.generation += 100;
        let authority = crate::editor::RecoverySlot::reserve().unwrap().retain("late failed read recovery".into(), || Ok(()));
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("file"), "text").unwrap();
        let state = crate::editor_reload::stamp(directory.path(), Path::new("file"), Path::new("file")).unwrap().unwrap();
        drop(runtime.update(RepositoryAction::DiskLoaded { identity, ticket: 999, stamp: state, baseline: blake3::hash(b""), result: Err(FileIoError::Recovery { message: "retained late failure".into(), authority: authority.clone() }) }));
        assert!(runtime.has_editor_recovery());
        let (mut replacement, _) = RepositoryRuntime::post_persist_failure_fixture(card);
        drop(replacement.update(RepositoryAction::ResolveEditorRecovery { card, authority: authority.clone() }));
        assert!(!authority.is_resolved()); assert!(!replacement.has_editor_recovery());
    }
}
