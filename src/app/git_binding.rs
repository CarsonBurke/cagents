//! The Git panel retains the physical repository displayed by its first read.
use super::*;
use crate::repository::{WorktreeAuthority, WorktreeExecutor};

pub(super) type ScopedService = RepositoryService<WorktreeExecutor<crate::process::SystemProcessExecutor>>;

#[derive(Clone, Debug)]
pub(super) struct Binding {
    pub(super) id: crate::model::OperationId,
    pub(super) key: ActiveWorktreeKey,
    pub(super) authority: Option<WorktreeAuthority>,
    pub(super) initialized: bool,
    pub(super) mutation_revision: Arc<std::sync::atomic::AtomicU64>,
}
impl Binding {
    pub(super) fn new(key: ActiveWorktreeKey) -> Self {
        Self { id: crate::model::OperationId::new(), key, authority: None, initialized: false, mutation_revision: Arc::new(std::sync::atomic::AtomicU64::new(0)) }
    }
    /// Blocking worker only. Once displayed, a refresh never recaptures a path.
    pub(super) fn initialize(&mut self) -> Result<(), String> {
        if !self.initialized {
            self.authority = if WorktreeAuthority::supported() {
                Some(WorktreeAuthority::capture(&self.key.canonical_path).map_err(|error| error.to_string())?)
            } else { None };
            self.initialized = true;
        }
        Ok(())
    }
    pub(super) fn validate(&self) -> Result<(), String> {
        self.authority.as_ref().map_or(Ok(()), |authority| authority.validate().map_err(|error| error.to_string()))
    }
    pub(super) fn history(&self, request: &HistoryRequest, query: &str, cancellation: &CancellationToken) -> Result<Vec<crate::repository::CommitSummary>, String> {
        let result = if self.authority.is_some() {
            self.mutation_service()?.filtered_history(request, query, cancellation)
        } else if !WorktreeAuthority::supported() {
            RepositoryService::system("git").filtered_history(request, query, cancellation)
        } else { return Err("The original Git worktree binding is unavailable.".into()); }.map_err(|error| error.to_string())?;
        self.validate()?;
        Ok(result)
    }
    pub(super) fn head_message(&self, head: &crate::repository::Oid, cancellation: &CancellationToken) -> Result<String, String> {
        let result = if self.authority.is_some() {
            self.mutation_service()?.commit_message(&self.key.canonical_path, head, cancellation)
        } else if !WorktreeAuthority::supported() {
            RepositoryService::system("git").commit_message(&self.key.canonical_path, head, cancellation)
        } else { return Err("The original Git worktree binding is unavailable.".into()); }.map_err(|error| error.to_string())?;
        self.validate()?;
        Ok(result)
    }
    /// Construction validates held worktree/private/common roots in a worker.
    pub(super) fn mutation_service(&self) -> Result<ScopedService, String> {
        let authority = self.authority.clone().ok_or("Git mutation authority is unavailable for this displayed worktree.")?;
        RepositoryService::system("git").with_worktree_authority(authority).map_err(|error| error.to_string())
    }
}

