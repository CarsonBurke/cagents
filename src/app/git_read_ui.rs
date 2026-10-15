//! Native repository actions use the source that produced their displayed bytes.
use super::*;
use crate::repository::GitReadSource;

#[derive(Clone, Debug)]
pub(super) struct PendingReader {
    request: OperationId,
    view: RepositoryView,
    binding: git_binding::Binding,
    open: Option<InitialOpen>,
}

#[derive(Clone, Debug)]
enum InitialOpen {
    Action(Box<RepositoryAction>),
    SourceLocation { path: PathBuf, line: usize },
}

impl Cagents {
    /// Closing one pane drops only its local request/origin metadata. Another
    /// pane or the strip can still own the same original reader.
    pub(super) fn forget_repository_reader(&mut self, card: CardId) {
        self.refresh_ui.pane_readers.remove(&card);
        self.refresh_ui.pane_reader_keys.remove(&card);
    }

    /// Only original read/navigation intent can wait for initialization.
    /// Git commands, captured controls and arbitrary editor edits never do.
    pub(super) fn defer_initial_repository_open(&mut self, action: &RepositoryAction) -> bool {
        if !matches!(action, RepositoryAction::OpenWorkingDiff { .. }
            | RepositoryAction::OpenCommittedFile { .. }
            | RepositoryAction::OpenFileFromBrowser { .. }
            | RepositoryAction::OpenLocation { .. }) { return false; }
        let Some(pending) = self.refresh_ui.pane_readers.get_mut(&action.card()) else { return false; };
        pending.open = Some(InitialOpen::Action(Box::new(action.clone())));
        true
    }

    pub(super) fn defer_initial_source_location(&mut self, card: CardId, path: PathBuf, line: usize) {
        if let Some(pending) = self.refresh_ui.pane_readers.get_mut(&card) {
            pending.open = Some(InitialOpen::SourceLocation { path, line });
        }
    }

    pub(super) fn repository_view_key(&self, view: &RepositoryView) -> Option<ActiveWorktreeKey> {
        let project = self.sidebar_model.project(view.project)?;
        let worktree = project.worktrees.iter().find(|worktree| worktree.path == view.worktree)?;
        Some(ActiveWorktreeKey { project_id: project.id, worktree_id: worktree.id, canonical_path: worktree.path.clone() })
    }

    pub(super) fn pending_runtime_git_binding(&self, key: &ActiveWorktreeKey) -> Option<git_binding::Binding> {
        self.refresh_ui.pane_readers.values().find(|pending| pending.binding.key == *key)
            .map(|pending| pending.binding.clone())
    }

    /// An explicit retry can replace an actor which never captured an
    /// authority or produced any displayed bytes. A qualified reader is never
    /// rebound this way, including after a physical validation failure.
    pub(super) fn retry_failed_initial_git_reader(&mut self, key: &ActiveWorktreeKey) {
        let Some(old) = self.refresh_ui.source_binding.as_ref()
            .filter(|binding| binding.key == *key && binding.initial_capture_failed()) else { return; };
        old.invalidate();
        self.cancel_source_read();
        let binding = git_binding::Binding::new(key.clone());
        if let Some(model) = self.sidebar_model.source_control.as_mut().filter(|model| model.key == *key) {
            model.set_command_binding_id(binding.id);
        }
        self.refresh_ui.source_binding = Some(binding);
    }

