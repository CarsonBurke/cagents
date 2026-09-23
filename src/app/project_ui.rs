use super::*;
use crate::repository::{CommandSpec, ProcessExecutor, SystemProcessExecutor};
use crate::{
    github::avatar::{self, OwnerKey},
    model::WorktreeId,
    sidebar::ProjectCommand,
};

#[derive(Debug, Default)]
pub struct State {
    owners: HashMap<OwnerKey, Option<iced::advanced::image::Handle>>,
    projects: HashMap<ProjectId, OwnerKey>,
    dialog: Option<Dialog>,
    /// Deletions the user has confirmed, by worktree, until each resolves. A
    /// failed one stays so its row can say why and offer to run it again.
    deletions: HashMap<WorktreeId, Deletion>,
    editors: Vec<crate::external_editor::Editor>,
}

/// A confirmed deletion. It runs behind the worktree's row, not a dialog.
#[derive(Debug, Clone)]
struct Deletion {
    target: Target,
    /// Why the last attempt failed; `None` while one is running.
    failure: Option<String>,
}

impl State {
    pub fn modal_open(&self) -> bool {
        self.dialog.is_some()
    }

    fn running(&self) -> impl Iterator<Item = &Target> {
        self.deletions
            .values()
            .filter(|deletion| deletion.failure.is_none())
            .map(|deletion| &deletion.target)
    }

    /// Whether `directory` is inside a worktree whose deletion is running.
    ///
    /// Nothing may start there: its sessions have been or are being closed,
    /// and Git is about to remove the directory from under whatever did.
    pub(super) fn deleting_directory(&self, directory: &Path) -> bool {
        self.running()
            .any(|target| directory.starts_with(&target.path))
    }

    /// Whether `card` works inside a worktree whose deletion is running.
    pub(super) fn deleting_card(&self, card: &CardState) -> bool {
        self.running()
            .any(|target| card_belongs_to(card, &target.path))
    }

    fn row_deletion(&self, worktree: WorktreeId) -> Option<crate::sidebar::RowDeletion> {
        self.deletions
            .get(&worktree)
            .map(|deletion| match &deletion.failure {
                None => crate::sidebar::RowDeletion::Deleting,
                Some(reason) => crate::sidebar::RowDeletion::Failed(reason.as_str().into()),
            })
    }

    /// Whether the deletion of `worktree` is under way.
    fn is_deleting(&self, worktree: WorktreeId) -> bool {
        self.deletions
            .get(&worktree)
            .is_some_and(|deletion| deletion.failure.is_none())
    }

    /// Whether any worktree deletion is under way.
    pub(super) fn deletion_running(&self) -> bool {
        self.running().next().is_some()
    }

    pub(super) fn focus_target(&self) -> widget::Id {
        widget::Id::new(
            if self
                .dialog
                .as_ref()
                .is_some_and(|dialog| dialog.blocker.is_some())
            {
                "dialog.project.close"
            } else {
                "dialog.project.confirm"
            },
        )
    }
}

#[derive(Debug, Clone)]
struct Dialog {
    target: Target,
    /// A pre-flight refusal: deletion cannot proceed, so the confirm button
    /// stays disabled until the user resolves it outside the dialog.
    blocker: Option<String>,
    /// Why the last confirmed deletion of this worktree failed, when the
    /// dialog is opened again from its row. Confirming retries it.
    failure: Option<String>,
}

impl Dialog {
    fn message(&self) -> Option<&str> {
        self.blocker.as_deref().or(self.failure.as_deref())
    }
}

#[derive(Debug, Clone)]
pub struct Target {
    project: ProjectId,
    worktree: Option<WorktreeId>,
    repository: PathBuf,
    path: PathBuf,
    label: String,
}

#[derive(Debug, Clone)]
pub enum Action {
    Command {
        project_id: ProjectId,
        worktree_id: Option<WorktreeId>,
        command: ProjectCommand,
    },
    EditorsLoaded(Result<Vec<crate::external_editor::Editor>, String>),
    ConfirmDelete,
    /// Runs a failed deletion again. The user confirmed it once already.
    RetryDelete(WorktreeId),
    AwaitSessions(Target),
    Deleted {
        target: Target,
        result: Result<(), String>,
    },
    AvatarLoaded {
        owner: OwnerKey,
        image: Option<iced::advanced::image::Handle>,
    },
    Finished(Result<(), String>),
    Dismiss,
}

impl Cagents {
    pub(super) fn editor_discovery_task() -> Task<Message> {
        cosmic::task::future(async {
            let result = tokio::task::spawn_blocking(crate::external_editor::installed)
                .await
                .map_err(|error| format!("Could not discover installed editors: {error}"));
            Message::ProjectUi(Action::EditorsLoaded(result))
        })
    }

    pub(super) fn open_file_in_editor(
        &mut self,
        path: PathBuf,
        editor_id: String,
    ) -> Task<Message> {
        if self.modal_is_open() {
            return Task::none();
        }
        self.launch_editor(path, &editor_id)
    }

    fn launch_editor(&mut self, path: PathBuf, editor_id: &str) -> Task<Message> {
        let Some(editor) = self
            .project_ui
            .editors
            .iter()
            .find(|editor| editor.choice.id == editor_id)
            .cloned()
        else {
            self.status = Some(TransientNotice::error(
                "The selected editor is no longer available.",
            ));
            return Task::none();
        };
        cosmic::task::future(async move {
            let result =
                tokio::task::spawn_blocking(move || crate::external_editor::launch(&editor, &path))
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
            Message::ProjectUi(Action::Finished(result))
        })
    }

