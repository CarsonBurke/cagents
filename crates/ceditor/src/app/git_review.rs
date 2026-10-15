//! Standalone callers for the shared Git history and commit composer contracts.
use super::*;
use cagents::repository::{CommitSummary, DiffEntry, HistoryRequest, HistoryRevision, Oid};

#[derive(Debug, Clone)]
pub(super) struct Origin {
    card: CardId,
    key: ActiveWorktreeKey,
    authority: Option<WorktreeAuthority>,
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



// Portable read-only Git remains available where retained-descriptor mutations
// are unsupported. On supported platforms, a missing captured authority is an
// error rather than permission to recapture a possibly replaced pathname.
macro_rules! read_git {
    ($authority:expr, $method:ident($($argument:expr),* $(,)?)) => {{
        let service = RepositoryService::system("git");
        match $authority {
            Some(authority) => service.with_worktree_authority(authority).map_err(|error| error.to_string())?
                .$method($($argument),*).map_err(|error| error.to_string()),
            None if !WorktreeAuthority::supported() => service.$method($($argument),*).map_err(|error| error.to_string()),
            None => Err("The original worktree could not be retained. Reopen it before reading Git history.".to_owned()),
        }
    }};
}

impl Editor {
    fn git_review_bound(&self, card: CardId, key: &ActiveWorktreeKey) -> bool {
        self.tab(card).is_some_and(|tab| tab.git
            && tab.runtime.descriptor.worktree == key.canonical_path
            && tab.navigation.active_worktree() == Some(key)
            && tab.sidebar.source_control.as_ref().is_some_and(|source| source.key == *key))
    }

