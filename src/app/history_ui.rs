//! Lazy, cancellable inspection of immutable commits from the history list.
use super::*;
use crate::repository::{DiffEntry, Oid, RepositoryError};
use crate::sidebar::CommitChanges;

#[derive(Debug, Clone)]
pub enum ReadFailure {
    Limited {
        matches: Vec<CommitSummary>,
        notice: String,
    },
    Failed(String),
}

impl From<RepositoryError> for ReadFailure {
    fn from(error: RepositoryError) -> Self {
        match error {
            RepositoryError::HistorySearchLimit { matches, .. } => Self::Limited {
                notice: "Search reached its limit; more matches may exist. \
                         Refine the query to search further."
                    .into(),
                matches,
            },
            error => Self::Failed(error.to_string()),
        }
    }
}

impl From<String> for ReadFailure {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

impl From<&str> for ReadFailure {
    fn from(error: &str) -> Self {
        Self::Failed(error.to_owned())
    }
}

#[derive(Debug, Default)]
pub(super) struct State {
    generation: u64,
    page_generation: u64,
    page_cancellation: Option<CancellationToken>,
    cancellation: Option<CancellationToken>,
}

impl State {
    pub(super) fn next_page_request(&mut self) -> u64 {
        self.cancel_pages();
        self.page_cancellation = Some(CancellationToken::default());
        self.page_generation
    }

    pub(super) fn cancel_pages(&mut self) {
        self.page_generation = self.page_generation.wrapping_add(1);
        if let Some(cancellation) = self.page_cancellation.take() {
            cancellation.cancel();
        }
    }

    pub(super) fn page_cancellation(&self) -> CancellationToken {
        self.page_cancellation.clone().unwrap_or_default()
    }

    pub(super) fn accepts_page(&self, generation: u64) -> bool {
        self.page_generation == generation
    }

