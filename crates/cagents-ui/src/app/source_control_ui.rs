//! Applying source control results to the bound worktree: a loaded status
//! snapshot, a page of commit history, and the end of a staged mutation.
use super::*;
use crate::repository::SnapshotDigest;
use std::ops::ControlFlow;

impl Cagents {
    pub(super) fn source_control_loaded(
        &mut self,
        key: ActiveWorktreeKey,
        request: u64,
        result: Result<SourceControlRead, String>,
    ) -> Handled {
        if request != self.refresh_ui.source_generation || !self.source_control_is_bound(&key) {
            return ControlFlow::Break(Task::none());
        }
        self.refresh_ui.source_cancel = None;
        let model = match result {
            Ok(SourceControlRead::Changed(model)) => *model,
            Ok(SourceControlRead::Unchanged { digest }) => {
                let Some(source) = self.sidebar_model.source_control.as_mut() else {
                    return ControlFlow::Break(Task::none());
                };
                source.set_reloading(false);
                source.set_source_digest(digest);
                // The worker compared against the snapshot this model was
                // built from when the read began. A model that no longer
                // holds one was replaced or edited since, and the request
                // generation normally drops such an answer before here.
                let Some(snapshot) = source.source_snapshot().cloned() else {
                    return ControlFlow::Break(Task::none());
                };
                // The rows stand as built. The worktree row and the branch
                // comparison are kept current exactly as a rebuild would.
                self.apply_worktree_status(&key, &snapshot.status);
                let branch = self.refresh_branch_changes(&snapshot);
                return ControlFlow::Continue(Task::batch([branch, self.load_deferred_files()]));
            }
            Err(error) => {
                self.replace_source_control(SourceControlModel::failed(key, error));
                self.cancel_branch_changes();
                return ControlFlow::Continue(Task::none());
            }
        };
        let Some(snapshot) = model.source_snapshot().cloned() else {
            return ControlFlow::Break(Task::none());
        };
        self.apply_worktree_status(&key, &snapshot.status);
        self.replace_source_control(model);
        // A worktree's first read decides which Git view it opens on.
        if let Some(source) = self.sidebar_model.source_control.as_ref() {
            self.sidebar_state.settle_changes_tab(source);
        }
        let branch = self.refresh_branch_changes(&snapshot);
        ControlFlow::Continue(Task::batch([branch, self.load_deferred_files()]))
    }

    /// Reads the bound worktree again when Files is shown over rows whose
    /// Files tree a Changes-only read left unbuilt, and no read is running
    /// that could build it.
    ///
    /// Files comes into view by more than its own controls: an inspector
    /// opening or expanding, or a pane becoming the workspace's only one.
    /// The projection's rebind and each landed read call this, so whichever
    /// way Files appears, a read builds its tree and "Loading files…" is
    /// never left standing with nothing loading. A Files read always builds
    /// the tree, so this asks once.
    pub(super) fn load_deferred_files(&mut self) -> Task<Message> {
        if self.refresh_ui.source_cancel.is_some()
            || self.workspace_view_shown() != Some(context_sections::WorkspaceView::Files)
        {
            return Task::none();
        }
        let Some(source) = self
            .sidebar_model
            .source_control
            .as_ref()
            .filter(|source| !source.files_built())
        else {
            return Task::none();
        };
        let key = source.key.clone();
        self.reload_source_control(&key)
    }

    /// Shows a read's status on the sidebar row of the worktree it read.
    fn apply_worktree_status(&mut self, key: &ActiveWorktreeKey, status: &StatusSnapshot) {
        if let Some(worktree) = self
            .sidebar_model
            .projects
            .iter_mut()
            .find(|project| project.id == key.project_id)
            .and_then(|project| {
                project
                    .worktrees
                    .iter_mut()
                    .find(|worktree| worktree.id == key.worktree_id)
            })
        {
            worktree.apply_status(status);
        }
    }

    /// Installs a model built from a fresh read in place of the bound one.
    fn replace_source_control(&mut self, mut next: SourceControlModel) {
        // The reload replaces the rows and nothing else: an in-flight
        // mutation, its last failure, and the history already walked
        // belong to the binding, not to the page of Git output.
        if let Some(previous) = self.sidebar_model.source_control.as_ref() {
            previous.carry_mutation_into(&mut next);
        }
        self.sidebar_model.source_control = Some(next);
    }

