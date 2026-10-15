//! Standalone callers for the shared Git history and commit composer contracts.
use super::*;
use cagents::repository::{CommitSummary, DiffEntry, HistoryRequest, HistoryRevision, Oid};
use std::os::unix::fs::MetadataExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Origin {
    card: CardId,
    key: ActiveWorktreeKey,
    root_identity: (u64, u64),
}

#[derive(Default)]
pub(super) struct State {
    page_generation: u64,
    page_cancel: Option<CancellationToken>,
    commit_generation: u64,
    commit_cancel: Option<CancellationToken>,
    amend_cancel: Option<(u64, CancellationToken)>,
    submitted_revision: Option<u64>,
}
impl State {
    fn next_page(&mut self) -> (u64, CancellationToken) {
        if let Some(cancel) = self.page_cancel.take() { cancel.cancel(); }
        self.page_generation = self.page_generation.wrapping_add(1);
        let cancel = CancellationToken::new();
        self.page_cancel = Some(cancel.clone());
        (self.page_generation, cancel)
    }
    fn next_commit(&mut self) -> (u64, CancellationToken) {
        if let Some(cancel) = self.commit_cancel.take() { cancel.cancel(); }
        self.commit_generation = self.commit_generation.wrapping_add(1);
        let cancel = CancellationToken::new();
        self.commit_cancel = Some(cancel.clone());
        (self.commit_generation, cancel)
    }
}
impl Drop for State {
    fn drop(&mut self) {
        for cancel in [&self.page_cancel, &self.commit_cancel].into_iter().flatten() { cancel.cancel(); }
        if let Some((_, cancel)) = &self.amend_cancel { cancel.cancel(); }
    }
}

#[derive(Debug, Clone)]
pub(super) enum Action {
    AmendLoaded { origin: Origin, revision: u64, head: Oid, result: Result<String, String> },
    HistoryLoaded { origin: Origin, generation: u64, head: Oid, page: u32, query: String, result: Result<Vec<CommitSummary>, String> },
    CommitLoaded { origin: Origin, generation: u64, oid: Oid, result: Result<Vec<DiffEntry>, String> },
    Committed { origin: Origin, revision: u64, result: Result<CommitOutcome, String> },
}

pub(super) fn root_identity(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    metadata.is_dir().then(|| (metadata.dev(), metadata.ino()))
}
fn check_root(origin: &Origin) -> Result<(), String> {
    (root_identity(&origin.key.canonical_path) == Some(origin.root_identity))
        .then_some(()).ok_or_else(|| "The original worktree directory was replaced; reopen it before retrying.".into())
}

impl Editor {
    fn git_review_origin(&self, card: CardId, key: &ActiveWorktreeKey) -> Option<Origin> {
        self.tab(card).filter(|tab| tab.git
            && tab.runtime.descriptor.worktree == key.canonical_path
            && tab.navigation.active_worktree() == Some(key)
            && tab.sidebar.source_control.as_ref().is_some_and(|source| source.key == *key))?;
        Some(Origin { card, key: key.clone(), root_identity: root_identity(&key.canonical_path)? })
    }