    /// Run after installing RepositoryLoaded's new sidebar model, never in view.
    pub(super) fn project_avatar_task(&mut self, project: ProjectId) -> Task<Message> {
        let owner = self
            .github_availability
            .get(&project)
            .and_then(GitHubAvailability::repository_key)
            .and_then(OwnerKey::from_repository);
        let Some(owner) = owner.or_else(|| self.project_ui.projects.get(&project).cloned()) else {
            return Task::none();
        };
        self.project_ui.projects.insert(project, owner.clone());
        if let Some(image) = self.project_ui.owners.get(&owner) {
            if let Some(row) = self
                .sidebar_model
                .projects
                .iter_mut()
                .find(|row| row.id == project)
            {
                row.avatar.clone_from(image);
            }
            return Task::none();
        }
        // Insert before starting: None means loading or failed, both intentionally
        // deduplicated for this app lifetime, including repeated repository refreshes.
        self.project_ui.owners.insert(owner.clone(), None);
        cosmic::task::future(async move {
            let key = owner.clone();
            let image = tokio::task::spawn_blocking(move || avatar::load(&key))
                .await
                .ok()
                .flatten();
            Message::ProjectUi(Action::AvatarLoaded { owner, image })
        })
    }

    fn project_target(&self, project: ProjectId, worktree: Option<WorktreeId>) -> Option<Target> {
        let row = self
            .sidebar_model
            .projects
            .iter()
            .find(|row| row.id == project)?;
        let selected = match worktree {
            Some(id) => Some(row.worktrees.iter().find(|worktree| worktree.id == id)?),
            None => row.worktrees.first(),
        };
        Some(Target {
            project,
            worktree,
            repository: row.repository.clone(),
            path: selected.map_or_else(|| row.repository.clone(), |worktree| worktree.path.clone()),
            label: selected.filter(|_| worktree.is_some()).map_or_else(
                || row.name.clone(),
                |worktree| worktree.path.display().to_string(),
            ),
        })
    }

