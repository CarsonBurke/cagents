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
    read_receipt: Option<crate::repository::GitReadReceipt>,
    read_key: Option<ActiveWorktreeKey>,
    result: Arc<
        std::sync::OnceLock<Result<Option<Arc<repository_widget::PreparedDiffMutation>>, String>>,
    >,
}

impl Pending {
    fn new(card: CardId, buffer_only: bool) -> Self {
        Self {
            card,
            buffer_only,
            read_receipt: None,
            read_key: None,
            result: Arc::new(std::sync::OnceLock::new()),
        }
    }
    pub(super) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.result, &other.result)
    }
}

impl Cagents {
    fn prepared_diff_binding_current(&self, pending: &Pending) -> bool {
        pending.buffer_only || pending.read_receipt.as_ref().is_some_and(|receipt| {
            self.refresh_ui.source_binding.as_ref().is_some_and(|binding| {
                pending.read_key.as_ref() == Some(&binding.key)
                    && self.sidebar_model.contains_worktree(&binding.key)
                    && self.sidebar_model.source_control.as_ref().is_some_and(|model|
                        model.key == binding.key && model.command_binding_id() == binding.id)
                    && binding.authority.as_ref().is_some_and(|authority|
                        authority.same_scope(receipt.source().authority()))
                    && receipt.source().id() == binding.id
                    && binding.read_source.as_ref().is_some_and(|source| receipt.matches_source(source))
            })
        })
    }