    /// Returns Some only for actions owned by this adapter. Operations still
    /// enter the shell's normal Git dispatcher and its per-worktree queue.
    pub(super) fn git_review_sidebar(&mut self, action: &SidebarAction) -> Option<Task<Message>> {
        use sidebar::{ChangesTab, HistoryCommand, PanelSection};
        let key = match action {
            SidebarAction::AmendChanged { key, .. }
            | SidebarAction::Commit { key, .. }
            | SidebarAction::HistoryQueryChanged { key, .. }
            | SidebarAction::SearchHistory { key, .. }
            | SidebarAction::HistoryCommand { key, .. }
            | SidebarAction::HistoryBranchChanged { key, .. }
            | SidebarAction::CreateHistoryBranch { key, .. }
            | SidebarAction::LoadMoreHistory { key, .. }
            | SidebarAction::OpenCommit { key, .. }
            | SidebarAction::CloseCommit { key }
            | SidebarAction::ShowMoreCommitFiles { key, .. }
            | SidebarAction::OpenCommitFile { key, .. }
            | SidebarAction::SetChangesTab { key, tab: ChangesTab::History }
            | SidebarAction::ToggleSection { key, section: PanelSection::History } => Some(key),
            SidebarAction::CancelHistoryBranch => None,
            _ => return None,
        };
        let Some(card) = self.active else { return Some(Task::none()); };
        let origin = match key {
            Some(key) => match self.git_review_origin(card, key) { Some(origin) => Some(origin), None => return Some(Task::none()) },
            None => None,
        };
        if self.modal_open() { return Some(Task::none()); }
        if let SidebarAction::HistoryCommand { oid, mainline, message, .. } = action {
            let current = self.tab(card).and_then(|tab| tab.sidebar.source_control.as_ref()).is_some_and(|source|
                source.history.iter().chain(source.commit_changes.iter().map(|changes| &changes.commit))
                    .any(|row| row.oid == *oid && row.mainline == *mainline && row.message == *message));
            if !current { return Some(Task::none()); }
        }
        if let SidebarAction::CreateHistoryBranch { oid, name, .. } = action {
            if !self.tab(card)?.navigation.history_branch_matches(oid, name) { return Some(Task::none()); }
        }
        self.tab_mut(card)?.navigation.apply(action);
        let Some(origin) = origin else { return Some(Task::none()); };
        Some(match action {
            SidebarAction::AmendChanged { amend, .. } => self.read_amend_message(origin, *amend),
            SidebarAction::Commit { message, amend, .. } => self.commit_review_draft(origin, message.clone(), *amend),
            SidebarAction::SearchHistory { query, .. } => self.read_history_page(origin, 0, Some(query.clone())),
            SidebarAction::LoadMoreHistory { page, .. } => self.read_history_page(origin, *page, None),
            SidebarAction::SetChangesTab { .. } | SidebarAction::ToggleSection { .. } => self.ensure_git_history(card),
            SidebarAction::HistoryCommand { oid, command, mainline, message, .. } => {
                let current = self.tab(card).and_then(|tab| tab.sidebar.source_control.as_ref()).is_some_and(|source|
                    source.history.iter().chain(source.commit_changes.iter().map(|changes| &changes.commit))
                        .any(|row| row.oid == *oid && row.mainline == *mainline && row.message == *message));
                if !current { return Some(Task::none()); }
                match command {
                    HistoryCommand::CopySha => iced::clipboard::write(oid.to_string()).map(cosmic::Action::App),
                    HistoryCommand::CopyMessage => iced::clipboard::write(message.to_string()).map(cosmic::Action::App),
                    HistoryCommand::NewWorktree => { self.begin_commit_checkout(origin.key.canonical_path, oid.clone()); Task::none() },
                    HistoryCommand::CreateBranch => Task::none(),
                    HistoryCommand::Revert | HistoryCommand::CherryPick => {
                        let operation = if *command == HistoryCommand::Revert {
                            cagents::repository::GitOperation::RevertCommit { oid: oid.clone(), mainline: *mainline }
                        } else { cagents::repository::GitOperation::CherryPick { oid: oid.clone(), mainline: *mainline } };
                        self.update(Message::Sidebar(SidebarAction::GitOperation { key: origin.key, operation }))
                    }
                }
            }
            SidebarAction::CreateHistoryBranch { oid, name, .. } => self.update(Message::Sidebar(SidebarAction::GitOperation {
                key: origin.key, operation: cagents::repository::GitOperation::CreateBranch { oid: oid.clone(), name: name.clone() },
            })),
            SidebarAction::OpenCommit { oid, .. } => self.read_history_commit(origin, oid.clone()),
            SidebarAction::CloseCommit { .. } => {
                let tab = self.tab_mut(card).unwrap();
                tab.git_review.next_commit();
                if let Some(source) = tab.sidebar.source_control.as_mut() { source.commit_changes = None; }
                Task::none()
            }
            SidebarAction::ShowMoreCommitFiles { oid, .. } => {
                if let Some(changes) = self.tab_mut(card).and_then(|tab| tab.sidebar.source_control.as_mut())
                    .and_then(|source| source.commit_changes.as_mut()).filter(|changes| changes.commit.oid == *oid) {
                    changes.visible_files = changes.visible_files.saturating_add(200);
                }
                Task::none()
            }
            SidebarAction::OpenCommitFile { oid, path, .. } => self.open_history_file(origin, oid.clone(), path.clone()),
            _ => Task::none(),
        })
    }

