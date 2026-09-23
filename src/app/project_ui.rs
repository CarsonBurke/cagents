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
    editors: Vec<crate::external_editor::Editor>,
}

impl State {
    pub fn modal_open(&self) -> bool {
        self.dialog.is_some()
    }
    pub fn deleting(&self) -> bool {
        self.dialog.as_ref().is_some_and(|dialog| dialog.pending)
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
    pending: bool,
    /// A pre-flight refusal: deletion cannot proceed, so the confirm button
    /// stays disabled until the user resolves it outside the dialog.
    blocker: Option<String>,
    /// A failed attempt. `ConfirmDelete` is written to be re-entered, so this
    /// must be displayed without disabling the retry.
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
                let Some(dialog) = &mut self.project_ui.dialog else {
                    return Task::none();
                };
                if dialog.target.worktree != target.worktree
                    || dialog.target.path != target.path
                    || !dialog.pending
                {
                    return Task::none();
                }
                dialog.pending = false;
                return self.handle_project_action(Action::ConfirmDelete);
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
                if self.project_ui.deleting()
                    || (self.modal_is_open() && !self.project_ui.modal_open())
                {
                    return Task::none();
                }
                let Some(target) = self.project_target(project_id, worktree_id) else {
                    return Task::none();
                };
                match command {
                    ProjectCommand::OpenEditor(editor_id) => {
                        return self.launch_editor(target.path, &editor_id);
                    }
                    ProjectCommand::Delete => {
                        let blocker = self.worktree_deletion_blocker(&target);
                        self.project_ui.dialog = Some(Dialog {
                            target,
                            pending: false,
                            blocker,
                            failure: None,
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
                if self.project_ui.deleting() {
                    return Task::none();
                }
                self.project_ui.dialog = None;
                let focus = self.nested_modal_focus_target().unwrap_or_else(|| {
                    self.workspace
                        .focused()
                        .map(|card| self.card_keyboard_focus(card))
                        .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID))
                });
                return Task::batch([
                    iced::widget::operation::focus(focus).map(cosmic::Action::App),
                    self.lifecycle_sweep(),
                    self.sync_chrome(),
                ]);
            }
            Action::ConfirmDelete => {
                let Some(dialog) = &self.project_ui.dialog else {
                    return Task::none();
                };
                if dialog.pending {
                    return Task::none();
                }
                let target = dialog.target.clone();
                if let Some(blocker) = self.worktree_deletion_blocker(&target) {
                    let dialog = self.project_ui.dialog.as_mut().unwrap();
                    dialog.blocker = Some(blocker);
                    dialog.failure = None;
                    return Task::none();
                }
                let dialog = self.project_ui.dialog.as_mut().unwrap();
                dialog.pending = true;
                dialog.blocker = None;
                dialog.failure = None;
                if self.durable.cards.iter().any(|card| {
                    card.existence.is_open()
                        && card_belongs_to(card, &target.path)
                        && (self.pending_terminal_spawn(card.id)
                            || self.chat_start_pending(card.id))
                }) {
                    return cosmic::task::future(async move {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Message::ProjectUi(Action::AwaitSessions(target))
                    });
                }
                return cosmic::task::future(async move {
                    let request = target.clone();
                    let result = tokio::task::spawn_blocking(move || delete_worktree(&request))
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result);
                    Message::ProjectUi(Action::Deleted { target, result })
                });
            }
            Action::Deleted { target, result } => match result {
                Err(failure) => {
                    // Re-check first: the failure may have been caused by state
                    // that has since become a hard blocker.
                    let blocker = self.worktree_deletion_blocker(&target);
                    if let Some(dialog) = &mut self.project_ui.dialog {
                        dialog.pending = false;
                        dialog.blocker = blocker;
                        dialog.failure = Some(failure);
                    }
                }
                Ok(()) => {
                    self.project_ui.dialog = None;
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
                    let scope = if self.active_workspace.as_ref() == Some(&target.path) {
                        target.repository.clone()
                    } else {
                        self.active_workspace
                            .clone()
                            .unwrap_or_else(|| target.repository.clone())
                    };
                    let select = self.select_worktree(scope, false);
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
        // A failed attempt leaves the confirm button live: `ConfirmDelete`
        // re-checks every precondition, so a transient git failure must not
        // force the user to dismiss and re-open the dialog to retry.
        let confirm = (!dialog.pending && dialog.blocker.is_none())
            .then_some(Message::ProjectUi(Action::ConfirmDelete));
        rows.push(
            widget::row::with_children(vec![
                iced::widget::Space::new().width(Length::Fill).into(),
                widget::button::standard("Cancel")
                    .id(widget::Id::new("dialog.project.close"))
                    .on_press_maybe(
                        (!dialog.pending).then_some(Message::ProjectUi(Action::Dismiss)),
                    )
                    .into(),
                widget::button::destructive(if dialog.pending {
                    "Deleting…"
                } else if dialog.failure.is_some() {
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
        let records = service
            .list_worktrees(&target.repository, &cancellation)
            .map_err(|_| error.to_string())?;
        if records.iter().any(|record| record.id == id) {
            return Err(error.to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A worktree deleted before the catalog ever stored it is still recorded
    /// as deleted, and the tombstone lands after a save of it as open that was
    /// still queued (#245).
    #[test]
    fn deletion_records_its_tombstone_without_waiting_for_the_catalog() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap();
        let (session, _) = SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("test"),
        )
        .unwrap();
        app.session = Some(session);
        let project = ProjectId::new();
        let repository = PathBuf::from("/repo");
        let linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            repository.join("worktrees/feature"),
            Some("feature".into()),
        );
        let id = linked.id;
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            repository.clone(),
            vec![
                SidebarWorktree::from_durable(
                    WorktreeId::new(),
                    repository.clone(),
                    Some("main".into()),
                ),
                linked.clone(),
            ],
            Vec::new(),
        ));
        let session = app.session.as_ref().unwrap();
        session.save_project(ProjectState {
            id: project,
            generation: 0,
            name: "repo".into(),
            repository: repository.clone(),
            existence: Existence::Open,
        });

        let _ = app.handle_project_action(Action::Command {
            project_id: project,
            worktree_id: Some(id),
            command: ProjectCommand::Delete,
        });
        let target = app.project_ui.dialog.as_ref().unwrap().target.clone();
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
        assert!(app.project_ui.dialog.is_none());
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
    }
}
