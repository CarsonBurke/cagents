//! Applying source control results to the bound worktree: a loaded status
//! snapshot, a page of commit history, and the end of a staged mutation.
use super::*;
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
        let snapshot: Arc<RepositorySnapshot> = match result {
            Ok(SourceControlRead::Changed(snapshot)) => snapshot.into(),
            Ok(SourceControlRead::Unchanged) => {
                let Some(source) = self.sidebar_model.source_control.as_mut() else {
                    return ControlFlow::Break(Task::none());
                };
                source.set_reloading(false);
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
                return ControlFlow::Continue(self.refresh_branch_changes(&snapshot));
            }
            Err(error) => {
                self.replace_source_control(SourceControlModel::failed(key, error));
                self.cancel_branch_changes();
                return ControlFlow::Continue(Task::none());
            }
        };
        self.apply_worktree_status(&key, &snapshot.status);
        self.replace_source_control(
            SourceControlModel::ready(key, &snapshot).built_from(Arc::clone(&snapshot)),
        );
        ControlFlow::Continue(self.refresh_branch_changes(&snapshot))
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

/// Compares a finished read with the snapshot the panel was built from. It
/// runs on the reader's blocking thread, so an unchanged worktree costs the
/// UI thread nothing but the answer.
pub(super) fn source_control_read(
    previous: Option<&RepositorySnapshot>,
    snapshot: RepositorySnapshot,
) -> SourceControlRead {
    if previous == Some(&snapshot) {
        SourceControlRead::Unchanged
    } else {
        SourceControlRead::Changed(Box::new(snapshot))
    }
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

    /// Starts a reload and answers it with what the worker would make of
    /// `read`, given the snapshot the panel held when the reload began.
    fn reload(app: &mut Cagents, key: &ActiveWorktreeKey, read: RepositorySnapshot) {
        let previous = source(app).source_snapshot().cloned();
        drop(app.reload_source_control(key));
        let request = app.refresh_ui.source_generation;
        let read = source_control_read(previous.as_deref(), read);
        drop(app.source_control_loaded(key.clone(), request, Ok(read)));
    }

    #[test]
    fn source_control_reload_with_an_identical_snapshot_keeps_the_built_model() {
        let (mut app, key) = bound();
        let request = app.refresh_ui.source_generation;
        drop(app.source_control_loaded(
            key.clone(),
            request,
            Ok(SourceControlRead::Changed(Box::new(snapshot()))),
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
        drop(app.source_control_loaded(key.clone(), request, Ok(SourceControlRead::Unchanged)));
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
        let shown = snapshot();
        assert!(matches!(
            source_control_read(None, snapshot()),
            SourceControlRead::Changed(_)
        ));
        assert!(matches!(
            source_control_read(Some(&shown), snapshot()),
            SourceControlRead::Unchanged
        ));
        let mut status = snapshot();
        status.status.entries.push(unstaged("src/lib.rs"));
        assert!(matches!(
            source_control_read(Some(&shown), status),
            SourceControlRead::Changed(read) if read.status.entries.len() == 2
        ));
        let mut ignored = snapshot();
        ignored.files.ignored.push(repo_path("build/out.log"));
        assert!(matches!(
            source_control_read(Some(&shown), ignored),
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
            let read = source_control_read(
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
}