    fn read_amend_message(&mut self, origin: Origin, amend: bool) -> Task<Message> {
        let tab = self.tab_mut(origin.card).unwrap();
        if let Some((_, cancel)) = tab.git_review.amend_cancel.take() { cancel.cancel(); }
        if !amend { return Task::none(); }
        let revision = match tab.navigation.amend_request_revision() { Some(revision) => revision, None => return Task::none() };
        let source = tab.sidebar.source_control.as_mut().unwrap();
        if source.operation.is_some() {
            tab.navigation.apply(&SidebarAction::AmendChanged { key: origin.key.clone(), amend: false });
            source.set_error(Some("Finish or abort the current Git operation before amending.".into()));
            return Task::none();
        }
        let Some(head) = source.head_oid.as_deref().and_then(|head| Oid::parse_object(head).ok()) else {
            tab.navigation.finish_amend_read(&origin.key, revision);
            return Task::none();
        };
        tab.navigation.pin_amend_head(&origin.key, revision, head.clone());
        let cancel = CancellationToken::new();
        tab.git_review.amend_cancel = Some((revision, cancel.clone()));
        cosmic::task::future(async move {
            let root = origin.key.canonical_path.clone(); let target = head.clone();
            let authority = origin.clone();
            let result = tokio::task::spawn_blocking(move || { check_root(&authority)?; RepositoryService::system("git").commit_message(&root, &target, &cancel).map_err(|error| error.to_string()) })
                .await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::GitReview(Action::AmendLoaded { origin, revision, head, result })
        })
    }

    pub(super) fn ensure_git_history(&mut self, card: CardId) -> Task<Message> {
        let Some(tab) = self.tab(card) else { return Task::none(); };
        if tab.navigation.changes_tab() != sidebar::ChangesTab::History { return Task::none(); }
        let Some(source) = tab.sidebar.source_control.as_ref().filter(|source| source.wants_history() && !source.history_requested()) else { return Task::none(); };
        let Some(origin) = self.git_review_origin(card, &source.key) else { return Task::none(); };
        self.read_history_page(origin, 0, None)
    }

    fn read_history_page(&mut self, origin: Origin, page: u32, query: Option<String>) -> Task<Message> {
        let tab = self.tab_mut(origin.card).unwrap();
        let source = tab.sidebar.source_control.as_mut().unwrap();
        let Some(head) = source.head_oid.as_deref().and_then(|head| Oid::parse_object(head).ok()) else { return Task::none(); };
        if let Some(query) = query { source.search_history(query); }
        else {
            if source.history_loading || source.history_complete || source.next_history_page != page { return Task::none(); }
            source.mark_history_requested();
        }
        let query = source.history_query.clone();
        let (generation, cancel) = tab.git_review.next_page();
        cosmic::task::future(async move {
            let request = HistoryRequest { worktree: origin.key.canonical_path.clone(), revision: HistoryRevision::Object { oid: head.clone() }, page, path: None };
            let filter = query.clone();
            let authority = origin.clone();
            let result = tokio::task::spawn_blocking(move || { check_root(&authority)?; RepositoryService::system("git").filtered_history(&request, &filter, &cancel).map_err(|error| error.to_string()) })
                .await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::GitReview(Action::HistoryLoaded { origin, generation, head, page, query, result })
        })
    }