    pub(super) fn source_control_history(
        &mut self,
        generation: u64,
        key: ActiveWorktreeKey,
        page: u32,
        head: crate::repository::Oid,
        result: Result<Vec<CommitSummary>, String>,
    ) -> Handled {
        if !self.source_control_is_bound(&key) || !self.history_ui.accepts_page(generation) {
            return ControlFlow::Break(Task::none());
        }
        match result {
            Ok(commits) => {
                if let Some(source) = self.sidebar_model.source_control.as_mut() {
                    // A page that arrives out of order would append the
                    // same commits twice, so only the page the walk is
                    // waiting for is taken.
                    if source.head_oid.as_deref() == Some(head.as_str())
                        && source.next_history_page == page
                    {
                        source.append_history(&commits);
                    }
                }
            }
            Err(error) => {
                if let Some(source) = self.sidebar_model.source_control.as_mut()
                    && source.head_oid.as_deref() == Some(head.as_str())
                    && source.next_history_page == page
                    && source.history_loading
                {
                    source.history_failed(format!("Could not read history: {error}"));
                }
            }
        }
        ControlFlow::Continue(Task::none())
    }

    pub(super) fn source_control_mutated(
        &mut self,
        key: ActiveWorktreeKey,
        operation: uuid::Uuid,
        report: Option<String>,
        committed: bool,
    ) -> Handled {
        let Some(mutation) = self
            .source_control_mutations
            .get(&key.canonical_path)
            .copied()
            .filter(|mutation| mutation.id == operation)
        else {
            return ControlFlow::Break(Task::none());
        };
        let notice_key =
            (mutation.kind == MutationKind::Commit).then(|| NoticeKey::GitCommit(key.clone()));
        self.source_control_mutations.remove(&key.canonical_path);
        if committed && let Some(revision) = mutation.commit_revision {
            self.sidebar_state.clear_submitted_commit(&key, revision);
        }
        let mut refreshes = Vec::new();
        for (&card, runtime) in &mut self.cards {
            if let CardRuntime::Repository(runtime) = runtime
                && runtime.descriptor.worktree == key.canonical_path
            {
                refreshes.push(repository_task(runtime.refresh_after_mutation(card)));
            }
        }
        let bound = self
            .sidebar_model
            .source_control
            .as_mut()
            .filter(|source| source.key.canonical_path == key.canonical_path)
            .map(|source| {
                source.set_mutation(None);
                source.set_error(report.clone());
                source.key.clone()
            });
        if let Some(report) = report {
            let notice =
                TransientNotice::error(format!("{}: {report}", display_path(&key.canonical_path)));
            self.status = Some(match notice_key {
                Some(key) => notice.with_key(key),
                None => notice,
            });
        } else if let Some(notice_key) = notice_key {
            // Only an admitted Commit's complete success resolves its
            // report. Path selections and push targets are not captured
            // here, so other Git actions retain their unkeyed notices.
            self.resolve_notice(notice_key);
        }
        // Navigation does not cancel Git. Only this exact completion
        // releases the worktree, then the index is read afresh.
        if let Some(bound) = bound {
            refreshes.push(self.reload_source_control(&bound));
        }
        ControlFlow::Continue(Task::batch(refreshes))
    }
}

/// What a read that builds `tree` may compare with: the snapshot `source` was
/// built from and its digest, unless its rows cannot stand for that read.
pub(super) fn kept_source(
    source: Option<&SourceControlModel>,
    tree: FileTree,
) -> (Option<Arc<RepositorySnapshot>>, Option<SnapshotDigest>) {
    source
        .filter(|source| source.stands_for(tree))
        .map_or((None, None), |source| {
            (
                source.source_snapshot().cloned(),
                source.source_digest().copied(),
            )
        })
}