    pub(super) fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.cancel();
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancel();
        self.cancel_pages();
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Loaded {
        key: ActiveWorktreeKey,
        oid: Oid,
        generation: u64,
        result: Result<Vec<DiffEntry>, String>,
    },
}

impl Cagents {
    pub(super) fn open_history_commit(
        &mut self,
        key: ActiveWorktreeKey,
        oid: Oid,
    ) -> Task<Message> {
        if self.modal_is_open() || !self.source_control_is_bound(&key) {
            return Task::none();
        }
        let Some(source) = self.sidebar_model.source_control.as_mut() else {
            return Task::none();
        };
        let Some(commit) = source
            .history
            .iter()
            .find(|row| row.oid == oid)
            .cloned()
            .or_else(|| {
                source
                    .commit_changes
                    .as_ref()
                    .filter(|changes| changes.commit.oid == oid)
                    .map(|changes| changes.commit.clone())
            })
        else {
            return Task::none();
        };
        let parent = commit.parent.clone();
        source.commit_changes = Some(CommitChanges {
            visible_files: 200,
            commit,
            result: None,
        });
        self.history_ui.cancel();
        let generation = self.history_ui.generation;
        let cancellation = CancellationToken::default();
        self.history_ui.cancellation = Some(cancellation.clone());
        cosmic::task::future(async move {
            let worktree = key.canonical_path.clone();
            let revision = oid.clone();
            let result = tokio::task::spawn_blocking(move || {
                RepositoryService::system("git")
                    .commit_diff_entries(&worktree, &revision, parent.as_ref(), &cancellation)
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| format!("Commit reader failed: {error}"))
            .and_then(|result| result);
            Message::CommitUi(Action::Loaded {
                key,
                oid,
                generation,
                result,
            })
        })
    }

    pub(super) fn update_commit_ui(&mut self, action: Action) -> Task<Message> {
        let Action::Loaded {
            key,
            oid,
            generation,
            result,
        } = action;
        if generation != self.history_ui.generation || !self.source_control_is_bound(&key) {
            return Task::none();
        }
        self.history_ui.cancellation = None;
        if let Some(changes) = self
            .sidebar_model
            .source_control
            .as_mut()
            .and_then(|source| source.commit_changes.as_mut())
            && changes.commit.oid == oid
        {
            changes.result = Some(result);
        }
        Task::none()
    }

    pub(super) fn open_commit_file(
        &mut self,
        key: ActiveWorktreeKey,
        oid: Oid,
        path: RepoPath,
    ) -> Task<Message> {
        if self.modal_is_open() || !self.source_control_is_bound(&key) {
            return Task::none();
        }
        let Some(changes) = self
            .sidebar_model
            .source_control
            .as_ref()
            .and_then(|source| source.commit_changes.as_ref())
            .filter(|changes| changes.commit.oid == oid)
        else {
            return Task::none();
        };
        let Some(Ok(entries)) = &changes.result else {
            return Task::none();
        };
        let Some(entry) = entries.iter().find(|entry| entry.path == path) else {
            return Task::none();
        };
        let old_path = entry.old_path.clone();
        let basis = DiffBasis::CommitToParent {
            oid,
            parent: changes.commit.parent.clone(),
        };
        self.open_committed_file(key, path, old_path, basis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::CommitIdentity;

    fn fixture() -> (Cagents, ActiveWorktreeKey, Oid, Oid) {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let key = ActiveWorktreeKey {
            project_id: crate::model::ProjectId::new(),
            worktree_id: crate::model::WorktreeId::new(),
            canonical_path: "/repo".into(),
        };
        let first = Oid::parse_object("11".repeat(20)).unwrap();
        let second = Oid::parse_object("22".repeat(20)).unwrap();
        let author = CommitIdentity {
            name: "Author".into(),
            email: "author@example.invalid".into(),
            timestamp: 0,
            offset_minutes: 0,
        };
        let mut source = SourceControlModel::loading(key.clone());
        source.append_history(&[
            CommitSummary {
                oid: first.clone(),
                parents: vec![],
                author: author.clone(),
                committer: author.clone(),
                subject: "Root".into(),
                body: String::new(),
            },
            CommitSummary {
                oid: second.clone(),
                parents: vec![first.clone()],
                author: author.clone(),
                committer: author,
                subject: "Next".into(),
                body: String::new(),
            },
        ]);
        app.sidebar_model.source_control = Some(source);
        (app, key, first, second)
    }

    #[test]
    fn history_search_without_head_does_not_start_an_unfinishable_page() {
        let (mut app, key, _, _) = fixture();
        let mut source = SourceControlModel::loading(key.clone());
        source.unborn = true;
        app.sidebar_model.source_control = Some(source);
        let generation = app.history_ui.page_generation;
        drop(app.update(Message::Sidebar(SidebarAction::SearchHistory {
            key,
            query: "Author".into(),
        })));
        let source = app.sidebar_model.source_control.as_ref().unwrap();
        assert!(!source.history_loading);
        assert!(!source.history_requested());
        assert!(source.history_query.is_empty());
        assert_eq!(app.history_ui.page_generation, generation);
    }

    #[test]
    fn history_paging_without_head_keeps_the_page_idle() {
        for unborn in [false, true] {
            let (mut app, key, _, _) = fixture();
            let mut source = SourceControlModel::loading(key.clone());
            source.unborn = unborn;
            app.sidebar_model.source_control = Some(source);
            let generation = app.history_ui.page_generation;
            drop(app.update(Message::Sidebar(SidebarAction::LoadMoreHistory {
                key,
                page: 0,
            })));
            let source = app.sidebar_model.source_control.as_ref().unwrap();
            assert!(!source.history_loading);
            assert!(!source.history_requested());
            assert_eq!(source.next_history_page, 0);
            assert_eq!(app.history_ui.page_generation, generation);
        }
    }

    #[test]
    fn switching_commits_cancels_the_old_read_and_rejects_its_completion() {
        let (mut app, key, first, second) = fixture();
        drop(app.open_history_commit(key.clone(), first.clone()));
        let generation = app.history_ui.generation;
        let cancellation = app.history_ui.cancellation.clone().unwrap();
        assert_eq!(
            app.sidebar_model
                .source_control
                .as_ref()
                .unwrap()
                .commit_changes
                .as_ref()
                .unwrap()
                .commit
                .parent,
            None
        );
        drop(app.open_history_commit(key.clone(), second.clone()));
        assert!(cancellation.is_cancelled());
        drop(app.update_commit_ui(Action::Loaded {
            key: key.clone(),
            oid: first.clone(),
            generation,
            result: Ok(vec![]),
        }));
        let changes = app
            .sidebar_model
            .source_control
            .as_ref()
            .unwrap()
            .commit_changes
            .as_ref()
            .unwrap();
        assert_eq!(changes.commit.oid, second);
        assert_eq!(changes.commit.parent, Some(first));
        assert!(changes.result.is_none());
        let generation = app.history_ui.generation;
        drop(app.update_commit_ui(Action::Loaded {
            key,
            oid: second,
            generation,
            result: Ok(vec![]),
        }));
        assert!(matches!(
            app.sidebar_model
                .source_control
                .as_ref()
                .unwrap()
                .commit_changes
                .as_ref()
                .unwrap()
                .result,
            Some(Ok(_))
        ));
    }

    #[test]
    fn superseded_history_searches_and_rebindings_cancel_the_old_worker() {
        let mut state = State::default();
        let old = state.next_page_request();
        let cancelled = state.page_cancellation();
        let current = state.next_page_request();
        assert!(cancelled.is_cancelled());
        assert!(!state.accepts_page(old));
        assert!(state.accepts_page(current));
        let active = state.page_cancellation();
        state.cancel_pages();
        assert!(active.is_cancelled());
        assert!(!state.accepts_page(current));
        state.next_page_request();
        let final_token = state.page_cancellation();
        drop(state);
        assert!(final_token.is_cancelled());
    }

    #[test]
    fn an_abandoned_history_error_cannot_replace_a_new_page_request() {
        let (mut app, key, head, _) = fixture();
        let old = app.history_ui.next_page_request();
        app.history_ui.next_page_request();
        let source = app.sidebar_model.source_control.as_mut().unwrap();
        source.head_oid = Some(head.to_string().into());
        source.mark_history_requested();
        drop(app.update(Message::SourceControlHistory {
            generation: old,
            key,
            page: 0,
            head,
            result: Err("abandoned request".into()),
        }));
        let source = app.sidebar_model.source_control.as_ref().unwrap();
        assert!(source.history_loading);
        assert!(source.history_error.is_none());
    }

    fn history_summary(oid: Oid) -> CommitSummary {
        let identity = CommitIdentity {
            name: "Author".into(),
            email: "author@example.invalid".into(),
            timestamp: 0,
            offset_minutes: 0,
        };
        CommitSummary {
            oid,
            parents: vec![],
            author: identity.clone(),
            committer: identity,
            subject: "Known matching commit".into(),
            body: String::new(),
        }
    }

    fn begin_history_query(app: &mut Cagents, key: &ActiveWorktreeKey, query: &str) -> u64 {
        drop(
            app.update(Message::Sidebar(SidebarAction::HistoryQueryChanged {
                key: key.clone(),
                query: query.into(),
            })),
        );
        drop(app.update(Message::Sidebar(SidebarAction::SearchHistory {
            key: key.clone(),
            query: query.into(),
        })));
        app.history_ui.page_generation
    }

    fn limited(commits: Vec<CommitSummary>) -> ReadFailure {
        RepositoryError::HistorySearchLimit {
            matches: commits,
            failure: None,
        }
        .into()
    }

    #[test]
    fn superseded_history_success_and_limit_cannot_replace_the_current_query() {
        let (mut app, key, head, current) = fixture();
        app.sidebar_model.source_control.as_mut().unwrap().head_oid = Some(head.to_string().into());
        let first = begin_history_query(&mut app, &key, "A");
        let abandoned = app.history_ui.page_cancellation();
        let second = begin_history_query(&mut app, &key, "B");
        assert!(abandoned.is_cancelled());
        assert_ne!(first, second);
        let cancelled =
            ReadFailure::from(RepositoryError::Process(crate::process::process_failure(
                &crate::process::CommandSpec::new("git"),
                crate::process::ProcessFailureKind::Cancelled,
                "superseded history request",
                false,
            )));
        assert!(matches!(&cancelled, ReadFailure::Failed(_)));
        for result in [
            Ok(vec![history_summary(head.clone())]),
            Err(limited(vec![history_summary(head.clone())])),
            Err(cancelled),
        ] {
            drop(app.update(Message::SourceControlHistory {
                generation: first,
                key: key.clone(),
                page: 0,
                head: head.clone(),
                result,
            }));
            let source = app.sidebar_model.source_control.as_ref().unwrap();
            assert_eq!(source.history_query, "B");
            assert!(source.history_loading);
            assert!(source.history.is_empty());
            assert!(source.history_limit.is_none());
            assert!(source.history_error.is_none());
        }
        drop(app.update(Message::SourceControlHistory {
            generation: second,
            key,
            page: 0,
            head,
            result: Ok(vec![history_summary(current.clone())]),
        }));
        let source = app.sidebar_model.source_control.as_ref().unwrap();
        assert_eq!(source.history_query, "B");
        assert_eq!(source.history.len(), 1);
        assert_eq!(source.history[0].oid, current);
        assert!(source.history_complete);
        assert!(source.history_limit.is_none());
    }

    #[test]
    fn limited_history_keeps_known_rows_inspectable_and_refuses_replay_or_more() {
        let (mut app, key, head, found) = fixture();
        app.sidebar_model.source_control.as_mut().unwrap().head_oid = Some(head.to_string().into());
        let generation = begin_history_query(&mut app, &key, found.as_str());
        let token = app.history_ui.page_cancellation();
        let payload = limited(vec![history_summary(found.clone())]);
        drop(app.update(Message::SourceControlHistory {
            generation,
            key: key.clone(),
            page: 0,
            head: head.clone(),
            result: Err(payload.clone()),
        }));
        let source = app.sidebar_model.source_control.as_ref().unwrap();
        let notice = source.history_limit.clone();
        assert!(notice.as_ref().unwrap().contains("more matches may exist"));
        assert!(source.history_loaded);
        assert!(!source.history_loading);
        assert!(!source.history_complete);
        assert_eq!(source.next_history_page, 0);
        assert_eq!(source.history.len(), 1);
        assert_eq!(source.history[0].oid, found);
        drop(app.update(Message::Sidebar(SidebarAction::LoadMoreHistory {
            key: key.clone(),
            page: 0,
        })));
        assert_eq!(app.history_ui.page_generation, generation);
        assert!(!token.is_cancelled());
        for result in [Err(payload), Ok(vec![history_summary(found.clone())])] {
            drop(app.update(Message::SourceControlHistory {
                generation,
                key: key.clone(),
                page: 0,
                head: head.clone(),
                result,
            }));
        }
        let source = app.sidebar_model.source_control.as_ref().unwrap();
        assert_eq!(source.history.len(), 1);
        assert_eq!(source.history_limit, notice);
        // This is the ordinary action reached by the existing selectable row.
        // Its returned Git future is unpolled; the selected retained target
        // and detail ownership must still be installed by the real reducer.
        drop(app.update(Message::Sidebar(SidebarAction::OpenCommit {
            key: key.clone(),
            oid: found.clone(),
        })));
        assert_eq!(
            app.sidebar_model
                .source_control
                .as_ref()
                .unwrap()
                .commit_changes
                .as_ref()
                .unwrap()
                .commit
                .oid,
            found
        );
        let next = begin_history_query(&mut app, &key, "refined");
        assert_ne!(next, generation);
        assert!(token.is_cancelled());
        let source = app.sidebar_model.source_control.as_ref().unwrap();
        assert!(source.history_limit.is_none());
        assert!(source.history_loading);
        assert!(source.history.is_empty());
        assert!(source.commit_changes.is_none());
    }

    #[test]
    fn limited_history_completion_keeps_binding_head_page_and_loading_fences() {
        for wrong in ["binding", "head", "page", "loading"] {
            let (mut app, key, head, found) = fixture();
            app.sidebar_model.source_control.as_mut().unwrap().head_oid =
                Some(head.to_string().into());
            let generation = begin_history_query(&mut app, &key, "current");
            if wrong == "loading" {
                app.sidebar_model
                    .source_control
                    .as_mut()
                    .unwrap()
                    .history_loading = false;
            }
            let mut reply_key = key.clone();
            if wrong == "binding" {
                reply_key.canonical_path = "/another-worktree".into();
            }
            drop(app.update(Message::SourceControlHistory {
                generation,
                key: reply_key,
                page: u32::from(wrong == "page"),
                head: if wrong == "head" { found.clone() } else { head },
                result: Err(limited(vec![history_summary(found)])),
            }));
            let source = app.sidebar_model.source_control.as_ref().unwrap();
            assert!(source.history.is_empty(), "{wrong}");
            assert!(source.history_limit.is_none(), "{wrong}");
            assert!(source.history_error.is_none(), "{wrong}");
            assert_eq!(source.history_loading, wrong != "loading", "{wrong}");
        }
    }

    #[test]
    fn leaving_and_returning_to_the_same_commit_rejects_the_previous_read() {
        let (mut app, key, first, _) = fixture();
        drop(app.open_history_commit(key.clone(), first.clone()));
        let generation = app.history_ui.generation;
        app.history_ui.cancel();
        drop(app.open_history_commit(key.clone(), first.clone()));
        drop(app.update_commit_ui(Action::Loaded {
            key,
            oid: first,
            generation,
            result: Err("obsolete failure".into()),
        }));
        assert!(
            app.sidebar_model
                .source_control
                .as_ref()
                .unwrap()
                .commit_changes
                .as_ref()
                .unwrap()
                .result
                .is_none()
        );
    }
}