    fn read_history_commit(&mut self, origin: Origin, oid: Oid) -> Task<Message> {
        let tab = self.tab_mut(origin.card).unwrap();
        let source = tab.sidebar.source_control.as_mut().unwrap();
        let Some(commit) = source.history.iter().find(|commit| commit.oid == oid).cloned() else { return Task::none(); };
        let parent = commit.parent.clone();
        source.commit_changes = Some(sidebar::CommitChanges { commit, visible_files: 200, result: None });
        let (generation, cancel) = tab.git_review.next_commit();
        cosmic::task::future(async move {
            let root = origin.key.canonical_path.clone(); let target = oid.clone();
            let authority = origin.clone();
            let result = tokio::task::spawn_blocking(move || { check_root(&authority)?; RepositoryService::system("git").commit_diff_entries(&root, &target, parent.as_ref(), &cancel).map_err(|error| error.to_string()) })
                .await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::GitReview(Action::CommitLoaded { origin, generation, oid, result })
        })
    }

    fn open_history_file(&mut self, origin: Origin, oid: Oid, path: RepoPath) -> Task<Message> {
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        let tab = self.tab_mut(origin.card).unwrap();
        if tab.runtime.blocks_navigation() { self.notice = Some("Save or discard the current file before opening a commit.".into()); return Task::none(); }
        let Some(changes) = tab.sidebar.source_control.as_ref().and_then(|source| source.commit_changes.as_ref()).filter(|changes| changes.commit.oid == oid) else { return Task::none(); };
        let Some(entry) = changes.result.as_ref().and_then(|result| result.as_ref().ok()).and_then(|entries| entries.iter().find(|entry| entry.path == path)) else { return Task::none(); };
        let basis = DiffBasis::CommitToParent { oid, parent: changes.commit.parent.clone() };
        let old_path = entry.old_path.clone();
        tab.title = path.as_path().file_name().unwrap_or(path.as_path().as_os_str()).to_string_lossy().into_owned();
        tab.owned_file = Some(tab.runtime.descriptor.worktree.join(path.as_path()));
        tab.pending_file = None; tab.pending_fragment = None;
        repository_task(tab.runtime.update(RepositoryAction::OpenCommittedFile { card: origin.card, path, old_path, basis }))
    }

    fn commit_review_draft(&mut self, origin: Origin, message: String, amend: bool) -> Task<Message> {
        let tab = self.tab(origin.card).unwrap();
        let Some(revision) = tab.navigation.submitted_commit_revision(&origin.key, &message, amend) else { return Task::none(); };
        let expected = tab.navigation.amend_target();
        if amend && (expected.is_none() || tab.sidebar.source_control.as_ref().is_some_and(|source| source.operation.is_some())) {
            self.notice = Some("Load HEAD's message and finish the current Git operation before amending.".into()); return Task::none();
        }
        let Some(worktree) = self.begin_mutation(origin.card, MutationKind::Commit) else { return Task::none(); };
        self.tab_mut(origin.card).unwrap().git_review.submitted_revision = Some(revision);
        cosmic::task::future(async move {
            let request = CommitRequest { worktree, message };
            let authority = origin.clone();
            let result = tokio::task::spawn_blocking(move || {
                check_root(&authority)?;
                let service = RepositoryService::system("git"); let cancel = CancellationToken::new();
                match expected { Some(head) if amend => service.commit_amend_at(&request, &head, &cancel), _ => service.commit_with_amend(&request, amend, &cancel) }.map_err(|error| error.to_string())
            }).await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::GitReview(Action::Committed { origin, revision, result })
        })
    }