    fn git_review_origin(&self, card: CardId, key: &ActiveWorktreeKey) -> Option<Origin> {
        self.git_review_bound(card, key).then_some(())?;
        Some(Origin { card, key: key.clone(), authority: self.tab(card)?.worktree_authority.clone() })
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
        if self.modal_open() { return Some(Task::none()); }
        // Draft-only UI edits remain available when the directory disappears,
        // and typing a filter never performs a filesystem metadata read.
        if matches!(action, SidebarAction::HistoryQueryChanged { .. } | SidebarAction::HistoryBranchChanged { .. } | SidebarAction::CancelHistoryBranch | SidebarAction::AmendChanged { amend: false, .. }) {
            if key.is_some_and(|key| !self.git_review_bound(card, key)) { return Some(Task::none()); }
            let tab = self.tab_mut(card)?;
            tab.navigation.apply(action);
            if matches!(action, SidebarAction::AmendChanged { amend: false, .. }) {
                if let Some((_, cancel)) = tab.git_review.amend_cancel.take() { cancel.cancel(); }
            }
            return Some(Task::none());
        }
        let origin = match key {
            Some(key) => match self.git_review_origin(card, key) { Some(origin) => Some(origin), None => return Some(Task::none()) },
            None => None,
        };
        if let SidebarAction::HistoryCommand { oid, mainline, message, .. } = action {
            let current = self.tab(card).and_then(|tab| tab.sidebar.source_control.as_ref()).is_some_and(|source|
                source.history.iter().chain(source.commit_changes.iter().map(|changes| &changes.commit))
                    .any(|row| row.oid == *oid && row.mainline == *mainline && row.message == *message));
            if !current { return Some(Task::none()); }
        }
        if let SidebarAction::CreateHistoryBranch { oid, name, .. } = action {
            if !self.tab(card)?.navigation.history_branch_matches(oid, name) { return Some(Task::none()); }
        }
        let mutation_requested = matches!(action,
            SidebarAction::Commit { .. } | SidebarAction::CreateHistoryBranch { .. }
            | SidebarAction::HistoryCommand { command: HistoryCommand::NewWorktree | HistoryCommand::CreateBranch | HistoryCommand::Revert | HistoryCommand::CherryPick, .. });
        if mutation_requested && origin.as_ref().is_some_and(|origin| origin.authority.is_none()) {
            self.notice = Some(if WorktreeAuthority::supported() {
                "The original worktree could not be retained. Reopen it before changing Git state."
            } else {
                "Git mutations require retained directory authority, which this platform does not support yet. Read-only history and files remain available."
            }.into());
            return Some(Task::none());
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
                    HistoryCommand::NewWorktree => { self.begin_commit_checkout(origin.authority.expect("mutation admission requires retained authority"), oid.clone()); Task::none() },
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
            let result = tokio::task::spawn_blocking(move || { read_git!(authority.authority, commit_message(&root, &target, &cancel)) })
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
            let result = tokio::task::spawn_blocking(move || { read_git!(authority.authority, filtered_history(&request, &filter, &cancel)) })
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
            let result = tokio::task::spawn_blocking(move || { read_git!(authority.authority, commit_diff_entries(&root, &target, parent.as_ref(), &cancel)) })
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
        // An immutable commit view does not own the live working file. Live
        // open/dedup must still mount its current editable document.
        tab.owned_file = None;
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
        let cancel = self.mutation_cancellation(origin.card).expect("admitted commit has cancellation");
        cosmic::task::future(async move {
            let request = CommitRequest { worktree, message };
            let authority = origin.clone();
            let result = tokio::task::spawn_blocking(move || {
                let service = RepositoryService::system("git").with_worktree_authority(authority.authority.expect("mutation admission requires retained authority")).map_err(|error| error.to_string())?;
                match expected { Some(head) if amend => service.commit_amend_at(&request, &head, &cancel), _ => service.commit_with_amend(&request, amend, &cancel) }.map_err(|error| error.to_string())
            }).await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::GitReview(Action::Committed { origin, revision, result })
        })
    }

    pub(super) fn git_review_action(&mut self, action: Action) -> Task<Message> {
        let origin = match &action { Action::AmendLoaded { origin, .. } | Action::HistoryLoaded { origin, .. } | Action::CommitLoaded { origin, .. } | Action::Committed { origin, .. } => origin };
        // Physical disappearance refuses effects, but must still settle the
        // exact logical request that reserved this tab's mutation/read slot.
        if !self.git_review_bound(origin.card, &origin.key) { return Task::none(); }
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
                if tab.git_review.page_cancel.is_none() || tab.git_review.page_generation != generation { return Task::none(); }
                tab.git_review.page_cancel = None;
                if source.head_oid.as_deref() != Some(head.as_str()) || source.next_history_page != page || source.history_query != query {
                    source.history_failed("History changed while loading. Search again to read the current worktree.".into());
                    return Task::none();
                }
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
    use super::super::tests::{editor, git, git_output, git_fixture, open_real_git, run_tasks, sidebar_action};

    fn bound(app: &Editor, card: CardId) -> Origin {
        let key = app.tab(card).unwrap().navigation.active_worktree().unwrap();
        app.git_review_origin(card, key).unwrap()
    }

    #[test]
    fn standalone_missing_authority_never_silently_admits_mutation_or_recaptures_linux_history() {
        let directory = git_fixture(); let root = directory.path();
        let mut app = editor(); let card = open_real_git(&mut app, root, None); let origin = bound(&app, card);
        sidebar_action(&mut app, SidebarAction::SetChangesTab { key: origin.key.clone(), tab: sidebar::ChangesTab::History });
        let row = app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap().history[0].clone();
        app.tab_mut(card).unwrap().worktree_authority = None;
        let original_head = git(root, &["rev-parse", "HEAD"]);
        for command in [sidebar::HistoryCommand::CreateBranch, sidebar::HistoryCommand::Revert, sidebar::HistoryCommand::CherryPick, sidebar::HistoryCommand::NewWorktree] {
            sidebar_action(&mut app, SidebarAction::HistoryCommand { key: origin.key.clone(), oid: row.oid.clone(), mainline: row.mainline, message: row.message.clone(), command });
            assert!(app.notice.as_deref().is_some_and(|notice| notice.contains("retained")));
            assert!(app.checkout_dialog.is_none());
            assert_eq!(git(root, &["rev-parse", "HEAD"]), original_head);
        }
        sidebar_action(&mut app, SidebarAction::CommitMessageChanged { key: origin.key.clone(), message: "Keep unsupported draft".into() });
        sidebar_action(&mut app, SidebarAction::Commit { key: origin.key.clone(), message: "Keep unsupported draft".into(), amend: false });
        assert!(app.tab(card).unwrap().mutation.is_none());
        assert_eq!(app.tab(card).unwrap().navigation.commit_message(), "Keep unsupported draft");
        sidebar_action(&mut app, SidebarAction::SearchHistory { key: origin.key, query: "Initial".into() });
        let source = app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap();
        if WorktreeAuthority::supported() {
            assert!(source.history_error.as_deref().is_some_and(|error| error.contains("retained")));
        } else {
            assert_eq!(source.history.len(), 1);
            assert!(source.history_error.is_none());
        }
    }

    #[test]
    fn standalone_amend_loads_full_message_restores_draft_and_commits_exact_loaded_bytes() {
        let directory = git_fixture(); let root = directory.path();
        let message = "Original subject\n\nBody paragraph\n\n# literal message line\n";
        git(root, &["commit", "--amend", "--cleanup=verbatim", "-m", message]);
        let expected = git(root, &["log", "-1", "--format=%B"]);
        let original_commit = git_output(root, &["cat-file", "commit", "HEAD"]).stdout;
        let original_message = original_commit[original_commit.windows(2).position(|pair| pair == b"\n\n").unwrap() + 2..].to_vec();
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
        let amended_commit = git_output(root, &["cat-file", "commit", "HEAD"]).stdout;
        let amended_message = &amended_commit[amended_commit.windows(2).position(|pair| pair == b"\n\n").unwrap() + 2..];
        assert_eq!(amended_message, original_message, "untouched subject/body bytes survive the actual caller");
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
    fn standalone_history_changed_head_settles_its_old_read_without_installing_results() {
        let directory = git_fixture(); let mut app = editor();
        let card = open_real_git(&mut app, directory.path(), None); let origin = bound(&app, card);
        let task = app.update(Message::Sidebar(SidebarAction::SearchHistory { key: origin.key, query: "Initial".into() }));
        app.tab_mut(card).unwrap().sidebar.source_control.as_mut().unwrap().head_oid = Some("f".repeat(40));
        run_tasks(&mut app, task);
        let source = app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap();
        assert!(source.history.is_empty());
        assert!(!source.history_loading);
        assert!(source.history_error.as_deref().is_some_and(|error| error.contains("History changed")));
        assert!(app.tab(card).unwrap().git_review.page_cancel.is_none());
    }

    #[test]
    fn standalone_history_search_finds_author_body_and_sha_beyond_first_page() {
        let directory = git_fixture(); let root = directory.path();
        git(root, &["config", "user.name", "Beyond Page Author"]);
        git(root, &["commit", "--amend", "--reset-author", "-m", "Old subject\n\nHidden body marker"]);
        let oldest = git(root, &["rev-parse", "HEAD"]);
        let tree = git(root, &["rev-parse", "HEAD^{tree}"]);
        git(root, &["config", "user.name", "Recent Author"]);
        let mut head = oldest.clone();
        for index in 0..201 { head = git(root, &["commit-tree", &tree, "-p", &head, "-m", &format!("Recent {index}")]); }
        git(root, &["update-ref", "HEAD", &head]);
        let mut app = editor(); let card = open_real_git(&mut app, root, None); let origin = bound(&app, card);
        sidebar_action(&mut app, SidebarAction::SetChangesTab { key: origin.key.clone(), tab: sidebar::ChangesTab::History });
        let source = app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap();
        assert_eq!(source.history.len(), 200);
        assert!(!source.history.iter().any(|row| row.oid.as_str() == oldest));
        for query in ["BEYOND PAGE AUTHOR".to_owned(), "hidden body marker".to_owned(), oldest[..12].to_owned()] {
            sidebar_action(&mut app, SidebarAction::SearchHistory { key: origin.key.clone(), query });
            let rows = &app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap().history;
            assert_eq!(rows.len(), 1); assert_eq!(rows[0].oid.as_str(), oldest);
        }
    }

    #[test]
    fn standalone_history_copy_actions_emit_exact_clipboard_payloads() {
        use cosmic::iced::futures::StreamExt;
        let directory = git_fixture(); let mut app = editor();
        let card = open_real_git(&mut app, directory.path(), None); let origin = bound(&app, card);
        sidebar_action(&mut app, SidebarAction::SetChangesTab { key: origin.key.clone(), tab: sidebar::ChangesTab::History });
        let row = app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap().history[0].clone();
        for (command, expected) in [(sidebar::HistoryCommand::CopySha, row.oid.to_string()), (sidebar::HistoryCommand::CopyMessage, row.message.to_string())] {
            let task = app.update(Message::Sidebar(SidebarAction::HistoryCommand { key: origin.key.clone(), oid: row.oid.clone(), mainline: row.mainline, message: row.message.clone(), command }));
            let writes = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let mut writes = Vec::new(); let mut stream = iced::runtime::task::into_stream(task).expect("clipboard task");
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Clipboard(iced::runtime::clipboard::Action::Write { contents, .. }) = action { writes.push(contents); }
                }
                writes
            });
            assert_eq!(writes, [expected]);
        }
    }