    pub(super) fn handle_project_action(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::AwaitSessions(target) => {
                let running = target
                    .worktree
                    .and_then(|id| self.project_ui.deletions.get(&id))
                    .is_some_and(|deletion| {
                        deletion.failure.is_none() && deletion.target.path == target.path
                    });
                if running {
                    return self.run_worktree_deletion(target);
                }
            }
            Action::EditorsLoaded(result) => match result {
                Ok(editors) => {
                    let choices: Vec<_> =
                        editors.iter().map(|editor| editor.choice.clone()).collect();
                    self.sidebar_model.external_editors = choices.clone();
                    for runtime in self.cards.values_mut() {
                        if let CardRuntime::Repository(runtime) = runtime {
                            runtime.set_external_editors(choices.clone());
                        }
                    }
                    self.project_ui.editors = editors;
                }
                Err(error) => self.status = Some(TransientNotice::warning(error)),
            },
            Action::AvatarLoaded { owner, image } => {
                for project in &mut self.sidebar_model.projects {
                    if self.project_ui.projects.get(&project.id) == Some(&owner) {
                        project.avatar.clone_from(&image);
                    }
                }
                self.project_ui.owners.insert(owner, image);
            }
            Action::Command {
                project_id,
                worktree_id,
                command,
            } => {
                if self.modal_is_open() && !self.project_ui.modal_open() {
                    return Task::none();
                }
                let Some(target) = self.project_target(project_id, worktree_id) else {
                    return Task::none();
                };
                // The row offers nothing while its worktree is being deleted;
                // a command that raced the deletion is refused the same way,
                // even once a read has moved the row to another path.
                if target
                    .worktree
                    .is_some_and(|id| self.project_ui.is_deleting(id))
                    || (target.worktree.is_some()
                        && self.project_ui.deleting_directory(&target.path))
                {
                    return Task::none();
                }
                match command {
                    ProjectCommand::OpenEditor(editor_id) => {
                        return self.launch_editor(target.path, &editor_id);
                    }
                    ProjectCommand::Delete => {
                        let blocker = self.worktree_deletion_blocker(&target);
                        let failure = target
                            .worktree
                            .and_then(|id| self.project_ui.deletions.get(&id))
                            .and_then(|deletion| deletion.failure.clone());
                        self.project_ui.dialog = Some(Dialog {
                            target,
                            blocker,
                            failure,
                        });
                        return iced::widget::operation::focus(self.project_ui.focus_target())
                            .map(cosmic::Action::App);
                    }
                    ProjectCommand::Reveal => {
                        self.project_ui.dialog = None;
                        return launch_task(PathBuf::from("xdg-open"), target.path, false);
                    }
                    ProjectCommand::NewTerminal => {
                        self.project_ui.dialog = None;
                        let select = self.select_worktree(target.path.clone(), false);
                        let terminal = self.add_terminal(target.path);
                        return Task::batch([select, terminal]);
                    }
                    ProjectCommand::NewWorktree => {
                        self.project_ui.dialog = None;
                        return cosmic::task::message(Message::Sidebar(
                            SidebarAction::NewWorktree {
                                project_id,
                                repository: target.repository,
                            },
                        ));
                    }
                    ProjectCommand::RepositoryUrl => {
                        let url = self
                            .github_availability
                            .get(&project_id)
                            .and_then(GitHubAvailability::repository_key)
                            .and_then(|key| {
                                Url::parse(&format!(
                                    "https://{}/{}/{}",
                                    key.authority(),
                                    key.owner,
                                    key.name
                                ))
                                .ok()
                            });
                        self.project_ui.dialog = None;
                        if let Some(url) = url {
                            return self.open_clicked_link(url);
                        }
                        self.status = Some(TransientNotice::warning(
                            "No GitHub repository URL is available. Refresh the repository after authenticating gh.",
                        ));
                    }
                }
            }
            Action::Dismiss => {
                let dismissed = self.project_ui.dialog.take();
                // Declining to run a failed deletion again is the answer to
                // it, so its row stops asking.
                if let Some(id) = dismissed.and_then(|dialog| dialog.target.worktree)
                    && self
                        .project_ui
                        .deletions
                        .get(&id)
                        .is_some_and(|deletion| deletion.failure.is_some())
                {
                    self.project_ui.deletions.remove(&id);
                    self.sync_worktree_operation_rows();
                }
                return self.project_dialog_closed();
            }
            Action::ConfirmDelete => {
                let Some(dialog) = &self.project_ui.dialog else {
                    return Task::none();
                };
                let target = dialog.target.clone();
                if let Some(blocker) = self.worktree_deletion_blocker(&target) {
                    let dialog = self.project_ui.dialog.as_mut().unwrap();
                    dialog.blocker = Some(blocker);
                    dialog.failure = None;
                    return Task::none();
                }
                // Confirmed is registered: the row reports the deletion from
                // here, so the dialog has nothing left to say.
                self.project_ui.dialog = None;
                let deletion = self.start_worktree_deletion(target);
                return Task::batch([deletion, self.project_dialog_closed()]);
            }
            Action::RetryDelete(id) => {
                let Some(deletion) = self.project_ui.deletions.get(&id) else {
                    return Task::none();
                };
                if deletion.failure.is_none() {
                    return Task::none();
                }
                // The row may have been read again since; retry what it lists
                // now, and let a row that is gone take its failure with it.
                let Some(target) = self.project_target(deletion.target.project, Some(id)) else {
                    self.project_ui.deletions.remove(&id);
                    self.sync_worktree_operation_rows();
                    return Task::none();
                };
                if let Some(blocker) = self.worktree_deletion_blocker(&target) {
                    self.status = Some(TransientNotice::warning(format!(
                        "Could not delete {}: {blocker}",
                        target.label
                    )));
                    if let Some(deletion) = self.project_ui.deletions.get_mut(&id) {
                        deletion.failure = Some(blocker);
                    }
                    self.sync_worktree_operation_rows();
                    return Task::none();
                }
                return self.start_worktree_deletion(target);
            }
            Action::Deleted { target, result } => match result {
                Err(failure) => {
                    // A blocker names what to resolve, so it outranks Git's
                    // own account of the same refusal.
                    let reason = self.worktree_deletion_blocker(&target).unwrap_or(failure);
                    self.status = Some(TransientNotice::error(format!(
                        "Could not delete {}: {reason}",
                        target.label
                    )));
                    if let Some(deletion) = target
                        .worktree
                        .and_then(|id| self.project_ui.deletions.get_mut(&id))
                    {
                        deletion.failure = Some(reason);
                    }
                    self.sync_worktree_operation_rows();
                    // The worktree stays, so the cards the sweep held back
                    // while it was going can mount again.
                    return self.lifecycle_sweep();
                }
                Ok(()) => {
                    if let Some(id) = target.worktree {
                        self.project_ui.deletions.remove(&id);
                    }
                    // The daemon has already closed every terminal and chat in
                    // the directory, so their cards are tombstoned here without
                    // a second close request: dropping a runtime only detaches.
                    let ids = self
                        .durable
                        .cards
                        .iter()
                        .filter(|card| {
                            card.existence.is_open() && card_belongs_to(card, &target.path)
                        })
                        .map(|card| card.id)
                        .collect::<Vec<_>>();
                    for &id in &ids {
                        self.retire_lsp_card(id);
                        self.sidebar_state.clear_agent_activity(id);
                        self.sidebar_state.acknowledge_agent_update(id);
                        self.chat_discard(id);
                        self.cards.remove(&id);
                        self.lifecycle.remove(id);
                        if let Some(card) = self.card_state_mut(id) {
                            card.existence = removed_existence("worktree deleted by user");
                        }
                    }
                    let emptied = self.close_emptied_channels(&ids);
                    self.worktree_offsets.remove(&target.path);
                    self.worktree_focus.remove(&target.path);
                    let removed = self
                        .sidebar_model
                        .projects
                        .iter_mut()
                        .find(|project| project.id == target.project)
                        .and_then(|project| {
                            let index = project
                                .worktrees
                                .iter()
                                .position(|worktree| Some(worktree.id) == target.worktree)?;
                            Some(project.worktrees.remove(index))
                        });
                    // Queued behind any save of the open worktree, so the
                    // tombstone is the catalog's last word on it.
                    if let (Some(id), Some(session)) = (target.worktree, &self.session) {
                        session.save_worktrees([WorktreeState {
                            id,
                            project_id: target.project,
                            generation: 0,
                            path: target.path.clone(),
                            branch: removed.and_then(|worktree| worktree.branch),
                            existence: removed_existence("worktree deleted by user"),
                        }]);
                    }
                    self.forget_worktree_creation(&target.path);
                    // The window may have moved anywhere while Git worked, a
                    // channel included, so the scope it is on is re-selected
                    // to drop the retired cards rather than replaced.
                    let select = match self.durable.active_channel {
                        Some(channel) => self.select_channel(channel),
                        None => {
                            let scope = self
                                .active_workspace
                                .clone()
                                .filter(|active| !active.starts_with(&target.path))
                                .unwrap_or_else(|| target.repository.clone());
                            self.select_worktree(scope, false)
                        }
                    };
                    self.persist();
                    self.status = Some(TransientNotice::success(
                        "Worktree deleted; branch retained.",
                    ));
                    let refresh =
                        self.repository_load_task(target.repository, Some(target.project), false);
                    return Task::batch([
                        select,
                        emptied,
                        refresh,
                        self.lifecycle_sweep(),
                        self.sync_chrome(),
                    ]);
                }
            },
            Action::Finished(result) => {
                if let Err(error) = result {
                    self.status = Some(TransientNotice::error(error));
                }
            }
        }
        Task::none()
    }

    /// Registers a confirmed deletion on its row and starts it.
    ///
    /// The workspace leaves the worktree now rather than once it is gone, so
    /// nothing the user does in the meantime lands in a directory whose
    /// sessions are being closed.
    fn start_worktree_deletion(&mut self, target: Target) -> Task<Message> {
        let Some(id) = target.worktree else {
            return Task::none();
        };
        self.project_ui.deletions.insert(
            id,
            Deletion {
                target: target.clone(),
                failure: None,
            },
        );
        self.sync_worktree_operation_rows();
        let leave = if self
            .active_workspace
            .as_deref()
            .is_some_and(|active| active.starts_with(&target.path))
        {
            self.select_worktree(target.repository.clone(), false)
        } else {
            Task::none()
        };
        Task::batch([leave, self.run_worktree_deletion(target)])
    }

    /// Runs the deletion once no session in the worktree is still starting:
    /// one that is would come up after its directory had been emptied.
    fn run_worktree_deletion(&mut self, target: Target) -> Task<Message> {
        if self.durable.cards.iter().any(|card| {
            card.existence.is_open()
                && card_belongs_to(card, &target.path)
                && (self.pending_terminal_spawn(card.id) || self.chat_start_pending(card.id))
        }) {
            return cosmic::task::future(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Message::ProjectUi(Action::AwaitSessions(target))
            });
        }
        let store = self.session.as_ref().map(SessionRuntime::operation_store);
        cosmic::task::future(async move {
            let request = target.clone();
            let result = tokio::task::spawn_blocking(move || {
                delete_worktree(&request)?;
                if let Some(store) = store
                    && let Err(error) =
                        crate::repository::release_deleted_worktree_creations(&store, &request.path)
                {
                    // The worktree is gone either way; what stays held is
                    // only the chance to create the same one again.
                    tracing::warn!(
                        worktree = %request.path.display(),
                        %error,
                        "could not release the deleted worktree's creation"
                    );
                }
                Ok(())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::ProjectUi(Action::Deleted { target, result })
        })
    }

    /// Refuses, and says so, to start anything in `directory` while the
    /// worktree it is in is being deleted.
    pub(super) fn refuse_in_deleting_worktree(&mut self, directory: &Path) -> bool {
        if !self.project_ui.deleting_directory(directory) {
            return false;
        }
        self.status = Some(TransientNotice::warning("That worktree is being deleted."));
        true
    }

    /// Refuses, and says so, to start a session for `card` while the
    /// worktree it works in is being deleted.
    pub(super) fn refuse_for_deleting_card(&mut self, card: CardId) -> bool {
        if !self
            .card_state(card)
            .is_some_and(|state| self.project_ui.deleting_card(state))
        {
            return false;
        }
        self.status = Some(TransientNotice::warning("That worktree is being deleted."));
        true
    }

    /// Hands focus back to whatever the dialog covered.
    fn project_dialog_closed(&mut self) -> Task<Message> {
        let focus = self.nested_modal_focus_target().unwrap_or_else(|| {
            self.workspace
                .focused()
                .map(|card| self.card_keyboard_focus(card))
                .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID))
        });
        Task::batch([
            iced::widget::operation::focus(focus).map(cosmic::Action::App),
            self.lifecycle_sweep(),
            self.sync_chrome(),
        ])
    }

    /// Shows each deletion on its worktree's row. A read of the checkout
    /// rebuilds the rows, so this runs after every one; a failure whose
    /// worktree the read no longer lists has nothing left to retry.
    pub(super) fn sync_worktree_deletion_rows(&mut self) {
        let projects = &mut self.sidebar_model.projects;
        self.project_ui.deletions.retain(|id, deletion| {
            deletion.failure.is_none()
                || projects
                    .iter()
                    .flat_map(|project| &project.worktrees)
                    .any(|worktree| worktree.id == *id)
        });
        for worktree in projects
            .iter_mut()
            .flat_map(|project| &mut project.worktrees)
        {
            worktree.deletion = self.project_ui.row_deletion(worktree.id);
        }
    }

    fn worktree_deletion_blocker(&self, target: &Target) -> Option<String> {
        if self.session.is_none() {
            return Some(
                "Session storage is unavailable; deletion cannot be recorded safely.".into(),
            );
        }
        let Some(id) = target.worktree else {
            return Some("The primary repository cannot be deleted.".into());
        };
        let Some(project) = self
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.id == target.project)
        else {
            return Some("The project disappeared. Refresh before deleting.".into());
        };
        let Some(worktree) = project
            .worktrees
            .iter()
            .find(|worktree| worktree.id == id && worktree.path == target.path)
        else {
            return Some("The worktree changed. Refresh before deleting.".into());
        };
        if worktree.path == project.repository
            || project.repository.parent() == Some(worktree.path.as_path())
        {
            return Some("The primary worktree cannot be deleted.".into());
        }
        if worktree.locked {
            return Some("This worktree is locked. Unlock it explicitly in Git first.".into());
        }
        if worktree
            .dirty
            .as_ref()
            .is_some_and(|dirty| dirty.is_dirty())
        {
            return Some(
                "Commit, stash, or discard the worktree changes before deleting it.".into(),
            );
        }
        if self.cards.values().any(|runtime| {
            matches!(runtime, CardRuntime::Repository(runtime)
            if runtime.descriptor.worktree.starts_with(&target.path) && runtime.blocks_navigation())
        }) {
            return Some(
                "Save and close this worktree’s unsaved editor buffers before deleting it.".into(),
            );
        }
        None
    }

    /// What the deletion takes with it: the open terminal and chat cards
    /// inside the worktree, counted so the user knows before confirming.
    fn worktree_cascade_summary(&self, target: &Target) -> Option<String> {
        let (mut terminals, mut chats) = (0, 0);
        for card in &self.durable.cards {
            if !card.existence.is_open() || !card_belongs_to(card, &target.path) {
                continue;
            }
            match &card.kind {
                CardKind::Terminal(_) => terminals += 1,
                CardKind::Chat(_) => chats += 1,
                _ => {}
            }
        }
        let count =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let parts = match (terminals, chats) {
            (0, 0) => return None,
            (t, 0) => count(t, "terminal", "terminals"),
            (0, c) => count(c, "chat", "chats"),
            (t, c) => format!(
                "{} and {}",
                count(t, "terminal", "terminals"),
                count(c, "chat", "chats")
            ),
        };
        Some(format!(
            "This closes {parts} in the worktree and ends their sessions."
        ))
    }

    pub(super) fn project_dialog_view(&self) -> Option<Element<'_, Message>> {
        let dialog = self.project_ui.dialog.as_ref()?;
        let target = &dialog.target;
        let mut rows: Vec<Element<'_, Message>> = vec![
            widget::text::title3("Delete worktree?").into(),
            super::notice_body(&target.label),
            super::notice_detail(target.path.display().to_string()),
            super::notice_detail(
                "Git will remove this clean worktree directory. The branch is retained. \
                 Terminals and chats running in it, including detached ones, are closed \
                 first. Primary, locked, and dirty worktrees are never removed.",
            ),
        ];
        if let Some(cascade) = self.worktree_cascade_summary(target) {
            rows.push(super::notice_body(cascade));
        }
        if let Some(message) = dialog.message() {
            rows.push(super::notice_body(message));
        }
        // Reopened from a row whose deletion failed, the dialog says why and
        // confirming runs it again; `ConfirmDelete` re-checks every
        // precondition first either way.
        let confirm = dialog
            .blocker
            .is_none()
            .then_some(Message::ProjectUi(Action::ConfirmDelete));
        rows.push(
            widget::row::with_children(vec![
                iced::widget::Space::new().width(Length::Fill).into(),
                widget::button::standard("Cancel")
                    .id(widget::Id::new("dialog.project.close"))
                    .on_press(Message::ProjectUi(Action::Dismiss))
                    .into(),
                widget::button::destructive(if dialog.failure.is_some() {
                    "Retry delete"
                } else {
                    "Delete worktree"
                })
                .id(widget::Id::new("dialog.project.confirm"))
                .on_press_maybe(confirm)
                .into(),
            ])
            .spacing(cosmic::theme::spacing().space_xxs)
            .into(),
        );
        Some(super::dialog_ui::surface(
            widget::column::with_children(rows).spacing(3.0 * design::GRID_UNIT),
            480.0,
        ))
    }
}