    pub(super) fn git_review_action(&mut self, mut action: Action) -> Task<Message> {
        let origin = match &action { Action::AmendLoaded { origin, .. } | Action::HistoryLoaded { origin, .. } | Action::CommitLoaded { origin, .. } | Action::Committed { origin, .. } => origin };
        let Some(current) = self.git_review_origin(origin.card, &origin.key) else { return Task::none(); };
        if current != *origin {
            let error = "The original worktree directory was replaced; reopen it before retrying.".to_owned();
            match &mut action {
                Action::AmendLoaded { result, .. } => *result = Err(error),
                Action::HistoryLoaded { result, .. } => *result = Err(error),
                Action::CommitLoaded { result, .. } => *result = Err(error),
                Action::Committed { result, .. } => *result = Err(error),
            }
        }
        match action {
            Action::AmendLoaded { origin, revision, head, result } => {
                let tab = self.tab_mut(origin.card).unwrap();
                if tab.git_review.amend_cancel.as_ref().map(|(pending, _)| *pending) != Some(revision) { return Task::none(); }
                tab.git_review.amend_cancel = None;
                let source = tab.sidebar.source_control.as_mut().unwrap();
                if source.head_oid.as_deref() != Some(head.as_str()) {
                    if tab.navigation.finish_amend_read(&origin.key, revision) { source.set_error(Some("HEAD changed while loading its message. Toggle Amend again.".into())); }
                } else { match result {
                    Ok(message) => { tab.navigation.load_amend_message(&origin.key, revision, &message); },
                    Err(error) => if tab.navigation.finish_amend_read(&origin.key, revision) { source.set_error(Some(format!("Could not load HEAD's message: {error}"))); },
                } }
                Task::none()
            }
            Action::HistoryLoaded { origin, generation, head, page, query, result } => {
                let tab = self.tab_mut(origin.card).unwrap();
                let source = tab.sidebar.source_control.as_mut().unwrap();
                if tab.git_review.page_cancel.is_none() || tab.git_review.page_generation != generation || source.head_oid.as_deref() != Some(head.as_str()) || source.next_history_page != page || source.history_query != query { return Task::none(); }
                tab.git_review.page_cancel = None;
                match result { Ok(commits) => source.append_history(&commits), Err(error) => source.history_failed(error) }
                Task::none()
            }
            Action::CommitLoaded { origin, generation, oid, result } => {
                let tab = self.tab_mut(origin.card).unwrap();
                if tab.git_review.commit_cancel.is_none() || tab.git_review.commit_generation != generation { return Task::none(); }
                tab.git_review.commit_cancel = None;
                if let Some(changes) = tab.sidebar.source_control.as_mut().and_then(|source| source.commit_changes.as_mut()).filter(|changes| changes.commit.oid == oid) { changes.result = Some(result); }
                Task::none()
            }
            Action::Committed { origin, revision, result } => {
                let tab = self.tab_mut(origin.card).unwrap();
                if tab.git_review.submitted_revision != Some(revision) { return Task::none(); }
                tab.git_review.submitted_revision = None;
                let notice = match result {
                    Ok(CommitOutcome::Created { warnings, .. }) => {
                        self.tab_mut(origin.card).unwrap().navigation.clear_submitted_commit(&origin.key, revision);
                        (!warnings.is_empty()).then(|| warnings.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))
                    },
                    Ok(CommitOutcome::NotCreated { failure }) => Some(format!("Commit failed: {failure}")),
                    Ok(CommitOutcome::OutcomeUnknown { failure, .. }) => Some(format!("Commit outcome is uncertain: {failure}")),
                    Err(error) => Some(error),
                };
                for tab in &mut self.tabs { if tab.runtime.descriptor.worktree == origin.key.canonical_path { if let Some(source) = tab.sidebar.source_control.as_mut() { source.set_error(notice.clone()); } } }
                let prior = self.notice.clone();
                let task = self.finish_mutation(origin.card, notice);
                if self.active != Some(origin.card) { self.notice = prior; }
                task
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::tests::{editor, git, git_fixture, open_real_git, run_tasks, sidebar_action};

    fn bound(app: &Editor, card: CardId) -> Origin {
        let key = app.tab(card).unwrap().navigation.active_worktree().unwrap();
        app.git_review_origin(card, key).unwrap()
    }

    #[test]
    fn standalone_amend_loads_full_message_restores_draft_and_commits_exact_loaded_bytes() {
        let directory = git_fixture(); let root = directory.path();
        let message = "Original subject\n\nBody paragraph\n\n# literal message line\n";
        git(root, &["commit", "--amend", "--cleanup=verbatim", "-m", message]);
        let expected = git(root, &["log", "-1", "--format=%B"]);
        let mut app = editor(); let card = open_real_git(&mut app, root, None); let origin = bound(&app, card);
        sidebar_action(&mut app, SidebarAction::CommitMessageChanged { key: origin.key.clone(), message: "Previous subject\n\nPrevious body".into() });
        sidebar_action(&mut app, SidebarAction::AmendChanged { key: origin.key.clone(), amend: true });
        assert_eq!(app.tab(card).unwrap().navigation.commit_message().trim_end(), expected);
        sidebar_action(&mut app, SidebarAction::AmendChanged { key: origin.key.clone(), amend: false });
        assert_eq!(app.tab(card).unwrap().navigation.commit_message(), "Previous subject\n\nPrevious body");
        sidebar_action(&mut app, SidebarAction::AmendChanged { key: origin.key.clone(), amend: true });
        let loaded = app.tab(card).unwrap().navigation.commit_message().to_owned();
        std::fs::write(root.join("tracked.txt"), "amended contents\n").unwrap();
        sidebar_action(&mut app, SidebarAction::Stage { key: origin.key.clone(), selection: vec![PathSelection::new(RepoPath::parse("tracked.txt").unwrap())] });
        sidebar_action(&mut app, SidebarAction::Commit { key: origin.key, message: loaded, amend: true });
        assert!(app.notice.is_none(), "{:?}", app.notice);
        assert_eq!(git(root, &["log", "-1", "--format=%B"]), expected);
        assert_eq!(git(root, &["rev-list", "--count", "HEAD"]), "1");
        assert_eq!(git(root, &["show", "HEAD:tracked.txt"]), "amended contents");
    }

    #[test]
    fn standalone_amend_late_callbacks_respect_typing_off_on_modal_and_background_tab() {
        let first = git_fixture(); let second = git_fixture();
        let mut app = editor(); let card = open_real_git(&mut app, first.path(), None); let origin = bound(&app, card);
        let task = app.update(Message::Sidebar(SidebarAction::AmendChanged { key: origin.key.clone(), amend: true }));
        let stale = app.tab(card).unwrap().navigation.amend_request_revision().unwrap();
        sidebar_action(&mut app, SidebarAction::CommitMessageChanged { key: origin.key.clone(), message: "Typed while loading".into() });
        run_tasks(&mut app, task);
        assert_eq!(app.tab(card).unwrap().navigation.commit_message(), "Typed while loading");
        sidebar_action(&mut app, SidebarAction::AmendChanged { key: origin.key.clone(), amend: false });
        let task = app.update(Message::Sidebar(SidebarAction::AmendChanged { key: origin.key.clone(), amend: true }));
        let current = app.tab(card).unwrap().navigation.amend_request_revision().unwrap();
        assert_ne!(current, stale);
        let head = Oid::parse_object(git(first.path(), &["rev-parse", "HEAD"])).unwrap();
        drop(app.update(Message::GitReview(Action::AmendLoaded { origin: origin.clone(), revision: stale, head, result: Ok("Obsolete".into()) })));
        assert_eq!(app.tab(card).unwrap().git_review.amend_cancel.as_ref().map(|(revision, _)| *revision), Some(current));
        let other = open_real_git(&mut app, second.path(), None);
        app.settings_open = true;
        run_tasks(&mut app, task);
        assert_eq!(app.active, Some(other));
        assert_eq!(app.tab(card).unwrap().navigation.commit_message().trim_end(), "Initial");
        assert!(app.tab(other).unwrap().navigation.commit_message().is_empty());
        assert!(app.settings_open);
    }

    #[test]
    fn standalone_history_search_and_captured_callbacks_use_exact_query_head_and_generation() {
        let directory = git_fixture(); let root = directory.path();
        git(root, &["commit", "--allow-empty", "-m", "Needle subject\n\nUnique body marker"]);
        let mut app = editor(); let card = open_real_git(&mut app, root, None); let origin = bound(&app, card);
        sidebar_action(&mut app, SidebarAction::SetChangesTab { key: origin.key.clone(), tab: sidebar::ChangesTab::History });
        assert_eq!(app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap().history.len(), 2);
        sidebar_action(&mut app, SidebarAction::SearchHistory { key: origin.key.clone(), query: "UNIQUE BODY MARKER".into() });
        let source = app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap();
        assert_eq!(source.history.len(), 1);
        let oid = source.history[0].oid.clone();
        let old = app.tab(card).unwrap().git_review.page_generation;
        let task = app.update(Message::Sidebar(SidebarAction::SearchHistory { key: origin.key.clone(), query: "Test".into() }));
        drop(app.update(Message::GitReview(Action::HistoryLoaded { origin: origin.clone(), generation: old, head: oid.clone(), page: 0, query: "UNIQUE BODY MARKER".into(), result: Ok(Vec::new()) })));
        assert!(app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap().history_loading);
        run_tasks(&mut app, task);
        assert_eq!(app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap().history.len(), 2);
        sidebar_action(&mut app, SidebarAction::SearchHistory { key: origin.key, query: oid.as_str()[..10].into() });
        assert_eq!(app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap().history[0].oid, oid);
    }

    #[test]
    fn standalone_history_menu_refuses_forged_rows_and_branch_form_retargets() {
        use sidebar::HistoryCommand;
        let directory = git_fixture(); let root = directory.path();
        let mut app = editor(); let card = open_real_git(&mut app, root, None); let origin = bound(&app, card);
        sidebar_action(&mut app, SidebarAction::SetChangesTab { key: origin.key.clone(), tab: sidebar::ChangesTab::History });
        let row = app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap().history[0].clone();
        sidebar_action(&mut app, SidebarAction::HistoryCommand { key: origin.key.clone(), oid: row.oid.clone(), mainline: row.mainline, message: "Forged".into(), command: HistoryCommand::CreateBranch });
        let name = cagents::repository::ValidatedBranchName::parse("from-history").unwrap();
        assert!(!app.tab(card).unwrap().navigation.history_branch_matches(&row.oid, &name));
        sidebar_action(&mut app, SidebarAction::HistoryCommand { key: origin.key.clone(), oid: row.oid.clone(), mainline: row.mainline, message: row.message.clone(), command: HistoryCommand::CreateBranch });
        sidebar_action(&mut app, SidebarAction::HistoryBranchChanged { key: origin.key.clone(), name: "different-name".into() });
        sidebar_action(&mut app, SidebarAction::CreateHistoryBranch { key: origin.key.clone(), oid: row.oid.clone(), name: name.clone() });
        assert!(git(root, &["for-each-ref", "--format=%(refname)", "refs/heads/from-history"]).is_empty());
        sidebar_action(&mut app, SidebarAction::HistoryBranchChanged { key: origin.key.clone(), name: name.as_str().into() });
        sidebar_action(&mut app, SidebarAction::CreateHistoryBranch { key: origin.key, oid: row.oid.clone(), name });
        assert_eq!(git(root, &["rev-parse", "from-history"]), row.oid.as_str());
    }

    #[test]
    fn standalone_history_commit_files_open_the_actual_committed_snapshot() {
        let directory = git_fixture(); let root = directory.path();
        let mut app = editor(); let card = open_real_git(&mut app, root, None); let origin = bound(&app, card);
        sidebar_action(&mut app, SidebarAction::SetChangesTab { key: origin.key.clone(), tab: sidebar::ChangesTab::History });
        let oid = app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap().history[0].oid.clone();
        sidebar_action(&mut app, SidebarAction::OpenCommit { key: origin.key.clone(), oid: oid.clone() });
        let path = RepoPath::parse("tracked.txt").unwrap();
        assert!(app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap().commit_changes.as_ref().unwrap().result.as_ref().unwrap().as_ref().unwrap().iter().any(|entry| entry.path == path));
        let epoch = app.selection_epoch;
        sidebar_action(&mut app, SidebarAction::OpenCommitFile { key: origin.key, oid, path });
        assert!(app.selection_epoch > epoch);
        assert!(app.tab(card).unwrap().runtime.shows_diff());
        assert_eq!(app.tab(card).unwrap().owned_file.as_deref(), Some(root.join("tracked.txt").as_path()));
        assert_eq!(app.tab(card).unwrap().title, "tracked.txt");
    }

    #[test]
    fn standalone_commit_worker_refuses_replaced_physical_worktree_before_effects() {
        let directory = git_fixture(); let root = directory.path();
        let parked = tempfile::tempdir().unwrap(); let replacement = git_fixture();
        let mut app = editor(); let card = open_real_git(&mut app, root, None); let origin = bound(&app, card);
        std::fs::write(root.join("tracked.txt"), "original pending commit\n").unwrap();
        sidebar_action(&mut app, SidebarAction::Stage { key: origin.key.clone(), selection: vec![PathSelection::new(RepoPath::parse("tracked.txt").unwrap())] });
        sidebar_action(&mut app, SidebarAction::CommitMessageChanged { key: origin.key.clone(), message: "Must not reach replacement".into() });
        let task = app.update(Message::Sidebar(SidebarAction::Commit { key: origin.key, message: "Must not reach replacement".into(), amend: false }));
        std::fs::rename(root, parked.path().join("original")).unwrap();
        std::fs::rename(replacement.path(), root).unwrap();
        let before = git(root, &["rev-parse", "HEAD"]);
        run_tasks(&mut app, task);
        assert_eq!(git(root, &["rev-parse", "HEAD"]), before);
        assert_eq!(git(root, &["rev-list", "--count", "HEAD"]), "1");
        assert!(app.notice.as_deref().is_some_and(|notice| notice.contains("replaced")));
        assert!(app.tab(card).unwrap().mutation.is_none());
        assert_eq!(git(&parked.path().join("original"), &["diff", "--cached", "--name-only"]), "tracked.txt");
    }

    #[test]
    fn standalone_commit_completion_preserves_newer_equal_draft_and_refuses_duplicate_release() {
        let directory = git_fixture(); let root = directory.path();
        let mut app = editor(); let card = open_real_git(&mut app, root, None); let origin = bound(&app, card);
        sidebar_action(&mut app, SidebarAction::CommitMessageChanged { key: origin.key.clone(), message: "same message".into() });
        let revision = app.tab(card).unwrap().navigation.submitted_commit_revision(&origin.key, "same message", false).unwrap();
        assert!(app.begin_mutation(card, MutationKind::Commit).is_some());
        app.tab_mut(card).unwrap().git_review.submitted_revision = Some(revision);
        sidebar_action(&mut app, SidebarAction::CommitMessageChanged { key: origin.key.clone(), message: "newer draft".into() });
        sidebar_action(&mut app, SidebarAction::CommitMessageChanged { key: origin.key.clone(), message: "same message".into() });
        let head = Oid::parse_object(git(root, &["rev-parse", "HEAD"])).unwrap();
        let result = CommitOutcome::Created { oid: head, warnings: Vec::new() };
        let task = app.update(Message::GitReview(Action::Committed { origin: origin.clone(), revision, result: Ok(result.clone()) }));
        run_tasks(&mut app, task);
        assert_eq!(app.tab(card).unwrap().navigation.commit_message(), "same message");
        assert!(app.begin_mutation(card, MutationKind::Commit).is_some());
        app.tab_mut(card).unwrap().git_review.submitted_revision = Some(revision.wrapping_add(1));
        drop(app.update(Message::GitReview(Action::Committed { origin, revision, result: Ok(result) })));
        assert_eq!(app.tab(card).unwrap().mutation, Some(MutationKind::Commit));
    }
}