    #[test]
    fn standalone_history_revert_and_cross_worktree_cherry_pick_use_actual_mutation_path() {
        let directory = git_fixture(); let root = directory.path(); let side = tempfile::tempdir().unwrap();
        git(root, &["branch", "history-target"]);
        std::fs::write(root.join("tracked.txt"), "picked contents\n").unwrap();
        git(root, &["add", "tracked.txt"]); git(root, &["commit", "-m", "Change to replay"]);
        let picked = git(root, &["rev-parse", "HEAD"]);
        let destination = side.path().join("worktree");
        git(root, &["worktree", "add", destination.to_str().unwrap(), "history-target"]);
        let mut app = editor(); let card = open_real_git(&mut app, root, None); let origin = bound(&app, card);
        sidebar_action(&mut app, SidebarAction::SetChangesTab { key: origin.key.clone(), tab: sidebar::ChangesTab::History });
        let row = app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap().history[0].clone();
        sidebar_action(&mut app, SidebarAction::HistoryCommand { key: origin.key, oid: row.oid, mainline: row.mainline, message: row.message, command: sidebar::HistoryCommand::Revert });
        assert_eq!(std::fs::read_to_string(root.join("tracked.txt")).unwrap(), "original\n");
        assert!(git(root, &["log", "-1", "--format=%s"]).starts_with("Revert"));
        let other = open_real_git(&mut app, &destination, None); let target = bound(&app, other);
        sidebar_action(&mut app, SidebarAction::SetChangesTab { key: target.key.clone(), tab: sidebar::ChangesTab::History });
        sidebar_action(&mut app, SidebarAction::SearchHistory { key: target.key.clone(), query: picked.clone() });
        let row = app.tab(other).unwrap().sidebar.source_control.as_ref().unwrap().history[0].clone();
        assert_eq!(row.oid.as_str(), picked);
        sidebar_action(&mut app, SidebarAction::HistoryCommand { key: target.key, oid: row.oid, mainline: row.mainline, message: row.message, command: sidebar::HistoryCommand::CherryPick });
        assert!(app.notice.is_none(), "{:?}", app.notice);
        assert_eq!(std::fs::read_to_string(destination.join("tracked.txt")).unwrap(), "picked contents\n");
        assert_eq!(std::fs::read_to_string(root.join("tracked.txt")).unwrap(), "original\n");
        assert_eq!(git(&destination, &["log", "-1", "--format=%s"]), "Change to replay");
    }