    /// Initialize the original scope off thread before constructing a runtime
    /// or issuing its first read. Concurrent panel/pane reads share one capture.
    pub(super) fn start_repository_reader(&mut self, card: CardId, view: RepositoryView) -> Task<Message> {
        let Some(key) = self.repository_view_key(&view) else { return Task::none(); };
        self.retry_failed_initial_git_reader(&key);
        let binding = self.refresh_ui.source_binding.as_ref().filter(|binding| binding.key == key).cloned()
            .or_else(|| self.retained_runtime_git_binding(&key))
            .or_else(|| self.pending_runtime_git_binding(&key))
            .unwrap_or_else(|| git_binding::Binding::new(key));
        let request = OperationId::new();
        self.refresh_ui.pane_readers.insert(card, PendingReader { request, view, binding: binding.clone(), open: None });
        self.refresh_ui.pane_reader_keys.remove(&card);
        self.cards.insert(card, CardRuntime::Loading);
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let mut binding = binding;
                binding.initialize()?;
                binding.validate()?;
                Ok(binding)
            }).await.map_err(|error| format!("Git reader initialization failed: {error}"))
                .and_then(|result| result);
            Message::RepositoryReaderReady { card, request, result }
        })
    }

    pub(super) fn finish_repository_reader(&mut self, card: CardId, request: OperationId,
        result: Result<git_binding::Binding, String>) -> Task<Message> {
        let Some(pending) = self.refresh_ui.pane_readers.get(&card).filter(|pending| pending.request == request) else {
            return Task::none();
        };
        let view = pending.view.clone();
        let original = pending.binding.id;
        let open = pending.open.clone();
        self.refresh_ui.pane_readers.remove(&card);
        if self.exit.is_some()
            || !matches!(self.cards.get(&card), Some(CardRuntime::Loading))
            || !self.card_state(card).is_some_and(|state| state.existence.is_open()
                && matches!(&state.kind, CardKind::Repository(current) if *current == view))
            || self.repository_view_key(&view).is_none()
        { return Task::none(); }
        if self.update_prepared {
            // The read completed behind an update freeze. Settle visibly so
            // cancelling the update leaves the real pane Retry route, rather
            // than a Loading card whose exact request has already been spent.
            self.fail_runtime(card,
                "Opening this file was paused for the update. Retry this pane after cancelling the update.".into());
            return Task::none();
        }
        match result {
            Ok(binding) if binding.id == original && binding.key.project_id == view.project
                && self.repository_view_key(&view).as_ref() == Some(&binding.key)
                && binding.read_source.as_ref().is_some_and(GitReadSource::is_available) => {
                let load = self.attach_repository_pane_with_source(card, view, binding.read_source);
                let navigate = match (self.cards.get_mut(&card), open) {
                    (Some(CardRuntime::Repository(runtime)), Some(InitialOpen::Action(action))) =>
                        repository_task(runtime.update(*action)),
                    (Some(CardRuntime::Repository(runtime)), Some(InitialOpen::SourceLocation { path, line })) =>
                        repository_task(runtime.open_source_file_location(card, path, line)),
                    _ => Task::none(),
                };
                Task::batch([load, navigate, self.drain_lsp_intents(card)])
            }
            Ok(_) => {
                self.cards.insert(card, CardRuntime::Failed("The original Git reader changed. Reload the project and reopen this pane.".into()));
                Task::none()
            }
            Err(error) => {
                self.cards.insert(card, CardRuntime::Failed(error.into()));
                Task::none()
            }
        }
    }

    pub(super) fn original_git_source_for_view(&self, view: &RepositoryView) -> Option<GitReadSource> {
        let key = self.repository_view_key(view)?;
        self.refresh_ui.source_binding.as_ref()
            .filter(|binding| binding.key == key)
            .and_then(|binding| binding.read_source.as_ref())
            .filter(|source| source.is_available() && source.authority().path() == view.worktree)
            .cloned()
            .or_else(|| self.cards.iter().find_map(|(card, runtime)| {
                let CardRuntime::Repository(runtime) = runtime else { return None; };
                (self.refresh_ui.pane_reader_keys.get(card) == Some(&key)
                    && runtime.descriptor.project == view.project && runtime.descriptor.worktree == view.worktree)
                    .then(|| runtime.git_read_source()).flatten()
                    .filter(|source| source.is_available() && source.authority().path() == view.worktree).cloned()
            }))
    }

    /// Only a still-mounted original reader or active queue keeps this actor
    /// alive off strip. There is no permanent per-path authority cache.
    pub(super) fn retained_runtime_git_binding(&self, key: &ActiveWorktreeKey) -> Option<git_binding::Binding> {
        self.cards.iter().find_map(|(card, runtime)| {
            let CardRuntime::Repository(runtime) = runtime else { return None; };
            if self.refresh_ui.pane_reader_keys.get(card) != Some(key)
                || runtime.descriptor.project != key.project_id || runtime.descriptor.worktree != key.canonical_path { return None; }
            git_binding::Binding::from_read_source(key.clone(), runtime.git_read_source()?.clone())
        })
    }

    pub(super) fn runtime_git_source_is_current(&self, card: CardId, runtime: &RepositoryRuntime) -> bool {
        let Some(source) = runtime.git_read_source() else { return false; };
        self.refresh_ui.source_binding.as_ref().is_some_and(|binding|
            self.refresh_ui.pane_reader_keys.get(&card) == Some(&binding.key)
                && self.sidebar_model.contains_worktree(&binding.key)
                && binding.key.project_id == runtime.descriptor.project
                && binding.key.canonical_path == runtime.descriptor.worktree
                && self.sidebar_model.source_control.as_ref().is_some_and(|model|
                    model.key == binding.key && model.command_binding_id() == source.id())
                && source.is_available()
                && binding.authority.as_ref().is_some_and(|authority| authority.same_scope(source.authority()))
                && source.authority().path() == runtime.descriptor.worktree
                && binding.read_source.as_ref().is_some_and(|current| source.same_source(current)))
    }

    /// A project ownership change permanently revokes its old native readers.
    /// Ordinary verified refresh/navigation does not call this retirement.
    pub(super) fn retire_project_git_readers(&mut self, project: ProjectId) {
        self.refresh_ui.pane_readers.retain(|_, pending| {
            if pending.view.project == project { pending.binding.invalidate(); false } else { true }
        });
        self.refresh_ui.pane_reader_keys.retain(|_, key| key.project_id != project);
        for runtime in self.cards.values() {
            if let CardRuntime::Repository(runtime) = runtime
                && runtime.descriptor.project == project
                && let Some(source) = runtime.git_read_source()
            { source.invalidate(); }
        }
        if let Some(binding) = self.refresh_ui.source_binding.as_ref()
            && binding.key.project_id == project
        {
            binding.invalidate();
            self.cancel_source_read();
            // Old queue workers retain their own original (revoked) source;
            // a new actor cannot join or receive their completion projection.
            self.refresh_ui.source_binding = None;
        }
        for active in self.source_control_mutations.values_mut().filter(|active| active.binding.key.project_id == project) {
            active.binding.invalidate();
            active.cancellation.cancel();
            active.pending.clear();
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use cosmic::iced::futures::StreamExt;

    async fn worker(task: Task<Message>) -> Message {
        let mut stream = iced::runtime::task::into_stream(task).unwrap();
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(cosmic::Action::App(message @ Message::RepositoryReaderReady { .. })) = action {
                return message;
            }
        }
        panic!("original reader completion missing");
    }

    #[tokio::test]
    async fn pending_pane_and_panel_use_one_original_capture_before_runtime_construction() {
        let directory = tempfile::tempdir().unwrap();
        git_binding::tests::init(directory.path(), "original\n");
        let (source_app, key) = git_binding::tests::app_for(directory.path());
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.sidebar_model = source_app.sidebar_model;
        let view = RepositoryView { project: key.project_id, repository: directory.path().to_owned(),
            worktree: directory.path().to_owned(), selected_file: Some("file.txt".into()) };
        app.card_state_mut(card).unwrap().kind = CardKind::Repository(view.clone());
        let binding = git_binding::Binding::new(key.clone());
        app.sidebar_model.source_control.as_mut().unwrap().set_command_binding_id(binding.id);
        app.refresh_ui.source_binding = Some(binding.clone());
        let initialize = app.attach_repository_pane(card, view);
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        let message = worker(initialize).await;
        let Message::RepositoryReaderReady { result: Ok(ready), .. } = &message else { panic!() };
        let original = ready.read_source.as_ref().unwrap().clone();
        let mut panel = binding;
        panel.initialize().unwrap();
        assert!(panel.read_source.as_ref().unwrap().same_source(&original));
        assert!(panel.authority.as_ref().unwrap().same_scope(original.authority()));
        // First project publication is not an ownership replacement and must
        // not retire a reader admitted during the initial load.
        app.repository_generations.insert(key.project_id, 1);
        app.publish_project_creation_source(key.project_id, 1,
            Some(worktree_dialog::WorktreeCreationSource::new(original.authority().clone())), false);
        assert!(original.is_available());
        drop(app.update(message));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else { panic!() };
        assert!(runtime.git_read_source().unwrap().same_source(&original));
        assert_eq!(app.refresh_ui.pane_reader_keys.get(&card), Some(&key));
    }

    #[tokio::test]
    async fn project_retirement_rejects_a_held_original_pane_initialization() {
        let directory = tempfile::tempdir().unwrap();
        git_binding::tests::init(directory.path(), "original\n");
        let (source_app, key) = git_binding::tests::app_for(directory.path());
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.sidebar_model = source_app.sidebar_model;
        let view = RepositoryView { project: key.project_id, repository: directory.path().to_owned(),
            worktree: directory.path().to_owned(), selected_file: Some("file.txt".into()) };
        app.card_state_mut(card).unwrap().kind = CardKind::Repository(view.clone());
        app.refresh_ui.source_binding = Some(git_binding::Binding::new(key.clone()));
        let task = app.attach_repository_pane(card, view);
        let message = worker(task).await;
        let Message::RepositoryReaderReady { result: Ok(ready), .. } = &message else { panic!() };
        let original = ready.read_source.as_ref().unwrap().clone();
        app.retire_project_creation_source(key.project_id);
        assert!(!original.is_available());
        drop(app.update(message));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert!(!app.refresh_ui.pane_reader_keys.contains_key(&card));
    }

    #[tokio::test]
    async fn update_prepare_ready_cancel_leaves_an_actual_retryable_pane() {
        let directory = tempfile::tempdir().unwrap();
        git_binding::tests::init(directory.path(), "original\n");
        let (source_app, key) = git_binding::tests::app_for(directory.path());
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        let card = app.workspace.focused().unwrap();
        app.sidebar_model = source_app.sidebar_model;
        let view = RepositoryView { project: key.project_id, repository: directory.path().to_owned(),
            worktree: directory.path().to_owned(), selected_file: Some("file.txt".into()) };
        app.card_state_mut(card).unwrap().kind = CardKind::Repository(view.clone());
        let binding = git_binding::Binding::new(key.clone());
        app.sidebar_model.source_control.as_mut().unwrap().set_command_binding_id(binding.id);
        app.refresh_ui.source_binding = Some(binding);
        let task = app.attach_repository_pane(card, view.clone());
        let held = worker(task).await;
        let Message::RepositoryReaderReady { result: Ok(ready), .. } = &held else { panic!() };
        let original = ready.read_source.as_ref().unwrap().clone();
        let (response, _receiver) = std::sync::mpsc::sync_channel(1);
        drop(app.update(Message::UiUpdate(instance::UiUpdateRequest {
            action: instance::UiUpdateAction::Prepare, response,
        })));
        assert!(app.update_prepared);
        drop(app.update(held));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
        assert!(!app.refresh_ui.pane_readers.contains_key(&card));
        assert!(original.is_available(), "this pane cannot retire the strip's reader");
        assert!(app.source_control_mutations.is_empty());
        let (response, receiver) = std::sync::mpsc::sync_channel(1);
        drop(app.update(Message::UiUpdate(instance::UiUpdateRequest {
            action: instance::UiUpdateAction::Cancel, response,
        })));
        assert_eq!(receiver.try_recv().unwrap(), "ok");
        assert!(!app.update_prepared);
        let retry = app.update(Message::RetryCard(card));
        let ready = worker(retry).await;
        drop(app.update(ready));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else { panic!() };
        assert_eq!(runtime.descriptor, view);
        assert!(runtime.git_read_source().unwrap().same_source(&original));
    }

    #[tokio::test]
    async fn actual_retry_after_initial_capture_failure_mints_a_new_unqualified_actor() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        git_binding::tests::init(&root, "original\n");
        let (source_app, key) = git_binding::tests::app_for(&root);
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.sidebar_model = source_app.sidebar_model;
        let view = RepositoryView { project: key.project_id, repository: root.clone(),
            worktree: root.clone(), selected_file: Some("file.txt".into()) };
        app.card_state_mut(card).unwrap().kind = CardKind::Repository(view.clone());
        let binding = git_binding::Binding::new(key.clone());
        let failed_actor = binding.id;
        app.sidebar_model.source_control.as_mut().unwrap().set_command_binding_id(binding.id);
        app.refresh_ui.source_binding = Some(binding);
        std::fs::rename(root.join(".git"), directory.path().join("parked-git")).unwrap();
        let opening = app.attach_repository_pane(card, view);
        let failed = worker(opening).await;
        assert!(matches!(&failed, Message::RepositoryReaderReady { result: Err(_), .. }));
        drop(app.update(failed));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
        assert!(app.refresh_ui.source_binding.as_ref().unwrap().initial_capture_failed());
        std::fs::rename(directory.path().join("parked-git"), root.join(".git")).unwrap();
        let retry = app.update(Message::RetryCard(card));
        assert_ne!(app.refresh_ui.source_binding.as_ref().unwrap().id, failed_actor);
        let ready = worker(retry).await;
        drop(app.update(ready));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else { panic!() };
        assert_ne!(runtime.git_read_source().unwrap().id(), failed_actor);
        assert!(runtime.git_read_source().unwrap().is_available());
    }

    #[tokio::test]
    async fn first_working_diff_navigation_survives_a_held_reader_initialization() {
        let directory = tempfile::tempdir().unwrap();
        git_binding::tests::init(directory.path(), "original\n");
        let (source_app, key) = git_binding::tests::app_for(directory.path());
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        let card = app.workspace.focused().unwrap();
        app.sidebar_model = source_app.sidebar_model;
        let view = RepositoryView { project: key.project_id, repository: directory.path().to_owned(),
            worktree: directory.path().to_owned(), selected_file: None };
        app.card_state_mut(card).unwrap().kind = CardKind::Repository(view.clone());
        app.refresh_ui.source_binding = Some(git_binding::Binding::new(key));
        let opening = app.attach_repository_pane(card, view);
        let held = worker(opening).await;
        drop(app.update(Message::Repository(RepositoryAction::OpenWorkingDiff {
            card, file: repository_widget::RepositoryFile { path: "file.txt".into(), staged: true, ..Default::default() },
            basis: DiffBasis::HeadToIndex,
        })));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert!(app.refresh_ui.pane_readers.get(&card).unwrap().open.is_some());
        // Arbitrary user commands are not replayed through this read-only slot.
        drop(app.update(Message::Repository(RepositoryAction::StageSelected { card, stage: true })));
        assert!(app.source_control_mutations.is_empty());
        drop(app.update(held));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else { panic!() };
        assert!(runtime.shows_diff());
        assert_eq!(runtime.selected_diff_basis(), &DiffBasis::HeadToIndex);
        assert_eq!(runtime.selected_path(), Some(Path::new("file.txt")));
    }

    #[test]
    fn concurrent_initialization_loser_never_pairs_a_new_capture_with_the_winning_source() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        git_binding::tests::init(&root, "original\n");
        let (_, key) = git_binding::tests::app_for(&root);
        let mut first = git_binding::Binding::new(key);
        let mut second = first.clone();
        first.initialize().unwrap();
        let original = first.read_source.as_ref().unwrap().clone();
        std::fs::rename(&root, directory.path().join("original")).unwrap();
        git_binding::tests::init(&root, "replacement\n");
        second.initialize().unwrap();
        assert!(second.read_source.as_ref().unwrap().same_source(&original));
        assert!(second.authority.as_ref().unwrap().same_scope(original.authority()));
        assert!(second.validate().is_err());
        assert!(second.mutation_service().is_err());
    }
}