/// What the worker makes of Git's answer before any rows are built: the
/// snapshot to build from with its digest, or, when nothing changed and
/// the panel's rows can stand, the answer to send back as is.
///
/// With Files shown, an unchanged answer still rebuilds from the kept
/// snapshot: Git's part stands, but the caller lists ignored folders again
/// from the filesystem, which the digest does not cover, so the kept
/// listing is cleared for it rather than carried over.
pub(super) fn snapshot_to_build(
    read: SnapshotRead,
    previous: Option<&RepositorySnapshot>,
    previous_digest: Option<SnapshotDigest>,
    tree: FileTree,
) -> Result<ControlFlow<SourceControlRead, (RepositorySnapshot, Option<SnapshotDigest>)>, String> {
    match read {
        SnapshotRead::Changed { snapshot, digest } => {
            Ok(ControlFlow::Continue((*snapshot, digest)))
        }
        SnapshotRead::Unchanged if tree == FileTree::Deferred => {
            Ok(ControlFlow::Break(SourceControlRead::Unchanged {
                digest: previous_digest,
            }))
        }
        SnapshotRead::Unchanged => {
            let mut snapshot = previous
                .cloned()
                .ok_or("an unchanged read has no snapshot to keep")?;
            snapshot.files.ignored.clear();
            snapshot.files.ignored_directories.clear();
            snapshot.files.ignored_directory_errors.clear();
            Ok(ControlFlow::Continue((snapshot, previous_digest)))
        }
    }
}