fn launch_task(executable: PathBuf, path: PathBuf, editor: bool) -> Task<Message> {
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            let mut command = std::process::Command::new(executable);
            if editor {
                command.arg("--");
            }
            let child = command
                .arg(&path)
                .current_dir(if path.is_file() {
                    path.parent().unwrap_or(&path)
                } else {
                    &path
                })
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|error| format!("Could not open {}: {error}", path.display()))?;
            // Reap the launcher without waiting for a long-lived editor on the UI thread.
            std::thread::spawn(move || {
                let _ = {
                    let mut child = child;
                    child.wait()
                };
            });
            Ok(())
        })
        .await
        .map_err(|error| error.to_string())
        .and_then(|result| result);
        Message::ProjectUi(Action::Finished(result))
    })
}

/// The tombstone a card gets when the container it lived in is deleted
/// outright, rather than the card itself being closed.
pub(super) fn removed_existence(reason: &str) -> Existence {
    Existence::Closed {
        tombstone: crate::model::Tombstone {
            operation_id: None,
            closed_at_unix_ms: unix_millis(),
            reason: Some(reason.into()),
        },
    }
}

/// Whether a card lives inside the worktree at `path`: its process runs there,
/// or it views files there.
fn card_belongs_to(card: &CardState, path: &Path) -> bool {
    match &card.kind {
        CardKind::Terminal(session) => session.launch.working_directory.starts_with(path),
        CardKind::Chat(session) => session.launch.recipe.working_directory.starts_with(path),
        CardKind::Repository(view) => view.worktree.starts_with(path),
        _ => false,
    }
}