pub(super) struct SnapshotInput {
    pub(super) worktree: PathBuf,
    pub(super) options: crate::repository::SnapshotOptions,
    pub(super) cancellation: CancellationToken,
    pub(super) cache: Arc<std::sync::Mutex<crate::repository::SnapshotCache>>,
    pub(super) invalidated: Arc<std::sync::atomic::AtomicU8>,
    pub(super) browse_files: bool,
    pub(super) previous_errors: std::collections::BTreeMap<RepoPath, String>,
    pub(super) expanded: Vec<RepoPath>,
    pub(super) model_key: ActiveWorktreeKey,
    pub(super) reveal_demand: Option<RepoPath>,
}
impl SnapshotInput {
    pub(super) fn read<E: crate::process::ProcessExecutor + 'static>(self, service: &RepositoryService<E>) -> Result<Option<(RepositorySnapshot, SourceControlModel)>, String> {
        let Self { worktree, options, cancellation, cache, invalidated, browse_files, previous_errors, expanded, model_key, reveal_demand } = self;
                let mut cache = cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let invalidation = invalidated.swap(0, std::sync::atomic::Ordering::AcqRel);
                if invalidation != 0 {
                    cache.invalidate(invalidation & 2 != 0);
                }
                let Some(mut snapshot) = service
                    .snapshot_cached(&worktree, options, &cancellation, &mut cache)
                    .map_err(|error| error.to_string())?
                else {
                    return Ok(None);
                };
                if cancellation.is_cancelled() {
                    cache.invalidate(false);
                    return Err("source-control read cancelled".to_owned());
                }
                let mut reveal_error = None;
                let mut revealed_path = None;
                if browse_files {
                    let ignored = service
                        .ignored_files_with_limit(
                            &worktree,
                            &snapshot.files.paths,
                            options.status_limit,
                            &cancellation,
                        )
                        .map_err(|error| {
                            cache.invalidate(false);
                            error.to_string()
                        })?;
                    snapshot.files.listing_truncated |= ignored.listing_truncated;
                    snapshot.files.ignored = ignored.ignored;
                    snapshot.files.ignored_directories = ignored.ignored_directories;
                    repository_widget::expand_ignored_directories(
                        &worktree,
                        &mut snapshot.files,
                        &expanded,
                        &previous_errors,
                        &cancellation,
                    )
                    .inspect_err(|_| cache.invalidate(false))?;
                    if let Some(path) = reveal_demand.as_ref() {
                        match crate::repository::include_revealed_path(
                            &worktree,
                            &mut snapshot.files,
                            path,
                        ) {
                            Ok(target) => revealed_path = target,
                            Err(error) => reveal_error = Some(error),
                        }
                    }
                    cache.observe_browse(&worktree, &snapshot.files);
                }
                let mut model =
                    SourceControlModel::ready_for_tab(model_key, &snapshot, browse_files);
                model.revealed_path = revealed_path;
                if let Some(error) = reveal_error {
                    model.set_error(Some(format!(
                        "Could not reveal the requested path: {error}"
                    )));
                }
                if cancellation.is_cancelled() {
                    cache.invalidate(false);
                    return Err("source-control read cancelled".to_owned());
                }
                Ok(Some((snapshot, model)))
    }
}


