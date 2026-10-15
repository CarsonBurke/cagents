//! Opaque prepared changes retain their source through the ordered Git queue.

use super::*;
use crate::{git_patch, repository_widget::DiffMutationRequest};

/// A preparing action reserves its queue position before its worker starts.
/// The reducer publishes its result once; neither a later result nor a newer
/// row can replace the source held by the opaque preparation.
#[derive(Clone, Debug)]
pub(super) struct Pending {
    card: CardId,
    buffer_only: bool,
    binding_revision: Option<(crate::model::OperationId, u64)>,
    result: Arc<
        std::sync::OnceLock<Result<Option<Arc<repository_widget::PreparedDiffMutation>>, String>>,
    >,
}

impl Pending {
    fn new(card: CardId, buffer_only: bool) -> Self {
        Self {
            card,
            buffer_only,
            binding_revision: None,
            result: Arc::new(std::sync::OnceLock::new()),
        }
    }
    pub(super) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.result, &other.result)
    }
}

impl Cagents {
    fn prepared_diff_binding_current(&self, pending: &Pending) -> bool {
        pending.binding_revision.is_none_or(|(id, revision)| {
            self.refresh_ui
                .source_binding
                .as_ref()
                .is_some_and(|binding| {
                    binding.id == id
                        && binding
                            .mutation_revision
                            .load(std::sync::atomic::Ordering::Acquire)
                            == revision
                })
        })
    }