    pub(super) fn start_diff_patch(
        &mut self,
        card: CardId,
        kind: git_patch::Kind,
        request: Option<Arc<DiffMutationRequest>>,
    ) -> Task<Message> {
        if self.modal_is_open() {
            return Task::none();
        }
        let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card) else {
            return Task::none();
        };
        let Some(capture) = runtime.capture_diff_mutation(card, kind, request.as_deref()) else {
            return Task::none();
        };
        let buffer_only = capture.applies_to_buffer();
        // Native Undo belongs to the mounted document, independently of the
        // Git strip's current worktree. The row supplies routing, not authority.
        let Some(key) = self.sidebar_model.project(runtime.descriptor.project)
            .and_then(|project| project.worktrees.iter()
                .find(|worktree| worktree.path == runtime.descriptor.worktree)
                .map(|worktree| ActiveWorktreeKey {
                    project_id: project.id,
                    worktree_id: worktree.id,
                    canonical_path: worktree.path.clone(),
                }))
        else { return Task::none(); };
        let mut pending = Pending::new(card, buffer_only);
        if !buffer_only {
            if !self.runtime_git_source_is_current(card, runtime) { return Task::none(); }
            let Some(receipt) = capture.git_read_receipt().filter(|receipt| {
                self.refresh_ui.source_binding.as_ref()
                    .filter(|binding| binding.key == key)
                    .and_then(|binding| binding.read_source.as_ref())
                    .is_some_and(|source| receipt.matches_source(source))
            }) else { return Task::none(); };
            pending.read_receipt = Some(receipt.clone());
            pending.read_key = Some(key.clone());
        }
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
        if self.modal_is_open()
            || !self.prepared_diff_binding_current(&pending)
            || prepared.git_read_receipt() != pending.read_receipt.as_ref()
        {
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
        super::super::git_binding::tests::init(temporary.path(), "original\nkeep\n");
        std::fs::write(temporary.path().join("file.txt"), "changed\nkeep\n").unwrap();
        let (mut app, key) = super::super::git_binding::tests::app_for(temporary.path());
        let card = CardId::new();
        #[cfg(target_os = "linux")]
        let runtime = repository_widget::RepositoryRuntime::selected_diff_fixture_from_source(
            card,
            RepositoryView { project: key.project_id, repository: temporary.path().to_owned(),
                worktree: temporary.path().to_owned(), selected_file: Some("file.txt".into()) },
            app.refresh_ui.source_binding.as_ref().unwrap().read_source.as_ref().unwrap().clone(),
        );
        #[cfg(not(target_os = "linux"))]
        let runtime = repository_widget::RepositoryRuntime::selected_diff_fixture_at(card, temporary.path());
        app.refresh_ui.pane_reader_keys.insert(card, key.clone());
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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn held_original_reader_cannot_prepare_or_stage_through_a_same_path_replacement() {
        use super::super::git_binding::tests::{git, init};
        for metadata_only in [false, true] {
            let (temporary, mut app, card, key) = fixture();
            let root = temporary.path();
            let original_source = app.refresh_ui.source_binding.as_ref().unwrap().read_source.as_ref().unwrap().clone();
            let task = app.start_diff_patch(card, git_patch::Kind::Stage, None);
            let held = worker_message(task, |message| matches!(message, Message::DiffPatchPrepared { .. })).await;
            let parked = tempfile::tempdir().unwrap();
            let original = parked.path().join("original");
            if metadata_only {
                std::fs::rename(root.join(".git"), &original).unwrap();
                init(root, "original\nkeep\n");
            } else {
                std::fs::rename(root, &original).unwrap();
                init(root, "original\nkeep\n");
            }
            std::fs::write(root.join("file.txt"), "changed\nkeep\n").unwrap();
            let mut replacement = git_binding::Binding::new(key.clone());
            replacement.initialize().unwrap();
            let new_source = replacement.read_source.as_ref().unwrap().clone();
            let snapshot = replacement.mutation_service().unwrap().snapshot(root, &CancellationToken::default()).unwrap();
            let mut current = SourceControlModel::ready_for_tab(key.clone(), &snapshot, false);
            current.set_command_binding_id(replacement.id);
            app.sidebar_model.source_control = Some(current);
            app.refresh_ui.source_binding = Some(replacement);
            assert!(!original_source.same_source(&new_source));
            // The exact old CPU receipt settles its own reservation, and never
            // lowers A's buffered patch into the currently bound B service.
            drop(app.update(held));
            assert!(app.source_control_mutations.is_empty());
            drop(app.start_diff_patch(card, git_patch::Kind::Stage, None));
            drop(app.update(Message::Repository(RepositoryAction::StageSelected { card, stage: true })));
            drop(app.update(Message::Repository(RepositoryAction::RevertFile { card })));
            assert!(app.source_control_mutations.is_empty());
            assert!(app.sidebar_state.pending_discard().is_none());
            assert_eq!(git(root, &["show", ":file.txt"]).stdout, b"original\nkeep\n");
            // A genuine new B read, with the same text/index, is independently
            // actionable. It is not an old A model with a new label.
            let descriptor = RepositoryView { project: key.project_id, repository: root.to_owned(),
                worktree: root.to_owned(), selected_file: Some("file.txt".into()) };
            let fresh = RepositoryRuntime::selected_diff_fixture_from_source(card, descriptor, new_source);
            app.cards.insert(card, CardRuntime::Repository(Box::new(fresh)));
            app.refresh_ui.pane_reader_keys.insert(card, key.clone());
            let prepare = app.start_diff_patch(card, git_patch::Kind::Stage, None);
            let receipt = worker_message(prepare, |message| matches!(message, Message::DiffPatchPrepared { .. })).await;
            let effect = app.update(receipt);
            let completed = worker_message(effect, |message| matches!(message, Message::SourceControlMutated { .. })).await;
            assert!(matches!(&completed, Message::SourceControlMutated { report: None, .. }));
            drop(app.update(completed));
            assert_eq!(git(root, &["show", ":file.txt"]).stdout, b"changed\nkeep\n");
            let old_git = if metadata_only { original } else { original.join(".git") };
            let old_index = std::process::Command::new("git").arg("--git-dir").arg(old_git)
                .args(["show", ":file.txt"]).output().unwrap();
            assert!(old_index.status.success());
            assert_eq!(old_index.stdout, b"original\nkeep\n");
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn returning_to_a_mounted_reader_reuses_its_original_actor_and_actual_worker() {
        let (temporary, mut app, card, key) = fixture();
        let original = app.refresh_ui.source_binding.as_ref().unwrap().read_source.as_ref().unwrap().clone();
        drop(app.release_source_control());
        app.sidebar_model.source_control = Some(SourceControlModel::loading(key.clone()));
        let reload = app.reload_source_control(&key);
        let binding = app.refresh_ui.source_binding.as_ref().unwrap();
        assert_eq!(binding.id, original.id());
        assert!(binding.read_source.as_ref().unwrap().same_source(&original));
        let loaded = worker_message(reload, |message| matches!(message, Message::SourceControlLoaded { .. })).await;
        drop(app.update(loaded));
        let prepare = app.start_diff_patch(card, git_patch::Kind::Stage, None);
        let receipt = worker_message(prepare, |message| matches!(message, Message::DiffPatchPrepared { .. })).await;
        let effect = app.update(receipt);
        let completed = worker_message(effect, |message| matches!(message, Message::SourceControlMutated { .. })).await;
        assert!(matches!(&completed, Message::SourceControlMutated { report: None, .. }));
        drop(app.update(completed));
        assert_eq!(git_binding::tests::git(temporary.path(), &["show", ":file.txt"]).stdout, b"changed\nkeep\n");
        assert_eq!(original.mutation_revision().load(std::sync::atomic::Ordering::Acquire), 1);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn buffer_revert_uses_its_native_document_while_the_git_strip_is_elsewhere() {
        let (_temporary, mut app, card, key) = fixture();
        let original = app.refresh_ui.source_binding.as_ref().unwrap().read_source.as_ref().unwrap().clone();
        let other = tempfile::tempdir().unwrap();
        git_binding::tests::init(other.path(), "other\n");
        let mut sibling = key.clone();
        sibling.worktree_id = crate::model::WorktreeId::new();
        sibling.canonical_path = other.path().to_owned();
        let mut binding = git_binding::Binding::new(sibling.clone());
        binding.initialize().unwrap();
        app.refresh_ui.source_binding = Some(binding);
        app.sidebar_model.source_control = Some(SourceControlModel::loading(sibling));
        let prepare = app.start_diff_patch(card, git_patch::Kind::Revert, None);
        let receipt = worker_message(prepare, |message| matches!(message, Message::DiffPatchPrepared { .. })).await;
        drop(app.update(receipt));
        assert!(app.source_control_mutations.is_empty());
        let CardRuntime::Repository(runtime) = app.cards.get_mut(&card).unwrap() else { panic!() };
        assert_eq!(runtime.editor_text().as_deref(), Some("original\nkeep\n"));
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(runtime.editor_text().as_deref(), Some("changed\nkeep\n"));
        assert!(runtime.git_read_source().unwrap().same_source(&original));
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
