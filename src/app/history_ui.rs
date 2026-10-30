//! Lazy, cancellable inspection of immutable commits from the history list.
use super::*;
use crate::repository::{DiffEntry, Oid};
use crate::sidebar::CommitChanges;

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