/// Compares a finished read with the snapshot the panel was built from and,
/// when they differ, builds the model that replaces it. It runs on the
/// reader's blocking thread, so a reload costs the UI thread only installing
/// the answer, however many files the worktree holds.
pub(super) fn source_control_read(
    key: ActiveWorktreeKey,
    previous: Option<&RepositorySnapshot>,
    snapshot: RepositorySnapshot,
    digest: Option<SnapshotDigest>,
    tree: FileTree,
) -> SourceControlRead {
    if previous == Some(&snapshot) {
        return SourceControlRead::Unchanged { digest };
    }
    let snapshot = Arc::new(snapshot);
    SourceControlRead::Changed(Box::new(
        SourceControlModel::build(key, &snapshot, tree).built_from(Arc::clone(&snapshot), digest),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{ChangeKind, ChangedPath, WorkingFiles};

    fn repo_path(path: &str) -> RepoPath {
        RepoPath::parse(path).unwrap()
    }

    fn unstaged(path: &str) -> ChangedPath {
        ChangedPath {
            path: repo_path(path),
            old_path: None,
            area: ChangeArea::Unstaged,
            kind: ChangeKind::Modified,
            conflict: None,
            submodule: false,
            added_lines: None,
            removed_lines: None,
        }
    }

    /// A worktree with one modified file and an ignored `build` folder.
    fn snapshot() -> RepositorySnapshot {
        RepositorySnapshot {
            status: StatusSnapshot {
                entries: vec![unstaged("src/main.rs")],
                ..StatusSnapshot::default()
            },
            files: WorkingFiles {
                paths: vec![repo_path("README.md"), repo_path("src/main.rs")],
                tracked_count: 2,
                ignored_directories: vec![repo_path("build")],
                ..WorkingFiles::default()
            },
            remotes: Vec::new(),
            local_refs: Vec::new(),
            remote_refs: Vec::new(),
            remote_heads: Vec::new(),
        }
    }

    /// The same worktree with the expanded `build` folder unreadable.
    fn unreadable_build() -> RepositorySnapshot {
        let mut snapshot = snapshot();
        snapshot
            .files
            .ignored_directory_errors
            .insert(repo_path("build"), "Permission denied".into());
        snapshot
    }

    fn bound() -> (Cagents, ActiveWorktreeKey) {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let key = ActiveWorktreeKey {
            project_id: crate::model::ProjectId::new(),
            worktree_id: crate::model::WorktreeId::new(),
            canonical_path: "/repo".into(),
        };
        app.sidebar_model.source_control = Some(SourceControlModel::loading(key.clone()));
        (app, key)
    }

    fn source(app: &Cagents) -> &SourceControlModel {
        app.sidebar_model.source_control.as_ref().unwrap()
    }

    /// What the worker makes of `read` for a panel showing Files.
    fn worker_read(
        key: &ActiveWorktreeKey,
        previous: Option<&RepositorySnapshot>,
        read: RepositorySnapshot,
    ) -> SourceControlRead {
        source_control_read(key.clone(), previous, read, None, FileTree::Built)
    }

    fn changed_rows(read: &SourceControlRead) -> usize {
        match read {
            SourceControlRead::Changed(model) => {
                model.source_snapshot().unwrap().status.entries.len()
            }
            SourceControlRead::Unchanged { .. } => panic!("the read should have changed"),
        }
    }

    /// Starts a reload and answers it with what the worker would make of
    /// `read`, given the snapshot the panel held when the reload began.
    fn reload(app: &mut Cagents, key: &ActiveWorktreeKey, read: RepositorySnapshot) {
        let previous = source(app).source_snapshot().cloned();
        drop(app.reload_source_control(key));
        let request = app.refresh_ui.source_generation;
        let read = worker_read(key, previous.as_deref(), read);
        drop(app.source_control_loaded(key.clone(), request, Ok(read)));
    }

    #[test]
    fn source_control_reload_with_an_identical_snapshot_keeps_the_built_model() {
        let (mut app, key) = bound();
        let request = app.refresh_ui.source_generation;
        drop(app.source_control_loaded(
            key.clone(),
            request,
            Ok(worker_read(&key, None, snapshot())),
        ));
        let first = Arc::clone(source(&app).source_snapshot().unwrap());
        // A rebuild allocates its rows while the model it replaces still
        // holds the old ones, so the same allocation shows none was built.
        let rows = |app: &Cagents| {
            source(app)
                .group(ChangeArea::Unstaged)
                .unwrap()
                .rows()
                .as_ptr()
        };
        let built = rows(&app);

        drop(app.reload_source_control(&key));
        assert_eq!(source(&app).progress_label(), Some("Refreshing…"));
        let request = app.refresh_ui.source_generation;
        drop(app.source_control_loaded(
            key.clone(),
            request,
            Ok(SourceControlRead::Unchanged { digest: None }),
        ));
        assert_eq!(source(&app).progress_label(), None);
        assert!(Arc::ptr_eq(source(&app).source_snapshot().unwrap(), &first));
        assert_eq!(rows(&app), built);

        let mut changed = snapshot();
        changed.status.entries.push(unstaged("src/lib.rs"));
        reload(&mut app, &key, changed);
        assert!(!Arc::ptr_eq(
            source(&app).source_snapshot().unwrap(),
            &first
        ));
        let unstaged = source(&app)
            .group(ChangeArea::Unstaged)
            .unwrap()
            .rows()
            .iter()
            .map(|row| row.entry.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            unstaged,
            [repo_path("src/main.rs"), repo_path("src/lib.rs")]
        );
    }

    #[test]
    fn source_control_worker_reports_unchanged_only_for_an_equal_snapshot() {
        let (_, key) = bound();
        let shown = snapshot();
        assert_eq!(changed_rows(&worker_read(&key, None, snapshot())), 1);
        assert!(matches!(
            worker_read(&key, Some(&shown), snapshot()),
            SourceControlRead::Unchanged { .. }
        ));
        let mut status = snapshot();
        status.status.entries.push(unstaged("src/lib.rs"));
        assert_eq!(changed_rows(&worker_read(&key, Some(&shown), status)), 2);
        let mut ignored = snapshot();
        ignored.files.ignored.push(repo_path("build/out.log"));
        assert!(matches!(
            worker_read(&key, Some(&shown), ignored),
            SourceControlRead::Changed(_)
        ));
    }

    /// Only a model built from a snapshot can be kept by an equal one; a
    /// folder listing or a failed read always takes the next answer.
    #[test]
    fn models_not_built_from_a_snapshot_never_match_a_read() {
        let (mut app, key) = bound();
        assert!(source(&app).source_snapshot().is_none());
        let request = app.refresh_ui.source_generation;
        drop(app.source_control_loaded(key.clone(), request, Err("not a repository".into())));
        assert!(source(&app).source_snapshot().is_none());
        assert!(
            SourceControlModel::directory(key, &[repo_path("README.md")])
                .source_snapshot()
                .is_none()
        );
    }

    /// Retrying an unreadable ignored folder clears its error before the
    /// read. When that read fails the same way it is equal to the snapshot
    /// the panel was built from, and it must still put the error back.
    #[test]
    fn a_retried_ignored_folder_that_fails_again_still_shows_its_error() {
        let build = repo_path("build");
        let retries: [fn(&ActiveWorktreeKey) -> SidebarAction; 2] = [
            |key| SidebarAction::RefreshWorktree { key: key.clone() },
            |key| SidebarAction::ToggleDirectory {
                key: key.clone(),
                path: RepoPath::parse("build").unwrap(),
            },
        ];
        for retry in retries {
            let (mut app, key) = bound();
            reload(&mut app, &key, unreadable_build());
            assert!(source(&app).ignored_directory_errors().contains_key(&build));

            drop(app.update(Message::Sidebar(retry(&key))));
            assert!(!source(&app).ignored_directory_errors().contains_key(&build));
            let request = app.refresh_ui.source_generation;
            let read = worker_read(
                &key,
                source(&app).source_snapshot().map(|shown| &**shown),
                unreadable_build(),
            );
            assert!(matches!(read, SourceControlRead::Changed(_)));
            drop(app.source_control_loaded(key.clone(), request, Ok(read)));
            assert_eq!(
                source(&app)
                    .ignored_directory_errors()
                    .get(&build)
                    .map(String::as_str),
                Some("Permission denied"),
            );
        }
    }

    /// With only Changes showing, the reader leaves the Files tree unbuilt,
    /// and that model cannot stand for the read that showing Files starts,
    /// even when Git answers it identically.
    #[test]
    fn a_changes_only_read_defers_the_files_tree_until_files_is_shown() {
        let (mut app, key) = bound();
        let digest = Some(SnapshotDigest::fixture(1));
        let request = app.refresh_ui.source_generation;
        let read = source_control_read(key.clone(), None, snapshot(), digest, FileTree::Deferred);
        drop(app.source_control_loaded(key.clone(), request, Ok(read)));
        let deferred = source(&app);
        assert!(deferred.files.is_empty());
        assert!(!deferred.files_built());
        assert!(deferred.stands_for(FileTree::Deferred));
        assert!(!deferred.stands_for(FileTree::Built));

        // The reader withholds the kept snapshot from a Files read, so an
        // equal answer is built rather than kept.
        let (kept, kept_digest) = kept_source(Some(deferred), FileTree::Built);
        assert_eq!((kept.as_ref(), kept_digest), (None, None));
        assert!(kept_source(Some(deferred), FileTree::Deferred).0.is_some());
        drop(app.reload_source_control(&key));
        let request = app.refresh_ui.source_generation;
        let read = source_control_read(
            key.clone(),
            kept.as_deref(),
            snapshot(),
            digest,
            FileTree::Built,
        );
        drop(app.source_control_loaded(key, request, Ok(read)));
        let built = source(&app);
        assert!(built.files_built());
        assert!(
            built
                .files
                .iter()
                .any(|row| row.path == repo_path("src/main.rs"))
        );
        assert!(built.stands_for(FileTree::Built) && built.stands_for(FileTree::Deferred));
        assert_eq!(built.source_digest(), digest.as_ref());
    }

    /// A touched but unedited file changes the digest and not the snapshot.
    /// The model keeps its rows and takes the new digest, or every later
    /// read would miss it and run in full.
    #[test]
    fn an_equal_read_with_a_new_digest_keeps_the_rows_and_takes_the_digest() {
        let (mut app, key) = bound();
        let request = app.refresh_ui.source_generation;
        let first = Some(SnapshotDigest::fixture(1));
        let read = source_control_read(key.clone(), None, snapshot(), first, FileTree::Deferred);
        drop(app.source_control_loaded(key.clone(), request, Ok(read)));
        let shown = Arc::clone(source(&app).source_snapshot().unwrap());
        assert_eq!(source(&app).source_digest(), first.as_ref());

        let touched = Some(SnapshotDigest::fixture(2));
        drop(app.reload_source_control(&key));
        let request = app.refresh_ui.source_generation;
        let read = source_control_read(
            key.clone(),
            Some(&shown),
            snapshot(),
            touched,
            FileTree::Deferred,
        );
        assert!(matches!(read, SourceControlRead::Unchanged { .. }));
        drop(app.source_control_loaded(key, request, Ok(read)));
        assert!(Arc::ptr_eq(source(&app).source_snapshot().unwrap(), &shown));
        assert_eq!(source(&app).source_digest(), touched.as_ref());
    }

    /// Files can come into view without its own controls: here an
    /// inspector opens on a workspace section saved showing Files, over
    /// rows a Changes-only read built. Showing it reads the worktree again
    /// rather than leaving "Loading files…" with nothing loading, and a
    /// Changes-only read landing while Files shows does the same.
    #[test]
    fn files_shown_over_a_deferred_tree_reads_the_worktree_for_it() {
        use super::super::context_sections::{Section, WorkspaceView};

        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().expect("fixture has a focused card");
        app.sidebar_model = sidebar::test_support::panel_model();
        app.sidebar_state = sidebar::test_support::panel_state(&app.sidebar_model);
        app.context_sections.workspace_view = WorkspaceView::Files;
        app.context_sections.set_open(Section::Workspace, true);
        drop(app.rebind_source_control());
        let key = app.sidebar_state.active_worktree().cloned().expect("bound");
        app.cancel_source_read();
        let deferred = |key: &ActiveWorktreeKey| {
            let snapshot = Arc::new(snapshot());
            SourceControlModel::build(key.clone(), &snapshot, FileTree::Deferred)
                .built_from(snapshot, Some(SnapshotDigest::fixture(1)))
        };
        app.sidebar_model.source_control = Some(deferred(&key));
        assert_eq!(app.workspace_view_shown(), None);
        // Nothing reads for Files while it is out of view.
        drop(app.rebind_source_control());
        assert!(app.refresh_ui.source_cancel.is_none());

        drop(app.update(Message::TogglePaneInspector(card)));
        app.settle_pane_inspectors();
        drop(app.rebind_source_control());
        assert_eq!(app.workspace_view_shown(), Some(WorkspaceView::Files));
        assert!(
            app.refresh_ui.source_cancel.is_some(),
            "showing Files reads"
        );
        // The rows stay up while the read builds the tree.
        assert!(
            source(&app).source_snapshot().is_some() && source(&app).progress_label().is_some()
        );
        let request = app.refresh_ui.source_generation;
        drop(app.rebind_source_control());
        assert_eq!(
            app.refresh_ui.source_generation, request,
            "a running read is not restarted"
        );

        // A Changes-only read that was already running lands deferred.
        let read = source_control_read(key.clone(), None, snapshot(), None, FileTree::Deferred);
        drop(app.source_control_loaded(key.clone(), request, Ok(read)));
        assert!(
            app.refresh_ui.source_cancel.is_some(),
            "its landing reads for Files"
        );
        assert_ne!(app.refresh_ui.source_generation, request);

        // Once a read builds the tree, nothing more is asked.
        let request = app.refresh_ui.source_generation;
        let read = source_control_read(key.clone(), None, snapshot(), None, FileTree::Built);
        drop(app.source_control_loaded(key, request, Ok(read)));
        assert!(source(&app).files_built());
        assert!(app.refresh_ui.source_cancel.is_none());
        drop(app.rebind_source_control());
        assert!(app.refresh_ui.source_cancel.is_none());
    }

    /// An unchanged answer stands as is for Changes. For Files it rebuilds
    /// from the kept snapshot with the ignored listing cleared, because the
    /// worker lists ignored folders again and the digest never covered
    /// them; a stale listing or error must not survive into the new rows.
    #[test]
    fn an_unchanged_answer_for_files_relists_ignored_folders() {
        let kept = unreadable_build();
        let digest = Some(SnapshotDigest::fixture(3));

        match snapshot_to_build(
            SnapshotRead::Unchanged,
            Some(&kept),
            digest,
            FileTree::Deferred,
        ) {
            Ok(ControlFlow::Break(SourceControlRead::Unchanged { digest: answered })) => {
                assert_eq!(answered, digest);
            }
            _ => panic!("an unchanged Changes read keeps the rows"),
        }

        let Ok(ControlFlow::Continue((rebuilt, answered))) = snapshot_to_build(
            SnapshotRead::Unchanged,
            Some(&kept),
            digest,
            FileTree::Built,
        ) else {
            panic!("an unchanged Files read rebuilds from the kept snapshot");
        };
        assert_eq!(answered, digest);
        assert_eq!(rebuilt.status, kept.status);
        assert_eq!(rebuilt.files.paths, kept.files.paths);
        assert!(rebuilt.files.ignored.is_empty());
        assert!(rebuilt.files.ignored_directories.is_empty());
        assert!(rebuilt.files.ignored_directory_errors.is_empty());

        assert!(snapshot_to_build(SnapshotRead::Unchanged, None, digest, FileTree::Built).is_err());

        let changed = SnapshotRead::Changed {
            snapshot: Box::new(snapshot()),
            digest: Some(SnapshotDigest::fixture(4)),
        };
        let Ok(ControlFlow::Continue((read, answered))) =
            snapshot_to_build(changed, Some(&kept), digest, FileTree::Deferred)
        else {
            panic!("a changed read builds");
        };
        assert_eq!(answered, Some(SnapshotDigest::fixture(4)));
        assert_eq!(
            read.files.ignored_directories,
            snapshot().files.ignored_directories
        );
    }
}