/// Ends every daemon terminal and chat running inside the worktree, the
/// detached and hidden ones included, so the directory can go.
///
/// Deleting a worktree is a decision about everything in it; a session that
/// survived would be a shell in a directory that no longer exists. The
/// terminals are closed through the checked path with active work allowed,
/// which is the same request the close dialog sends once the user has
/// confirmed. A close the daemon cannot confirm stops the deletion: the
/// directory is not removed from under a process whose fate is unknown.
fn close_worktree_sessions(target: &Target) -> Result<(), String> {
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    let mut client = match crate::control::ControlClient::connect(&socket) {
        Ok(client) => client,
        // No daemon means no sessions to close.
        Err(crate::control::ControlError::Connect(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(());
        }
        Err(error) => {
            return Err(format!(
                "Cannot reach the daemon to close sessions: {error}"
            ));
        }
    };
    let canonical_target = target
        .path
        .canonicalize()
        .map_err(|error| format!("Cannot resolve worktree identity: {error}"))?;
    let inside = |directory: &Path| {
        directory.starts_with(&target.path)
            || directory
                .canonicalize()
                .is_ok_and(|directory| directory.starts_with(&canonical_target))
    };
    for terminal in client
        .list()
        .map_err(|error| format!("Cannot list the worktree's terminals: {error}"))?
    {
        if !inside(&terminal.session.launch.working_directory)
            || matches!(
                terminal.session.continuity,
                TerminalContinuity::Exited { .. }
                    | TerminalContinuity::RelaunchAvailable {
                        last_exit: Some(_),
                        ..
                    }
            )
        {
            continue;
        }
        let id = terminal.session.id;
        match client
            .close_terminal_checked(id, true)
            .map_err(|error| format!("Could not close terminal {id}: {error}"))?
        {
            crate::terminal::ipc::TerminalCloseResult::Closed => {}
            crate::terminal::ipc::TerminalCloseResult::Active => {
                return Err(format!("Terminal {id} declined to close while active."));
            }
            crate::terminal::ipc::TerminalCloseResult::Unknown(detail) => {
                return Err(format!("Terminal {id} may still be running: {detail}"));
            }
        }
    }
    for session in client
        .list_chats()
        .map_err(|error| format!("Cannot list the worktree's chats: {error}"))?
    {
        if !inside(&session.launch.recipe.working_directory) {
            continue;
        }
        let id = session.id;
        client
            .close_chat(session)
            .map_err(|error| format!("Could not close chat {id}: {error}"))?;
    }
    Ok(())
}

/// Removes the worktree through Git once Git confirms it is the one the user
/// chose and is safe to remove.
///
/// Git's listing is the authority here, not the window's catalog: the catalog
/// is written behind the UI and may not have caught up with a worktree created
/// moments ago. The deletion is recorded in the catalog afterwards, on the same
/// ordered lane as every other catalog write.
fn delete_worktree(target: &Target) -> Result<(), String> {
    let id = target
        .worktree
        .ok_or("The primary repository cannot be deleted.")?;
    let service = RepositoryService::system("git");
    let cancellation = CancellationToken::default();
    let records = service
        .list_worktrees(&target.repository, &cancellation)
        .map_err(|error| error.to_string())?;
    let record = records
        .iter()
        .find(|record| record.id == id && record.path == target.path)
        .ok_or("Worktree changed or disappeared; refresh before deleting.")?;
    if records.first().is_some_and(|primary| primary.id == id) || record.bare {
        return Err("The primary worktree cannot be deleted.".into());
    }
    if record.lock.locked {
        return Err("The worktree is locked; unlock it explicitly in Git first.".into());
    }
    if service
        .status(&target.path, &cancellation)
        .map_err(|error| error.to_string())?
        .dirty
        .is_dirty()
    {
        return Err("The worktree has changes; commit, stash, or discard them first.".into());
    }
    close_worktree_sessions(target)?;
    // Git itself rechecks cleanliness/locks and owns directory removal; no force,
    // rm -rf, branch deletion, or shell interpolation.
    let command = CommandSpec::new("git")
        .cwd(&target.repository)
        .args(["worktree", "remove", "--"])
        .arg(target.path.as_os_str())
        .timeout(Duration::from_secs(30))
        .output_limits(64 * 1024, 64 * 1024);
    let result = SystemProcessExecutor
        .execute(&command, &cancellation)
        .and_then(|output| output.require_success(&command.program));
    if let Err(error) = result {
        // A timeout may happen after removal. Reconcile before saying it failed.
        // Git's own account is what the row and the notice report: a bare
        // exit status gives the user nothing to act on before a retry.
        let records = service
            .list_worktrees(&target.repository, &cancellation)
            .map_err(|_| error.reported_detail())?;
        if records.iter().any(|record| record.id == id) {
            return Err(error.reported_detail());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::RowDeletion;

    /// A window with session storage and one project at `repository` whose
    /// linked worktree is `linked`, both of them saved.
    fn deletable_app(
        repository: &Path,
        linked: &SidebarWorktree,
        primary: &SidebarWorktree,
    ) -> (Cagents, tempfile::TempDir, ProjectId) {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap();
        let (session, _) = SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("test"),
        )
        .unwrap();
        let project = ProjectId::new();
        session.save_project(ProjectState {
            id: project,
            generation: 0,
            name: "repo".into(),
            repository: repository.to_path_buf(),
            existence: Existence::Open,
        });
        app.session = Some(session);
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            repository.to_path_buf(),
            vec![primary.clone(), linked.clone()],
            Vec::new(),
        ));
        (app, directory, project)
    }

    fn row(app: &Cagents, id: WorktreeId) -> Option<&SidebarWorktree> {
        app.sidebar_model
            .projects
            .iter()
            .flat_map(|project| &project.worktrees)
            .find(|worktree| worktree.id == id)
    }

    /// A worktree deleted before the catalog ever stored it is still recorded
    /// as deleted, and the tombstone lands after a save of it as open that was
    /// still queued (#245).
    #[test]
    fn deletion_records_its_tombstone_without_waiting_for_the_catalog() {
        let repository = PathBuf::from("/repo");
        let linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("worktrees/feature"),
            Some("feature".into()),
        );
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.clone(),
            Some("main".into()),
        );
        let id = linked.id;
        let (mut app, _directory, project) = deletable_app(&repository, &linked, &primary);

        let _ = app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: Some(id),
            command: ProjectCommand::Delete,
        });
        let target = app.project_ui.dialog.as_ref().unwrap().target.clone();
        let _ = app.handle_project_action(Action::ConfirmDelete);
        assert!(app.project_ui.dialog.is_none());
        // A repository refresh that listed the worktree before Git removed it.
        app.session
            .as_ref()
            .unwrap()
            .save_worktrees([WorktreeState {
                id,
                project_id: project,
                generation: 0,
                path: linked.path.clone(),
                branch: Some("feature".into()),
                existence: Existence::Open,
            }]);
        let _ = app.handle_project_action(Action::Deleted {
            target,
            result: Ok(()),
        });
        assert!(app.project_ui.deletions.is_empty());
        assert!(
            !app.sidebar_model.projects[0]
                .worktrees
                .iter()
                .any(|worktree| worktree.id == id)
        );

        let session = app.session.as_ref().unwrap();
        let store = session.operation_store();
        session.flush(Duration::from_secs(5)).unwrap();
        let deleted = store.load_worktree(id).unwrap().unwrap();
        assert!(!deleted.existence.is_open());
        assert_eq!(deleted.project_id, project);
        assert_eq!(deleted.path, linked.path);
        assert_eq!(deleted.branch.as_deref(), Some("feature"));
    }

    /// The confirmation goes the moment it is answered, and the worktree's
    /// row carries the deletion from there (#264): nothing starts in the
    /// worktree meanwhile, a failure stays on the row through a re-read of
    /// the checkout, the row runs it again, and declining it clears the row.
    #[test]
    fn a_deletion_runs_behind_its_row_and_a_failure_is_retried_from_there() {
        let repository = PathBuf::from("/repo");
        let linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("worktrees/feature"),
            Some("feature".into()),
        );
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.clone(),
            Some("main".into()),
        );
        let id = linked.id;
        let (mut app, _directory, project) = deletable_app(&repository, &linked, &primary);
        let delete = Action::Command {
            project_id: project,
            worktree_id: Some(id),
            command: ProjectCommand::Delete,
        };

        let _ = app.handle_project_action(delete.clone());
        let _ = app.handle_project_action(Action::ConfirmDelete);
        assert!(!app.modal_is_open());
        assert_eq!(row(&app, id).unwrap().deletion, Some(RowDeletion::Deleting));

        let cards = app.durable.cards.len();
        let _ = app.add_terminal(linked.path.join("src"));
        assert_eq!(app.durable.cards.len(), cards, "a terminal started in it");
        assert_eq!(
            app.status.as_ref().map(|notice| notice.message.as_str()),
            Some("That worktree is being deleted.")
        );
        assert!(!app.project_ui.deleting_directory(&repository));

        let target = app.project_ui.deletions[&id].target.clone();
        let _ = app.handle_project_action(Action::Deleted {
            target: target.clone(),
            result: Err("fatal: cannot remove".into()),
        });
        let failed = Some(RowDeletion::Failed("fatal: cannot remove".into()));
        assert_eq!(row(&app, id).unwrap().deletion, failed);
        assert!(!app.project_ui.deleting_directory(&linked.path));
        assert!(
            app.status
                .as_ref()
                .is_some_and(|notice| notice.message.ends_with("fatal: cannot remove"))
        );

        // A read of the checkout replaces the rows; the failure is the
        // application's, so it is put back on the new one.
        app.sidebar_model.projects[0].worktrees = vec![
            primary.clone(),
            SidebarWorktree::from_durable(id, linked.path.clone(), Some("feature".into())),
        ];
        app.sync_worktree_operation_rows();
        assert_eq!(row(&app, id).unwrap().deletion, failed);

        let _ = app.handle_project_action(Action::RetryDelete(id));
        assert_eq!(row(&app, id).unwrap().deletion, Some(RowDeletion::Deleting));
        let _ = app.handle_project_action(Action::Deleted {
            target,
            result: Err("fatal: cannot remove".into()),
        });

        // Reopened from the row, the dialog says why; cancelling declines it.
        let _ = app.handle_project_action(delete);
        assert_eq!(
            app.project_ui.dialog.as_ref().unwrap().failure.as_deref(),
            Some("fatal: cannot remove")
        );
        let _ = app.handle_project_action(Action::Dismiss);
        assert_eq!(row(&app, id).unwrap().deletion, None);
        assert!(app.project_ui.deletions.is_empty());
    }

    /// The window moves off a worktree whose deletion it confirms and cannot
    /// be brought back while Git works, by the row or by cycling workspaces,
    /// nor restarted underneath it. Wherever the user goes meanwhile, a
    /// channel included, is where the window stays when the deletion lands.
    #[test]
    fn the_window_leaves_a_worktree_being_deleted_and_stays_where_the_user_went() {
        let repository = PathBuf::from("/repo");
        let linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("worktrees/feature"),
            Some("feature".into()),
        );
        let primary = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.clone(),
            Some("main".into()),
        );
        let id = linked.id;
        let (mut app, _directory, project) = deletable_app(&repository, &linked, &primary);
        let _ = app.select_worktree(linked.path.clone(), false);
        assert_eq!(app.active_workspace.as_ref(), Some(&linked.path));

        let _ = app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: Some(id),
            command: ProjectCommand::Delete,
        });
        let _ = app.handle_project_action(Action::ConfirmDelete);
        assert_eq!(app.active_workspace.as_ref(), Some(&repository));
        assert!(
            app.window_update_blocker().is_some(),
            "an update would restart the window halfway through the deletion"
        );

        let open = SidebarAction::OpenWorktree {
            project_id: project,
            worktree_id: id,
            path: linked.path.clone(),
        };
        assert!(
            !crate::sidebar::workspace_cycle(&app.sidebar_model, &app.sidebar_state)
                .contains(&open),
            "cycling workspaces lands on the worktree being deleted"
        );
        let _ = app.update(Message::Sidebar(open));
        assert_eq!(app.active_workspace.as_ref(), Some(&repository));
        assert_eq!(
            app.status.as_ref().map(|notice| notice.message.as_str()),
            Some("That worktree is being deleted.")
        );

        let channel = crate::model::ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(crate::model::ProjectChannel {
                id: channel,
                project_id: project,
                title: "Investigation".into(),
                ordinal: 0,
                shared: false,
                cards: Vec::new(),
                focused: None,
            });
        let _ = app.select_channel(channel);
        let target = app.project_ui.deletions[&id].target.clone();
        let _ = app.handle_project_action(Action::Deleted {
            target,
            result: Ok(()),
        });
        assert_eq!(app.durable.active_channel, Some(channel));
        assert!(app.window_update_blocker().is_none());
    }

    /// A load that reached this repository under another project entry, with
    /// a listing taken before the deletion, lands after it. The deletion
    /// started a newer load of the project, so the older listing adds nothing
    /// back.
    #[test]
    fn a_listing_older_than_a_deletion_cannot_bring_the_worktree_back() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let repository = root.join("checkout");
        let linked_path = root.join("linked");
        git(&root, &["init", "-b", "main", "checkout"]);
        git(&repository, &["commit", "--allow-empty", "-m", "initial"]);
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "linked",
                linked_path.to_str().unwrap(),
            ],
        );
        let mut app = super::super::tests::context_inspector_test_app(false);
        let (session, _) = SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("test"),
        )
        .unwrap();
        app.session = Some(session);
        let load = || {
            super::super::load_repository(&repository, None, CancellationToken::default()).unwrap()
        };
        let loaded = |app: &mut Cagents, project_id, repository| {
            let generation = app.repository_generations[&project_id];
            drop(app.update(Message::RepositoryLoaded {
                open_cards: false,
                project_id,
                generation,
                result: Ok(Box::new(repository)),
            }));
        };

        let first = load();
        let project = first.project.id;
        let _ = app.repository_load_task(repository.clone(), Some(project), false);
        loaded(&mut app, project, first);
        let id = app.sidebar_model.projects[0]
            .worktrees
            .iter()
            .find(|worktree| worktree.path == linked_path)
            .unwrap()
            .id;

        // Re-adding the repository starts a load under a new entry, and its
        // listing is taken while the worktree still exists.
        let alias = ProjectId::new();
        let _ = app.repository_load_task(repository.clone(), Some(alias), false);
        let stale = load();

        let _ = app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: Some(id),
            command: ProjectCommand::Delete,
        });
        let _ = app.handle_project_action(Action::ConfirmDelete);
        git(
            &repository,
            &["worktree", "remove", linked_path.to_str().unwrap()],
        );
        let target = app.project_ui.deletions[&id].target.clone();
        let _ = app.handle_project_action(Action::Deleted {
            target,
            result: Ok(()),
        });
        loaded(&mut app, alias, stale);

        assert!(
            row(&app, id).is_none(),
            "the stale listing re-added the row"
        );
        let session = app.session.as_ref().unwrap();
        session.flush(Duration::from_secs(5)).unwrap();
        assert!(
            !session
                .operation_store()
                .load_worktree(id)
                .unwrap()
                .unwrap()
                .existence
                .is_open(),
            "the stale listing reopened the catalog row"
        );
    }

    fn git(directory: &Path, arguments: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(directory)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// With the catalog no longer consulted, Git's listing is the only guard
    /// before the directory is removed. Each refusal leaves it in place. The
    /// removal itself is not exercised: it first closes sessions through the
    /// daemon, which a test must not reach.
    #[test]
    fn deletion_refuses_whatever_git_does_not_confirm_as_removable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let repository = root.join("checkout");
        let linked = root.join("linked");
        git(&root, &["init", "-b", "main", "checkout"]);
        git(&repository, &["commit", "--allow-empty", "-m", "initial"]);
        git(
            &repository,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        let records = RepositoryService::system("git")
            .list_worktrees(&repository, &CancellationToken::default())
            .unwrap();
        let id_of = |path: &Path| {
            records
                .iter()
                .find(|record| record.path == path)
                .unwrap()
                .id
        };
        let target = |worktree: Option<WorktreeId>, path: &Path| Target {
            project: ProjectId::new(),
            worktree,
            repository: repository.clone(),
            path: path.to_path_buf(),
            label: "linked".into(),
        };
        let linked_id = id_of(&linked);
        let refusal = |target: Target| delete_worktree(&target).unwrap_err();

        assert_eq!(
            refusal(target(None, &repository)),
            "The primary repository cannot be deleted."
        );
        assert_eq!(
            refusal(target(Some(id_of(&repository)), &repository)),
            "The primary worktree cannot be deleted."
        );
        assert_eq!(
            refusal(target(Some(WorktreeId::new()), &linked)),
            "Worktree changed or disappeared; refresh before deleting."
        );
        assert_eq!(
            refusal(target(Some(linked_id), &root.join("elsewhere"))),
            "Worktree changed or disappeared; refresh before deleting."
        );

        std::fs::write(linked.join("scratch"), "work").unwrap();
        assert_eq!(
            refusal(target(Some(linked_id), &linked)),
            "The worktree has changes; commit, stash, or discard them first."
        );
        std::fs::remove_file(linked.join("scratch")).unwrap();

        git(&repository, &["worktree", "lock", linked.to_str().unwrap()]);
        assert_eq!(
            refusal(target(Some(linked_id), &linked)),
            "The worktree is locked; unlock it explicitly in Git first."
        );
        assert!(linked.is_dir());

        // A bare repository lists itself first, as the primary it is.
        let bare = root.join("bare.git");
        git(&root, &["init", "--bare", "bare.git"]);
        let bare_id = RepositoryService::system("git")
            .list_worktrees(&bare, &CancellationToken::default())
            .unwrap()
            .into_iter()
            .find(|record| record.bare)
            .unwrap()
            .id;
        assert_eq!(
            delete_worktree(&Target {
                repository: bare.clone(),
                ..target(Some(bare_id), &bare)
            })
            .unwrap_err(),
            "The primary worktree cannot be deleted."
        );
        assert!(bare.is_dir());
    }
}