    #[test]
    fn standalone_history_cherry_pick_conflict_refreshes_conflict_controls_and_git_output() {
        let directory = git_fixture(); let root = directory.path(); let side = tempfile::tempdir().unwrap();
        git(root, &["branch", "conflict-target"]);
        std::fs::write(root.join("tracked.txt"), "incoming change\n").unwrap();
        git(root, &["add", "tracked.txt"]); git(root, &["commit", "-m", "Incoming change"]);
        let picked = git(root, &["rev-parse", "HEAD"]);
        let destination = side.path().join("worktree");
        git(root, &["worktree", "add", destination.to_str().unwrap(), "conflict-target"]);
        std::fs::write(destination.join("tracked.txt"), "local conflicting change\n").unwrap();
        git(&destination, &["add", "tracked.txt"]); git(&destination, &["commit", "-m", "Local conflicting change"]);
        let mut app = editor(); let card = open_real_git(&mut app, &destination, None); let origin = bound(&app, card);
        sidebar_action(&mut app, SidebarAction::SetChangesTab { key: origin.key.clone(), tab: sidebar::ChangesTab::History });
        sidebar_action(&mut app, SidebarAction::SearchHistory { key: origin.key.clone(), query: picked });
        let row = app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap().history[0].clone();
        sidebar_action(&mut app, SidebarAction::HistoryCommand { key: origin.key, oid: row.oid, mainline: row.mainline, message: row.message, command: sidebar::HistoryCommand::CherryPick });
        let source = app.tab(card).unwrap().sidebar.source_control.as_ref().unwrap();
        assert_eq!(source.operation.as_ref().unwrap().kind, cagents::repository::GitOperationKind::CherryPick);
        assert!(!source.group(cagents::repository::ChangeArea::Conflict).unwrap().is_empty());
        assert!(app.notice.as_deref().is_some_and(|notice| notice.contains("Git output:") && notice.contains("CONFLICT")), "{:?}", app.notice);
        assert!(app.tab(card).unwrap().mutation.is_none());
        assert_eq!(std::fs::read_to_string(root.join("tracked.txt")).unwrap(), "incoming change\n");
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
        assert!(app.tab(card).unwrap().owned_file.is_none());
        assert_eq!(app.tab(card).unwrap().title, "tracked.txt");
        std::fs::write(root.join("tracked.txt"), "newer working document\n").unwrap();
        let task = app.update(Message::Chosen(app.selection_epoch, Ok(Some(root.join("tracked.txt")))));
        run_tasks(&mut app, task);
        let working = app.active.unwrap();
        assert_ne!(working, card, "historical tab cannot deduplicate a live working-file open");
        assert_eq!(app.tab(working).unwrap().runtime.editor_text().as_deref(), Some("newer working document\n"));
        assert_eq!(app.tab(working).unwrap().owned_file.as_deref(), Some(root.join("tracked.txt").as_path()));
        assert!(app.tab(card).unwrap().runtime.shows_diff());
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
    fn standalone_commit_refuses_replaced_git_metadata_with_unchanged_worktree_directory() {
        use std::os::unix::fs::MetadataExt;
        let directory = git_fixture(); let root = directory.path(); let foreign = git_fixture(); let parked = tempfile::tempdir().unwrap();
        std::fs::write(root.join("tracked.txt"), "original staged content\n").unwrap(); git(root, &["add", "tracked.txt"]);
        git(foreign.path(), &["commit", "--allow-empty", "-m", "Foreign repository baseline"]);
        std::fs::write(foreign.path().join("tracked.txt"), "foreign staged content\n").unwrap(); git(foreign.path(), &["add", "tracked.txt"]);
        let foreign_head = git(foreign.path(), &["rev-parse", "HEAD"]);
        let original_head = git(root, &["rev-parse", "HEAD"]);
        let metadata = std::fs::metadata(root).unwrap(); let identity = (metadata.dev(), metadata.ino());
        let mut app = editor(); let card = open_real_git(&mut app, root, None); let origin = bound(&app, card);
        sidebar_action(&mut app, SidebarAction::CommitMessageChanged { key: origin.key.clone(), message: "Must belong to original repository".into() });
        let saved_git = parked.path().join("original-git");
        std::fs::rename(root.join(".git"), &saved_git).unwrap();
        std::fs::rename(foreign.path().join(".git"), root.join(".git")).unwrap();
        let metadata = std::fs::metadata(root).unwrap(); assert_eq!((metadata.dev(), metadata.ino()), identity);
        assert_eq!(git(root, &["rev-parse", "HEAD"]), foreign_head);
        sidebar_action(&mut app, SidebarAction::Commit { key: origin.key, message: "Must belong to original repository".into(), amend: false });
        assert_eq!(git(root, &["rev-parse", "HEAD"]), foreign_head, "stale displayed repository must not commit into replacement metadata");
        assert_eq!(git(root, &["--git-dir", saved_git.to_str().unwrap(), "rev-parse", "HEAD"]), original_head);
        assert_eq!(app.tab(card).unwrap().navigation.commit_message(), "Must belong to original repository");
        assert!(app.tab(card).unwrap().mutation.is_none());
        assert!(app.notice.is_some());
    }

    #[test]
    fn standalone_moved_root_without_replacement_settles_commit_and_read_slots() {
        for commit in [false, true] {
            let directory = git_fixture(); let root = directory.path();
            let parked = tempfile::tempdir().unwrap();
            let mut app = editor(); let card = open_real_git(&mut app, root, None); let origin = bound(&app, card);
            let task = if commit {
                std::fs::write(root.join("tracked.txt"), "pending contents\n").unwrap();
                sidebar_action(&mut app, SidebarAction::Stage { key: origin.key.clone(), selection: vec![PathSelection::new(RepoPath::parse("tracked.txt").unwrap())] });
                sidebar_action(&mut app, SidebarAction::CommitMessageChanged { key: origin.key.clone(), message: "Keep this draft".into() });
                app.update(Message::Sidebar(SidebarAction::Commit { key: origin.key, message: "Keep this draft".into(), amend: false }))
            } else {
                app.update(Message::Sidebar(SidebarAction::AmendChanged { key: origin.key, amend: true }))
            };
            let destination = parked.path().join("original");
            std::fs::rename(root, &destination).unwrap();
            run_tasks(&mut app, task);
            assert!(app.tab(card).unwrap().mutation.is_none());
            assert!(app.tab(card).unwrap().git_review.submitted_revision.is_none());
            assert!(app.tab(card).unwrap().git_review.amend_cancel.is_none());
            assert!(app.tab(card).unwrap().navigation.amend_request_revision().is_none());
            if commit { assert_eq!(app.tab(card).unwrap().navigation.commit_message(), "Keep this draft"); }
            else {
                let key = app.tab(card).unwrap().navigation.active_worktree().unwrap().clone();
                sidebar_action(&mut app, SidebarAction::AmendChanged { key: key.clone(), amend: false });
                assert!(app.tab(card).unwrap().navigation.amend_target().is_none());
                sidebar_action(&mut app, SidebarAction::HistoryQueryChanged { key, query: "draft while missing".into() });
            }
            assert_eq!(git(&destination, &["rev-list", "--count", "HEAD"]), "1");
        }
    }

    #[test]
    fn standalone_known_created_commit_remains_created_after_original_root_disappears() {
        let directory = git_fixture(); let root = directory.path(); let parked = tempfile::tempdir().unwrap();
        let mut app = editor(); let card = open_real_git(&mut app, root, None); let origin = bound(&app, card);
        sidebar_action(&mut app, SidebarAction::CommitMessageChanged { key: origin.key.clone(), message: "Captured successful commit".into() });
        let revision = app.tab(card).unwrap().navigation.submitted_commit_revision(&origin.key, "Captured successful commit", false).unwrap();
        assert!(app.begin_mutation(card, MutationKind::Commit).is_some());
        app.tab_mut(card).unwrap().git_review.submitted_revision = Some(revision);
        // Materialize the known successful worker outcome before delaying its
        // delivery and moving the directory away.
        git(root, &["commit", "--allow-empty", "-m", "Captured successful commit"]);
        let oid = Oid::parse_object(git(root, &["rev-parse", "HEAD"])).unwrap();
        let destination = parked.path().join("original"); std::fs::rename(root, &destination).unwrap();
        let task = app.update(Message::GitReview(Action::Committed { origin, revision, result: Ok(CommitOutcome::Created { oid, warnings: Vec::new() }) }));
        assert!(app.tab(card).unwrap().mutation.is_none());
        assert!(app.tab(card).unwrap().navigation.commit_message().is_empty());
        // The ordinary asynchronous refresh reports the missing binding; UI
        // delivery itself performs no filesystem validation or outcome rewrite.
        run_tasks(&mut app, task);
        assert!(app.notice.as_deref().is_some_and(|notice| !notice.contains("Commit failed") && (notice.contains("available") || notice.contains("No such file"))), "{:?}", app.notice);
        assert_eq!(git(&destination, &["log", "-1", "--format=%s"]), "Captured successful commit");
    }

    #[test]
    fn standalone_commit_cancel_uses_admitted_token_and_preserves_known_worker_outcome() {
        use cosmic::iced::futures::StreamExt;
        for after_worker in [false, true] {
            let directory = git_fixture(); let root = directory.path();
            std::fs::write(root.join("tracked.txt"), "staged cancellation fixture\n").unwrap();
            git(root, &["add", "tracked.txt"]);
            let original_head = git(root, &["rev-parse", "HEAD"]);
            let mut app = editor(); let card = open_real_git(&mut app, root, None); let origin = bound(&app, card);
            let message = "Commit with cancellation";
            sidebar_action(&mut app, SidebarAction::CommitMessageChanged { key: origin.key.clone(), message: message.into() });
            let task = app.update(Message::Sidebar(SidebarAction::Commit { key: origin.key.clone(), message: message.into(), amend: false }));
            let cancellation = app.mutation_cancellation(card).expect("Commit admitted");
            let mut messages = Vec::new();
            let task = if after_worker {
                messages = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                    let mut messages = Vec::new();
                    let mut stream = iced::runtime::task::into_stream(task).expect("Commit worker");
                    while let Some(action) = stream.next().await {
                        if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action { messages.push(message); }
                    }
                    messages
                });
                assert!(messages.iter().any(|message| matches!(message, Message::GitReview(Action::Committed { result: Ok(CommitOutcome::Created { .. }), .. }))));
                Task::none()
            } else { task };
            sidebar_action(&mut app, SidebarAction::CancelGitMutation { key: origin.key });
            assert!(cancellation.is_cancelled());
            assert_eq!(app.tab(card).unwrap().mutation, Some(MutationKind::Commit), "Cancel waits for the owned result");
            if after_worker {
                for message in messages { let result = app.update(message); run_tasks(&mut app, result); }
                assert_ne!(git(root, &["rev-parse", "HEAD"]), original_head);
                assert_eq!(git(root, &["log", "-1", "--format=%s"]), message);
                assert!(app.tab(card).unwrap().navigation.commit_message().is_empty(), "known Created remains successful after Cancel");
            } else {
                run_tasks(&mut app, task);
                assert_eq!(git(root, &["rev-parse", "HEAD"]), original_head);
                assert_eq!(app.tab(card).unwrap().navigation.commit_message(), message);
                assert!(!git(root, &["diff", "--cached", "--name-only"]).is_empty());
            }
            assert!(app.tab(card).unwrap().mutation.is_none());
            assert!(app.tab(card).unwrap().git_review.submitted_revision.is_none());
            assert!(app.mutation_cancellation(card).is_none());
        }
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