#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use cosmic::iced::futures::StreamExt;

    fn git(root: &Path, arguments: &[&str]) -> std::process::Output {
        let mut command = std::process::Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") { command.env_remove(key); }
        }
        command.arg("-C").arg(root).args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.invalid");
        command.output().unwrap()
    }
    pub(crate) fn init(root: &Path, content: &str) {
        std::fs::create_dir_all(root).unwrap();
        assert!(git(root, &["init", "-b", "main"]).status.success());
        std::fs::write(root.join("file.txt"), content).unwrap();
        assert!(git(root, &["add", "file.txt"]).status.success());
    }
    pub(crate) fn app_for(root: &Path) -> (Cagents, ActiveWorktreeKey) {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.sidebar_model = sidebar::tests::panel_model();
        let source = app.sidebar_model.source_control.as_mut().unwrap();
        source.key.canonical_path = root.to_owned();
        let key = source.key.clone();
        for project in &mut app.sidebar_model.projects {
            for worktree in &mut project.worktrees {
                if worktree.id == key.worktree_id { worktree.path = root.to_owned(); }
            }
        }
        app.active_workspace = Some(root.to_owned());
        app.sidebar_state.sync_active_worktree(&app.sidebar_model, &SidebarFocus::Repository { worktree: root.to_owned() });
        let mut binding = Binding::new(key.clone());
        binding.initialize().unwrap();
        app.sidebar_model.source_control.as_mut().unwrap().set_command_binding_id(binding.id);
        app.refresh_ui.source_binding = Some(binding);
        (app, key)
    }
    async fn output(task: Task<Message>) -> Message {
        let mut stream = iced::runtime::task::into_stream(task).unwrap();
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action { return message; }
        }
        panic!("worker result missing");
    }
    fn commit(app: &mut Cagents, key: &ActiveWorktreeKey) -> Task<Message> {
        let operation = app.begin_mutation(key, MutationKind::Commit, None).unwrap();
        let request = CommitRequest { worktree: key.canonical_path.clone(), message: "captured commit".into() };
        app.mutation_task(key.clone(), operation, move |service, cancel| {
            commit_report(service.commit_with_amend(&request, false, cancel))
        })
    }

    #[tokio::test]
    async fn admitted_commit_refuses_replaced_worktree_and_private_metadata() {
        if !WorktreeAuthority::supported() { return; }
        for metadata_only in [false, true] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("checkout");
            init(&root, "original staged text\n");
            let (mut app, key) = app_for(&root);
            let task = commit(&mut app, &key);
            let original = temporary.path().join("original");
            if metadata_only {
                std::fs::rename(root.join(".git"), &original).unwrap();
                assert!(git(&root, &["init", "-b", "main"]).status.success());
                std::fs::write(root.join("file.txt"), "replacement staged text\n").unwrap();
                assert!(git(&root, &["add", "file.txt"]).status.success());
            } else {
                std::fs::rename(&root, &original).unwrap();
                init(&root, "replacement staged text\n");
            }
            let Message::SourceControlMutated { report, committed, .. } = output(task).await else { panic!() };
            assert!(report.is_some(), "physical replacement must refuse");
            assert!(!committed);
            assert!(!git(&root, &["rev-parse", "--verify", "HEAD"]).status.success(), "replacement repository must remain uncommitted");
            assert_eq!(std::fs::read_to_string(root.join("file.txt")).unwrap(), "replacement staged text\n");
        }
    }

    #[tokio::test]
    async fn unchanged_displayed_worktree_commits_and_new_actor_cannot_join_old_queue() {
        if !WorktreeAuthority::supported() { return; }
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("checkout");
        init(&root, "original staged text\n");
        let (mut app, key) = app_for(&root);
        let task = commit(&mut app, &key);
        assert!(matches!(output(task).await, Message::SourceControlMutated { report: None, committed: true, .. }));
        assert!(git(&root, &["rev-parse", "--verify", "HEAD"]).status.success());
        let old_id = app.refresh_ui.source_binding.as_ref().unwrap().id;
        let mut replacement = Binding::new(key.clone());
        replacement.initialize().unwrap();
        assert_ne!(old_id, replacement.id);
        app.refresh_ui.source_binding = Some(replacement);
        drop(app.enqueue_git_mutation(key.clone(), QueuedGitMutation::Unstage(Vec::new())));
        assert!(app.source_control_mutations[&key.canonical_path].pending.is_empty(), "new logical actor cannot coalesce with the old actor's queue");
    }
    #[test]
    fn cancelled_parked_success_cannot_create_discard_or_marker_confirmation() {
        if !WorktreeAuthority::supported() { return; }
        for markers in [false, true] {
            let temporary = tempfile::tempdir().unwrap();
            init(temporary.path(), "original\n");
            let (mut app, key) = app_for(temporary.path());
            let operation = app.begin_mutation(&key, MutationKind::Other, None).unwrap();
            let selection = vec![crate::repository::PathSelection::new(RepoPath::parse("file.txt").unwrap())];
            // A worker completed successfully, but its message is parked until
            // the user's cancellation has been reduced.
            let parked = if markers {
                Message::SourceControlStageChecked { key: key.clone(), operation, selection, markers: Default::default() }
            } else {
                Message::SourceControlGroupSelection { key: key.clone(), operation,
                    area: crate::repository::ChangeArea::Untracked, command: sidebar::ChangeGroupCommand::Discard,
                    result: Ok(selection) }
            };
            drop(app.update(Message::Sidebar(SidebarAction::CancelGitMutation { key: key.clone() })));
            assert!(app.source_control_mutations[&key.canonical_path].cancellation.is_cancelled());
            drop(app.update(parked));
            assert!(!app.source_control_mutations.contains_key(&key.canonical_path));
            assert!(app.sidebar_state.pending_discard().is_none());
            assert!(app.sidebar_model.source_control.as_ref().unwrap().pending_marker_stage.is_none());
        }
    }

    #[test]
    fn emitted_stage_and_pending_discard_cannot_cross_same_key_actor_replacement() {
        if !WorktreeAuthority::supported() { return; }
        let temporary = tempfile::tempdir().unwrap();
        init(temporary.path(), "original\n");
        let (mut app, key) = app_for(temporary.path());
        let original = app.refresh_ui.source_binding.as_ref().unwrap().id;
        let selection = vec![crate::repository::PathSelection::new(RepoPath::parse("file.txt").unwrap())];
        let old_stage = sidebar::commands::bound(original, SidebarAction::Stage { key: key.clone(), selection: selection.clone() });
        let request = crate::repository::DiscardRequest { worktree: key.canonical_path.clone(), tracked: selection.clone(), untracked: Vec::new(), confirmed: false };
        drop(app.update(Message::Sidebar(sidebar::commands::bound(original, SidebarAction::RequestDiscard { key: key.clone(), request }))));
        let confirm = app.sidebar_state.pending_discard().unwrap().confirm_action();
        let mut replacement = Binding::new(key.clone());
        replacement.initialize().unwrap();
        let current = replacement.id;
        app.refresh_ui.source_binding = Some(replacement);
        app.sidebar_model.source_control.as_mut().unwrap().set_command_binding_id(current);
        drop(app.update(Message::Sidebar(old_stage)));
        assert!(app.source_control_mutations.is_empty());
        // Even wrapping a prior prompt payload with the new panel owner cannot
        // turn the old confirmation into permission for that newer actor.
        drop(app.update(Message::Sidebar(sidebar::commands::bound(current, confirm))));
        assert!(app.source_control_mutations.is_empty());
        assert!(app.sidebar_state.pending_discard().is_some());
        drop(app.update(Message::Sidebar(sidebar::commands::bound(original, SidebarAction::CancelDiscard))));
        assert!(app.sidebar_state.pending_discard().is_none(), "old-owner negative cleanup remains possible");
        drop(app.update(Message::Sidebar(sidebar::commands::bound(current, SidebarAction::Stage { key: key.clone(), selection }))));
        assert!(app.source_control_mutations.contains_key(&key.canonical_path), "current actor retains positive admission");
    }

    #[test]
    fn publish_form_cannot_transfer_old_draft_to_same_key_new_actor() {
        if !WorktreeAuthority::supported() { return; }
        let temporary = tempfile::tempdir().unwrap();
        init(temporary.path(), "original\n");
        let (mut app, key) = app_for(temporary.path());
        let original = app.refresh_ui.source_binding.as_ref().unwrap().id;
        let branch = crate::repository::ValidatedBranchName::parse("main").unwrap();
        drop(app.update(Message::Sidebar(sidebar::commands::bound(original, SidebarAction::OpenPublish {
            key: key.clone(), branch: branch.clone(), remotes: vec!["origin".into()],
        }))));
        let old_submit = app.sidebar_state.publish_draft().unwrap().push_action().unwrap();
        let mut next = Binding::new(key.clone());
        next.initialize().unwrap();
        let current = next.id;
        app.refresh_ui.source_binding = Some(next);
        app.sidebar_model.source_control.as_mut().unwrap().set_command_binding_id(current);
        // A rebuilt panel must not give an existing form permission for its
        // freshly captured owner merely by wrapping its old draft in that ID.
        drop(app.update(Message::Sidebar(sidebar::commands::bound(current, old_submit))));
        assert!(app.source_control_mutations.is_empty());
        assert!(app.sidebar_state.publish_draft().is_some());
        drop(app.update(Message::Sidebar(sidebar::commands::bound(current, SidebarAction::CancelPublish))));
        assert!(app.sidebar_state.publish_draft().is_none());
        drop(app.update(Message::Sidebar(sidebar::commands::bound(current, SidebarAction::OpenPublish {
            key, branch, remotes: vec!["origin".into()],
        }))));
        assert_eq!(app.refresh_ui.publish_binding, Some(current));
        assert!(app.sidebar_state.publish_draft().is_some());
    }

}