    pub(super) fn start_diff_patch(
        &mut self,
        card: CardId,
        kind: git_patch::Kind,
        request: Option<Arc<DiffMutationRequest>>,
    ) -> Task<Message> {
        if self.exit.is_some() || self.modal_is_open() {
            return Task::none();
        }
        let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card) else {
            return Task::none();
        };
        let Some(source) = self
            .sidebar_model
            .source_control
            .as_ref()
            .filter(|source| source.key.canonical_path == runtime.descriptor.worktree)
        else {
            return Task::none();
        };
        let Some(capture) = runtime.capture_diff_mutation(card, kind, request.as_deref()) else {
            return Task::none();
        };
        let key = source.key.clone();
        let mut pending = Pending::new(card, capture.applies_to_buffer());
        pending.binding_revision = self
            .refresh_ui
            .source_binding
            .as_ref()
            .filter(|binding| binding.key == key)
            .map(|binding| {
                (
                    binding.id,
                    binding
                        .mutation_revision
                        .load(std::sync::atomic::Ordering::Acquire),
                )
            });
        // A genuine native buffer Revert never writes Git or disk and remains
        // available on hosts without strong Git mutation authority.
        let reservation = if pending.buffer_only {
            Task::none()
        } else {
            self.enqueue_git_mutation(
                key.clone(),
                QueuedGitMutation::PreparedDiff(pending.clone()),
            )
        };
        let admitted = pending.buffer_only
            || self
                .source_control_mutations
                .get(&key.canonical_path)
                .is_some_and(|active| {
                    active
                        .request
                        .as_ref()
                        .is_some_and(|request| request.owns_diff_preparation(&pending))
                        || active
                            .pending
                            .iter()
                            .any(|request| request.owns_diff_preparation(&pending))
                });
        if !admitted {
            return reservation;
        }
        let prepare = cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || capture.prepare())
                .await
                .map_err(|error| format!("Could not prepare the selected changes: {error}"));
            Message::DiffPatchPrepared {
                key,
                pending,
                result,
            }
        });
        Task::batch([reservation, prepare])
    }

    pub(super) fn finish_diff_patch(
        &mut self,
        key: ActiveWorktreeKey,
        pending: Pending,
        result: Result<Option<repository_widget::PreparedDiffMutation>, String>,
    ) -> Task<Message> {
        // Completion must fill/settle its original reservation even when a
        // later recovery or shutdown forbids the prepared effect.
        let result = match self.git_effect_block_reason(&key.canonical_path) {
            Some(reason) => Err(reason.into()),
            None => result,
        };
        if pending.buffer_only {
            if pending
                .result
                .set(result.map(|prepared| prepared.map(Arc::new)))
                .is_err()
            {
                return Task::none();
            }
            return self.apply_prepared_buffer_diff(pending);
        }
        let Some(active) = self.source_control_mutations.get(&key.canonical_path) else {
            return Task::none();
        };
        let current = active
            .request
            .as_ref()
            .is_some_and(|request| request.owns_diff_preparation(&pending));
        if !current
            && !active
                .pending
                .iter()
                .any(|request| request.owns_diff_preparation(&pending))
        {
            return Task::none();
        }
        if pending
            .result
            .set(result.map(|prepared| prepared.map(Arc::new)))
            .is_err()
        {
            return Task::none();
        }
        let operation = active.id;
        if current {
            self.queued_git_task(key, operation, QueuedGitMutation::PreparedDiff(pending))
        } else {
            Task::none()
        }
    }

    fn apply_prepared_buffer_diff(&mut self, pending: Pending) -> Task<Message> {
        let Some(result) = pending.result.get() else {
            return Task::none();
        };
        let prepared = match result {
            Ok(Some(prepared)) if prepared.applies_to_buffer() => prepared,
            other => {
                self.status = Some(TransientNotice::warning(match other {
                    Err(error) => error.clone(),
                    _ => "The original buffer change is no longer available.".into(),
                }));
                return Task::none();
            }
        };
        if self.modal_is_open() || !self.prepared_diff_binding_current(&pending) {
            return Task::none();
        }
        let Some(CardRuntime::Repository(runtime)) = self.cards.get(&pending.card) else {
            return Task::none();
        };
        if let Some(reason) = self.git_effect_block_reason(&runtime.descriptor.worktree) {
            self.status = Some(TransientNotice::warning(reason));
            return Task::none();
        }
        let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&pending.card) else {
            return Task::none();
        };
        match prepared.as_ref().clone().apply(runtime) {
            Ok(None) => self.drain_lsp_intents(pending.card),
            Ok(Some(_)) => {
                self.status = Some(TransientNotice::warning(
                    "A buffer-only action cannot change Git state.",
                ));
                Task::none()
            }
            Err(error) => {
                self.status = Some(TransientNotice::warning(error));
                Task::none()
            }
        }
    }

    pub(super) fn apply_prepared_diff_at_turn(
        &mut self,
        key: ActiveWorktreeKey,
        operation: uuid::Uuid,
        pending: Pending,
    ) -> Task<Message> {
        let Some(result) = pending.result.get() else {
            return Task::none();
        };
        let complete = |report| Message::SourceControlMutated {
            key: key.clone(),
            operation,
            report,
            committed: false,
        };
        let prepared = match result {
            Ok(Some(prepared)) if !prepared.applies_to_buffer() => prepared,
            Ok(Some(_)) => {
                return self.update(complete(Some(
                    "The prepared action changed its effect type.".into(),
                )));
            }
            Ok(None) => {
                return self.update(complete(Some(
                    "The selected range contains no applicable changed lines.".into(),
                )));
            }
            Err(error) => return self.update(complete(Some(error.clone()))),
        };
        if let Some(reason) = self.git_effect_block_reason(&key.canonical_path) {
            return self.update(complete(Some(reason.into())));
        }
        if self.modal_is_open() || !self.prepared_diff_binding_current(&pending) {
            return self.update(complete(Some(
                "The original Git source changed. Retry the selected change.".into(),
            )));
        }
        let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&pending.card) else {
            return self.update(complete(Some(
                "The original diff is no longer available.".into(),
            )));
        };
        // This opaque check includes the model Arc, document/buffer revision,
        // annotations, basis, mode, layout and exact selection at this turn.
        match prepared.as_ref().clone().apply(runtime) {
            Ok(Some(effect)) => {
                self.queued_git_task(key, operation, QueuedGitMutation::Operation(effect))
            }
            Ok(None) => {
                let released = self.update(complete(None));
                Task::batch([released, self.drain_lsp_intents(pending.card)])
            }
            Err(error) => self.update(complete(Some(error))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Cagents, CardId, ActiveWorktreeKey) {
        let temporary = tempfile::tempdir().unwrap();
        super::super::git_binding::tests::init(temporary.path(), "changed\nkeep\n");
        let (mut app, key) = super::super::git_binding::tests::app_for(temporary.path());
        let card = CardId::new();
        let runtime =
            repository_widget::RepositoryRuntime::selected_diff_fixture_at(card, temporary.path());
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        (temporary, app, card, key)
    }

    #[test]
    fn delayed_preparation_reserves_turn_and_refuses_changed_diff_before_git() {
        if !crate::repository::WorktreeAuthority::supported() {
            return;
        }
        let (_temporary, mut app, card, key) = fixture();
        let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
            panic!()
        };
        let prepared = runtime
            .capture_diff_mutation(card, git_patch::Kind::Stage, None)
            .unwrap()
            .prepare()
            .unwrap();
        // Admission of the actual app action reserves before its worker runs.
        // Hold that task, then change the live rendered coordinates.
        let held = app.start_diff_patch(card, git_patch::Kind::Stage, None);
        let active = app
            .source_control_mutations
            .get(&key.canonical_path)
            .unwrap();
        let Some(QueuedGitMutation::PreparedDiff(pending)) = active.request.as_ref() else {
            panic!("preparation must own the turn")
        };
        let pending = pending.clone();
        drop(app.enqueue_git_mutation(key.clone(), QueuedGitMutation::Unstage(Vec::new())));
        assert_eq!(
            app.source_control_mutations[&key.canonical_path]
                .pending
                .len(),
            1
        );
        let CardRuntime::Repository(runtime) = app.cards.get_mut(&card).unwrap() else {
            panic!()
        };
        drop(runtime.update(RepositoryAction::ApplyDiffMode { card, split: true }));
        // Do not execute the queued successor: this test proves preparation
        // refuses before it can lower to any Git operation.
        app.source_control_mutations
            .get_mut(&key.canonical_path)
            .unwrap()
            .pending
            .clear();
        drop(app.finish_diff_patch(key.clone(), pending.clone(), Ok(Some(prepared.clone()))));
        assert!(
            !app.source_control_mutations
                .contains_key(&key.canonical_path)
        );
        assert!(
            app.notices
                .recent()
                .any(|entry| entry.notice.message.contains("changed"))
                || app
                    .status
                    .as_ref()
                    .is_some_and(|notice| notice.message.contains("changed"))
        );
        // A replay cannot reclaim a completed slot or submit a new operation.
        drop(app.finish_diff_patch(key.clone(), pending, Ok(Some(prepared))));
        assert!(
            !app.source_control_mutations
                .contains_key(&key.canonical_path)
        );
        drop(held);
    }

    #[test]
    fn unchanged_buffer_preparation_applies_once_and_keeps_one_undo() {
        let (_temporary, mut app, card, key) = fixture();
        let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
            panic!()
        };
        let prepared = runtime
            .capture_diff_mutation(card, git_patch::Kind::Revert, None)
            .unwrap()
            .prepare()
            .unwrap();
        assert!(prepared.applies_to_buffer());
        let pending = Pending::new(card, true);
        drop(app.finish_diff_patch(key.clone(), pending.clone(), Ok(Some(prepared.clone()))));
        let CardRuntime::Repository(runtime) = app.cards.get_mut(&card).unwrap() else {
            panic!()
        };
        assert_eq!(runtime.editor_text().unwrap(), "original\nkeep\n");
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(runtime.editor_text().unwrap(), "changed\nkeep\n");
        drop(app.finish_diff_patch(key, pending, Ok(Some(prepared))));
        let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
            panic!()
        };
        assert_eq!(runtime.editor_text().unwrap(), "changed\nkeep\n");
    }
    async fn worker_message(task: Task<Message>, accepts: impl Fn(&Message) -> bool) -> Message {
        use cosmic::iced::futures::StreamExt;
        let mut stream = iced::runtime::task::into_stream(task).expect("actual worker task");
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                if accepts(&message) {
                    return message;
                }
            }
        }
        panic!("expected worker receipt");
    }

    fn git_text(root: &Path, arguments: &[&str]) -> String {
        let output = super::super::git_binding::tests::git(root, arguments);
        assert!(output.status.success(), "{arguments:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    }

    fn committed_base(root: &Path) {
        std::fs::write(root.join("file.txt"), "original\nkeep\n").unwrap();
        git_text(root, &["add", "file.txt"]);
        git_text(root, &["commit", "-m", "base"]);
        std::fs::write(root.join("file.txt"), "changed\nkeep\n").unwrap();
    }

    fn retain_recovery(app: &mut Cagents, root: &Path) -> crate::editor::RecoveryAuthority {
        let authority = crate::editor::RecoverySlot::reserve().unwrap()
            .retain("held publication cleanup".into(), || Ok(()));
        drop(app.update(Message::EditorFeatures(editor_features::Action::Published {
            ticket: u64::MAX,
            result: Err(crate::editor_workspace_edit::PublicationFailure {
                message: "publication cleanup remains pending".into(), recovery: Some(authority.clone()),
                worktree: Some(root.to_owned()),
            }),
        })));
        assert!(app.publication_recovery_blocks(root));
        authority
    }

    #[tokio::test]
    async fn actual_preparation_receipt_retires_its_reservation_after_recovery_or_exit() {
        if !crate::repository::WorktreeAuthority::supported() { return; }
        for shutdown in [false, true] {
            let (temporary, mut app, card, key) = fixture();
            let root = temporary.path();
            committed_base(root);
            let receipt = worker_message(app.start_diff_patch(card, git_patch::Kind::Stage, None),
                |message| matches!(message, Message::DiffPatchPrepared { .. })).await;
            let Message::DiffPatchPrepared { pending, result: Ok(Some(_)), .. } = &receipt else {
                panic!("the actual preparation worker must complete before the new refusal");
            };
            let pending = pending.clone();
            std::fs::write(root.join("queued.txt"), "must remain untracked\n").unwrap();
            drop(app.enqueue_git_mutation(key.clone(), QueuedGitMutation::Stage(vec![
                crate::repository::PathSelection::new(RepoPath::parse("queued.txt").unwrap())])));
            assert_eq!(app.source_control_mutations[&key.canonical_path].pending.len(), 1);
            let _recovery = if shutdown {
                drop(app.exit_after_language_servers());
                assert!(app.exit.is_some());
                None
            } else { Some(retain_recovery(&mut app, root)) };
            drop(app.update(receipt));
            assert!(matches!(pending.result.get(), Some(Err(_))), "the exact owned slot records refusal");
            assert!(app.source_control_mutations.is_empty(), "completion must release the reserved turn");
            assert_eq!(git_text(root, &["show", ":file.txt"]), "original\nkeep\n");
            assert!(git_text(root, &["diff", "--cached", "--name-only"]).trim().is_empty());
            assert_eq!(std::fs::read_to_string(root.join("queued.txt")).unwrap(), "must remain untracked\n");
            assert!(git_text(root, &["ls-files", "--", "queued.txt"]).is_empty());
            // A duplicate receipt cannot recreate or fill a newer reservation.
            drop(app.finish_diff_patch(key, pending, Ok(None)));
            assert!(app.source_control_mutations.is_empty());
        }
    }

    #[tokio::test]
    async fn returned_git_effect_task_observes_recovery_before_its_first_worker_effect() {
        if !crate::repository::WorktreeAuthority::supported() { return; }
        let (temporary, mut app, card, key) = fixture();
        let root = temporary.path();
        committed_base(root);
        let prepared = worker_message(app.start_diff_patch(card, git_patch::Kind::Stage, None),
            |message| matches!(message, Message::DiffPatchPrepared { .. })).await;
        let ready_effect = app.update(prepared);
        let original_token = app.source_control_mutations[&key.canonical_path].cancellation.clone();
        assert!(!original_token.is_cancelled());
        let _recovery = retain_recovery(&mut app, root);
        assert!(original_token.is_cancelled(), "the returned task retains the same scoped cancellation owner");
        let refused = worker_message(ready_effect, |message| matches!(message, Message::SourceControlMutated { .. })).await;
        assert!(matches!(&refused, Message::SourceControlMutated { report: Some(reason), committed: false, .. }
            if reason.contains("cancelled before its worker started")));
        drop(app.update(refused));
        assert!(app.source_control_mutations.is_empty());
        assert_eq!(git_text(root, &["show", ":file.txt"]), "original\nkeep\n");
        assert!(git_text(root, &["diff", "--cached", "--name-only"]).is_empty());
    }

    #[tokio::test]
    async fn ready_successor_cannot_start_after_recovery_overtakes_an_earlier_actual_turn() {
        if !crate::repository::WorktreeAuthority::supported() { return; }
        let (temporary, mut app, card, key) = fixture();
        let root = temporary.path();
        committed_base(root);
        std::fs::write(root.join("earlier.txt"), "completed earlier turn\n").unwrap();
        let first = app.enqueue_git_mutation(key.clone(), QueuedGitMutation::Stage(vec![
            crate::repository::PathSelection::new(RepoPath::parse("earlier.txt").unwrap())]));
        let prepared = worker_message(app.start_diff_patch(card, git_patch::Kind::Stage, None),
            |message| matches!(message, Message::DiffPatchPrepared { .. })).await;
        drop(app.update(prepared));
        assert_eq!(app.source_control_mutations[&key.canonical_path].pending.len(), 1);
        let completed = worker_message(first, |message| matches!(message, Message::SourceControlMutated { .. })).await;
        assert!(matches!(&completed, Message::SourceControlMutated { report: None, committed: false, .. }),
            "the earlier worker's successful facts remain true");
        let _recovery = retain_recovery(&mut app, root);
        drop(app.update(completed));
        assert!(app.source_control_mutations.is_empty());
        assert_eq!(git_text(root, &["diff", "--cached", "--name-only"]).trim(), "earlier.txt");
        assert_eq!(git_text(root, &["show", ":file.txt"]), "original\nkeep\n");
        let refused = app.enqueue_git_mutation(key, QueuedGitMutation::Unstage(Vec::new()));
        assert!(iced::runtime::task::into_stream(refused).is_none(), "new admission is also refused");
        assert!(app.source_control_mutations.is_empty());
    }

    #[tokio::test]
    async fn delayed_native_buffer_preparation_records_refusal_without_editing_after_exit() {
        let (_temporary, mut app, card, _key) = fixture();
        let receipt = worker_message(app.start_diff_patch(card, git_patch::Kind::Revert, None),
            |message| matches!(message, Message::DiffPatchPrepared { .. })).await;
        let Message::DiffPatchPrepared { pending, result: Ok(Some(prepared)), .. } = &receipt else { panic!() };
        assert!(prepared.applies_to_buffer());
        let pending = pending.clone();
        assert!(app.source_control_mutations.is_empty(), "native Undo reserves no Git turn");
        drop(app.exit_after_language_servers());
        assert!(app.exit.is_some());
        drop(app.update(receipt));
        assert!(matches!(pending.result.get(), Some(Err(_))));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else { panic!() };
        assert_eq!(runtime.editor_text().unwrap(), "changed\nkeep\n");
        assert!(app.source_control_mutations.is_empty());
    }

    #[tokio::test]
    async fn actual_preparation_worker_and_owned_turn_stage_only_the_current_captured_patch() {
        if !crate::repository::WorktreeAuthority::supported() {
            return;
        }
        for earlier_turn in [false, true] {
            let (temporary, mut app, card, key) = fixture();
            let root = temporary.path();
            let git = |arguments: &[&str]| {
                let output = super::super::git_binding::tests::git(root, arguments);
                assert!(
                    output.status.success(),
                    "{arguments:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                output.stdout
            };
            std::fs::write(root.join("file.txt"), "original\nkeep\n").unwrap();
            git(&["add", "file.txt"]);
            git(&["commit", "-m", "base"]);
            std::fs::write(root.join("file.txt"), "changed\nkeep\n").unwrap();
            let first = if earlier_turn {
                std::fs::write(root.join("other.txt"), "earlier turn\n").unwrap();
                Some(app.enqueue_git_mutation(
                    key.clone(),
                    QueuedGitMutation::Stage(vec![crate::repository::PathSelection::new(
                        RepoPath::parse("other.txt").unwrap(),
                    )]),
                ))
            } else {
                None
            };
            let prepare = app.start_diff_patch(card, git_patch::Kind::Stage, None);
            let receipt = worker_message(prepare, |message| {
                matches!(message, Message::DiffPatchPrepared { .. })
            })
            .await;
            let effect = app.update(receipt);
            if let Some(first) = first {
                drop(effect); // Preparation filled its pending slot only.
                let completed = worker_message(first, |message| {
                    matches!(message, Message::SourceControlMutated { .. })
                })
                .await;
                assert!(matches!(
                    &completed,
                    Message::SourceControlMutated { report: None, .. }
                ));
                drop(app.update(completed));
                assert!(
                    app.source_control_mutations.is_empty(),
                    "stale prepared successor is refused at its owned turn"
                );
                assert_eq!(
                    String::from_utf8(git(&["diff", "--cached", "--name-only"]))
                        .unwrap()
                        .trim(),
                    "other.txt"
                );
                assert!(
                    app.notices
                        .recent()
                        .any(|entry| entry.notice.message.contains("original Git source changed"))
                );
            } else {
                let completed = worker_message(effect, |message| {
                    matches!(message, Message::SourceControlMutated { .. })
                })
                .await;
                assert!(matches!(
                    &completed,
                    Message::SourceControlMutated { report: None, .. }
                ));
                drop(app.update(completed));
                assert_eq!(
                    String::from_utf8(git(&["show", ":file.txt"])).unwrap(),
                    "changed\nkeep\n"
                );
                assert_eq!(
                    String::from_utf8(git(&["diff", "--cached", "--name-only"]))
                        .unwrap()
                        .trim(),
                    "file.txt"
                );
            }
        }
    }
}
