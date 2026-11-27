//! Native tools for remote files and source control. They never resolve a
//! selected device's path through the local filesystem.
use super::*;
use crate::device_service::{GitOperation, Operation, Reply};
use cosmic::widget::text_editor;
use std::collections::BTreeMap;

pub(super) fn file_editor_id() -> widget::Id {
    widget::Id::new("device-file-editor")
}

/// What a device window does instead of a sidebar action's local handling.
/// Actions with no route here need nothing from this computer's files or Git
/// and run as they do locally: channels, panes and projects live in the
/// device's registry, and terminals and chats start on the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SidebarRoute {
    /// The device's file browser stands in for this computer's file manager.
    BrowseOnDevice {
        project_id: ProjectId,
        worktree_id: Option<crate::model::WorktreeId>,
    },
    /// The device's worktree tools stand in for the worktree dialogs, which
    /// run Git on this computer (#688). `remove` names a worktree to delete.
    DeviceWorktrees {
        project_id: ProjectId,
        remove: Option<crate::model::WorktreeId>,
    },
    /// The device's Changes or Files panel stands in for the source control
    /// and file lists, which read this computer's checkout.
    DeviceTool(Panel),
    /// Deleting the selected rows depends on what they are.
    DeleteSelectedRows,
    /// A checkout's pull request opens on its forge; creating one needs `gh`.
    PullRequest {
        project_id: ProjectId,
        worktree_id: Option<crate::model::WorktreeId>,
    },
    /// A pull request opens on its forge rather than in the local review.
    OpenLink(Url),
    /// Not available on a device yet. The reason is shown as it is.
    Unavailable(&'static str),
}

/// Routes a sidebar action in a device window, or `None` to run it as
/// locally. Nothing here opens the device tools for an action that has
/// nothing to do with them: a command either runs, uses its device
/// equivalent, or says why it can't.
pub(super) fn sidebar_route(action: &SidebarAction) -> Option<SidebarRoute> {
    use crate::sidebar::ProjectCommand;
    use sidebar::SelectionRow;
    // Exhaustive, so a new action has to be classified here.
    Some(match action {
        SidebarAction::ReorderMember { .. }
        | SidebarAction::ReorderProject { .. }
        | SidebarAction::SearchChanged(_)
        | SidebarAction::SetFilter(_)
        | SidebarAction::HideReadAttention
        | SidebarAction::ToggleInProgress
        | SidebarAction::ToggleControls
        | SidebarAction::OpenFirstSearchResult
        | SidebarAction::FocusFirstSearchResult
        | SidebarAction::Resize { .. }
        | SidebarAction::ResizeFinished
        | SidebarAction::ScrollProjects(_)
        | SidebarAction::NavigateRow { .. }
        | SidebarAction::OpenChannel { .. }
        | SidebarAction::OpenCard { .. }
        | SidebarAction::SelectRows { .. }
        | SidebarAction::ClearRowSelection
        | SidebarAction::OpenRepository { .. }
        | SidebarAction::OpenWorktree { .. }
        | SidebarAction::Refresh
        | SidebarAction::RefreshProject { .. }
        | SidebarAction::ToggleProjects
        | SidebarAction::ToggleProject(_)
        | SidebarAction::ToggleWorktrees(_)
        | SidebarAction::SetWorktreeDisplay { .. }
        | SidebarAction::ToggleCheckouts
        | SidebarAction::DragWindow
        | SidebarAction::MaximizeWindow
        | SidebarAction::WindowMenu
        | SidebarAction::OpenSettings
        | SidebarAction::OpenAutomations
        | SidebarAction::OpenWork
        | SidebarAction::OpenRepositoryPicker
        | SidebarAction::NewChannel { .. }
        | SidebarAction::RenameChannel(_)
        | SidebarAction::MarkChannelRead(_)
        | SidebarAction::DeleteChannel { .. }
        | SidebarAction::RetryChannelDeletion { .. }
        | SidebarAction::DeleteRow(SelectionRow::Channel(_) | SelectionRow::Card(_))
        // Only a local worktree operation can be dismissed, and dismissing or
        // cancelling a confirmation changes nothing outside this window.
        | SidebarAction::DismissWorktreeOperation(_)
        | SidebarAction::CancelDiscard
        | SidebarAction::CancelPublish => {
            return None;
        }
        SidebarAction::ProjectCommand {
            project_id,
            worktree_id,
            command,
        } => match command {
            ProjectCommand::NewTerminal
            | ProjectCommand::NewChannel
            | ProjectCommand::CopyPath
            | ProjectCommand::CopyBranch
            | ProjectCommand::MoveUp
            | ProjectCommand::MoveDown
            | ProjectCommand::ResetSidebarOrder => return None,
            ProjectCommand::Reveal => SidebarRoute::BrowseOnDevice {
                project_id: *project_id,
                worktree_id: *worktree_id,
            },
            ProjectCommand::NewWorktree => SidebarRoute::DeviceWorktrees {
                project_id: *project_id,
                remove: None,
            },
            ProjectCommand::Delete => SidebarRoute::DeviceWorktrees {
                project_id: *project_id,
                remove: *worktree_id,
            },
            ProjectCommand::NewWorktreeFromBranch => SidebarRoute::Unavailable(
                "Starting a worktree from an existing branch isn't available in a device window \
                 yet. Use New worktree, or git worktree in a terminal on the device.",
            ),
            ProjectCommand::Remove => SidebarRoute::Unavailable(REMOVE_PROJECT_UNAVAILABLE),
            ProjectCommand::PullRequest => SidebarRoute::PullRequest {
                project_id: *project_id,
                worktree_id: *worktree_id,
            },
            ProjectCommand::RepositoryUrl => SidebarRoute::Unavailable(PULL_REQUESTS_UNAVAILABLE),
            ProjectCommand::OpenEditor(_) => SidebarRoute::Unavailable(
                "External editors open files on this computer. To edit a file on the device, \
                 use Browse device files in the device menu.",
            ),
        },
        SidebarAction::NewWorktree { project_id, .. } => SidebarRoute::DeviceWorktrees {
            project_id: *project_id,
            remove: None,
        },
        SidebarAction::DeleteRow(SelectionRow::Worktree(project_id, worktree_id)) => {
            SidebarRoute::DeviceWorktrees {
                project_id: *project_id,
                remove: Some(*worktree_id),
            }
        }
        SidebarAction::DeleteRow(SelectionRow::Project(_)) => {
            SidebarRoute::Unavailable(REMOVE_PROJECT_UNAVAILABLE)
        }
        SidebarAction::DeleteSelectedRows => SidebarRoute::DeleteSelectedRows,
        SidebarAction::OpenPullRequest { url, .. } => SidebarRoute::OpenLink(url.clone()),
        SidebarAction::BrowsePullRequests { .. } | SidebarAction::CreatePullRequest { .. } => {
            SidebarRoute::Unavailable(PULL_REQUESTS_UNAVAILABLE)
        }
        // A device's worktrees are created and removed in Device worktrees,
        // so there is no local operation to resume.
        SidebarAction::CancelWorktreeOperation(_)
        | SidebarAction::EditWorktreeOperation(_)
        | SidebarAction::RetryWorktreeOperation(_)
        | SidebarAction::RetryWorktreeDeletion { .. } => SidebarRoute::Unavailable(
            "A device's worktrees are managed in Device worktrees, in the device menu.",
        ),
        SidebarAction::OpenFileInEditor { .. }
        | SidebarAction::FileCommand { .. }
        | SidebarAction::ToggleFilesFilter { .. }
        | SidebarAction::FilesPage { .. }
        | SidebarAction::FilesQueryChanged { .. }
        | SidebarAction::ToggleDirectory { .. }
        | SidebarAction::OpenFile { .. }
        | SidebarAction::OpenBranchFile { .. } => SidebarRoute::DeviceTool(Panel::Files),
        SidebarAction::ChangeGroup { .. }
        | SidebarAction::ChangePage { .. }
        | SidebarAction::OpenCommit { .. }
        | SidebarAction::CloseCommit { .. }
        | SidebarAction::ShowMoreCommitFiles { .. }
        | SidebarAction::OpenCommitFile { .. }
        | SidebarAction::ToggleSection { .. }
        | SidebarAction::ToggleSourceControlPanel { .. }
        | SidebarAction::RefreshWorktree { .. }
        | SidebarAction::SetChangeListMode { .. }
        | SidebarAction::SetChangesTab { .. }
        | SidebarAction::SetWorktreePanel { .. }
        | SidebarAction::ToggleChangeDirectory { .. }
        | SidebarAction::OpenChange { .. }
        | SidebarAction::Stage { .. }
        | SidebarAction::Unstage { .. }
        | SidebarAction::RequestDiscard { .. }
        | SidebarAction::ConfirmDiscard { .. }
        | SidebarAction::CommitMessageChanged { .. }
        | SidebarAction::CommitSubjectEditorCursor { .. }
        | SidebarAction::CommitEditorCursor { .. }
        | SidebarAction::CommitSubjectEditorAction { .. }
        | SidebarAction::CommitEditorAction { .. }
        | SidebarAction::AmendChanged { .. }
        | SidebarAction::Commit { .. }
        | SidebarAction::Push { .. }
        | SidebarAction::OpenPublish { .. }
        | SidebarAction::PublishRemoteChanged { .. }
        | SidebarAction::PublishBranchChanged { .. }
        | SidebarAction::LoadMoreHistory { .. } => SidebarRoute::DeviceTool(Panel::Changes),
    })
}

const REMOVE_PROJECT_UNAVAILABLE: &str = "Removing a project from a device window isn't \
     available yet. Remove it from a window on that computer instead.";
const CHECKOUT_NOT_LOADED: &str =
    "That checkout hasn't loaded from the device yet. Try again in a moment.";
const PULL_REQUESTS_UNAVAILABLE: &str =
    "Pull requests aren't available in a device window yet. Run gh in a terminal on the device.";

#[derive(Debug, Clone)]
pub enum Action {
    Menu(bool),
    Open,
    OpenAt(PathBuf),
    OpenIn {
        path: Option<PathBuf>,
        panel: Panel,
    },
    OpenFile {
        path: PathBuf,
        fragment: Option<String>,
    },
    Configure,
    ConfigName(String),
    ConfigHost(String),
    ConfigExecutable(String),
    ConfigSave,
    ConfigSaved(Result<(), String>),
    Close,
    BackToFile,
    Panel(Panel),
    DiscardChanges,
    Path(String),
    Browse,
    Visit(PathBuf),
    Read(PathBuf),
    Edit(text_editor::Action),
    Save,
    Git(GitOperation),
    Pathspec(String),
    WorktreeName(String),
    WorktreeBranch(String),
    CommitMessage(String),
    OpenProject,
    Loaded {
        generation: u64,
        result: Result<Reply, String>,
    },
    ImageLoaded {
        generation: u64,
        result: Result<iced::widget::image::Handle, String>,
    },
    Device(usize),
    OpenDeviceWindow(usize),
    DeviceOpened {
        result: Result<(), String>,
        replace: bool,
    },
}
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    #[default]
    Files,
    Changes,
    Worktrees,
}

impl Panel {
    fn label(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Changes => "Changes",
            Self::Worktrees => "Worktrees",
        }
    }
}

pub(super) struct State {
    pub open: bool,
    pub menu_open: bool,
    configure: bool,
    config_name: String,
    config_host: String,
    config_executable: String,
    pub path: String,
    directory: PathBuf,
    entries: Vec<crate::device_service::Entry>,
    file: Option<PathBuf>,
    original: String,
    content: text_editor::Content<cosmic::Renderer>,
    output: String,
    worktree_output: String,
    panel: Panel,
    pathspec: String,
    pathspec_auto: bool,
    worktree_name: String,
    worktree_branch: String,
    commit_message: String,
    busy: bool,
    generation: u64,
    pending: Option<Operation>,
    image: Option<ImageRequest>,
    linked_file: Option<PathBuf>,
    linked_fragment: Option<String>,
    notice: Option<String>,
    pub devices: Vec<String>,
    device_hosts: BTreeMap<String, String>,
}
struct ImageRequest {
    path: PathBuf,
    device: Option<String>,
    // Abort queued/waiting work when the dialog closes. Blocking work retains
    // its worker permit until it actually finishes, even after cancellation.
    task: iced::task::Handle,
}
impl Default for State {
    fn default() -> Self {
        let devices = crate::device::list().unwrap_or_default();
        Self {
            open: false,
            menu_open: false,
            configure: false,
            config_name: String::new(),
            config_host: String::new(),
            config_executable: "cagents".into(),
            path: String::new(),
            directory: PathBuf::new(),
            entries: Vec::new(),
            file: None,
            original: String::new(),
            content: text_editor::Content::new(),
            output: String::new(),
            worktree_output: String::new(),
            panel: Panel::Files,
            pathspec: String::new(),
            pathspec_auto: true,
            worktree_name: String::new(),
            worktree_branch: String::new(),
            commit_message: String::new(),
            busy: false,
            generation: 0,
            pending: None,
            image: None,
            linked_file: None,
            linked_fragment: None,
            notice: None,
            devices: std::iter::once("local".to_owned())
                .chain(devices.iter().map(|device| device.name.clone()))
                .collect(),
            device_hosts: devices
                .into_iter()
                .map(|device| (device.name, device.host))
                .collect(),
        }
    }
}
impl Cagents {
    pub(super) fn device_project_read_task(
        &mut self,
        repository: PathBuf,
        project_id: ProjectId,
        generation: u64,
        kind: Option<ProjectKind>,
    ) -> Task<Message> {
        self.workspace_model_opens.insert(repository.clone());
        self.refresh_ui.schedule_remote_started(project_id);
        self.refresh_ui
            .projects
            .insert(project_id, CancellationToken::new());
        let kind = kind
            .or_else(|| {
                self.sidebar_model
                    .project(project_id)
                    .map(|project| project.kind)
            })
            .unwrap_or_default();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                crate::device_service::request(Operation::ProjectNavigation { repository, kind })
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result)
            .and_then(|reply| match reply {
                Reply::ProjectNavigation(navigation) => Ok(navigation),
                _ => Err("Unexpected project navigation reply".into()),
            });
            Message::DeviceProjectLoaded {
                project_id,
                generation,
                result,
            }
        })
    }

    pub(super) fn device_project_loaded(
        &mut self,
        project_id: ProjectId,
        generation: u64,
        result: Result<crate::device_service::Navigation, String>,
    ) -> Task<Message> {
        if self.repository_generations.get(&project_id) != Some(&generation) {
            return Task::none();
        }
        self.refresh_ui.projects.remove(&project_id);
        let Some(project) = self
            .sidebar_model
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        else {
            return Task::none();
        };
        if self.project_ui.closed.contains(&project.repository) {
            return Task::none();
        }
        let navigation = match result {
            Ok(navigation) if navigation.repository == project.repository => navigation,
            Ok(_) => {
                self.status = Some(TransientNotice::warning(
                    "The device returned a different project identity",
                ));
                return Task::none();
            }
            Err(error) => {
                project.unreachable = Some(error.clone());
                project.reloading = false;
                self.refresh_ui.finished(project_id, false);
                self.status = Some(TransientNotice::warning(error));
                return Task::none();
            }
        };
        let records = navigation
            .worktrees
            .into_iter()
            .map(|record| WorktreeRecord {
                id: crate::repository::stable_worktree_id(&record.path),
                path: record.path,
                branch: record.branch,
                head_oid: record.head_oid,
                detached: record.detached,
                bare: record.bare,
                lock: crate::repository::WorktreeLock {
                    locked: record.locked,
                    reason: record.lock_reason,
                },
                prunable_reason: record.prunable_reason,
                created_at_unix_ms: None,
            })
            .collect::<Vec<_>>();
        if let Some(session) = &self.session {
            session.save_worktrees(records.iter().map(|record| WorktreeState {
                id: record.id,
                project_id,
                generation: 0,
                path: record.path.clone(),
                branch: record.branch.as_deref().map(|branch| {
                    branch
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch)
                        .to_owned()
                }),
                existence: Existence::Open,
            }));
        }
        project.worktrees = records
            .iter()
            .map(|record| SidebarWorktree::from_repository(record, None))
            .collect();
        project.unreachable = None;
        project.reloading = false;
        self.refresh_ui.finished(project_id, true);
        self.refresh_sidebar_channels();
        self.reconcile_shared_views()
    }

    /// Carries out a device window's [`SidebarRoute`] for a sidebar action.
    pub(super) fn route_device_sidebar_action(&mut self, route: SidebarRoute) -> Task<Message> {
        match route {
            SidebarRoute::BrowseOnDevice {
                project_id,
                worktree_id,
            } => {
                let Some(project) = self.sidebar_model.project(project_id) else {
                    return Task::none();
                };
                let path = match worktree_id {
                    Some(id) => match project.worktrees.iter().find(|worktree| worktree.id == id) {
                        Some(worktree) => worktree.path.clone(),
                        None => {
                            self.status = Some(TransientNotice::warning(CHECKOUT_NOT_LOADED));
                            return Task::none();
                        }
                    },
                    None => worktree_ui::project_main_checkout(project),
                };
                self.update_device(Action::OpenIn {
                    path: Some(path),
                    panel: Panel::Files,
                })
            }
            SidebarRoute::DeviceWorktrees { project_id, remove } => {
                let Some(project) = self.sidebar_model.project(project_id) else {
                    return Task::none();
                };
                if project.kind == ProjectKind::Folder {
                    self.status = Some(TransientNotice::warning(
                        "This project is a folder, not a Git repository, so it has no worktrees.",
                    ));
                    return Task::none();
                }
                // The main checkout, where the device creates `.worktrees/`.
                let main = worktree_ui::project_main_checkout(project);
                let name = match remove {
                    None => String::new(),
                    Some(id) => {
                        let Some(worktree) =
                            project.worktrees.iter().find(|worktree| worktree.id == id)
                        else {
                            self.status = Some(TransientNotice::warning(CHECKOUT_NOT_LOADED));
                            return Task::none();
                        };
                        if worktree.path == main {
                            self.status = Some(TransientNotice::warning(
                                "The main checkout can't be deleted. Remove the project instead.",
                            ));
                            return Task::none();
                        }
                        // The device removes only `.worktrees/<name>` checkouts.
                        let Some(name) = worktree
                            .path
                            .strip_prefix(main.join(".worktrees"))
                            .ok()
                            .filter(|name| name.components().count() == 1)
                            .map(|name| name.display().to_string())
                        else {
                            self.status = Some(TransientNotice::warning(
                                "A device window can only delete worktrees in the project's \
                                 .worktrees folder. Use git worktree remove in a terminal on \
                                 the device.",
                            ));
                            return Task::none();
                        };
                        name
                    }
                };
                let task = self.update_device(Action::OpenIn {
                    path: Some(main),
                    panel: Panel::Worktrees,
                });
                // A busy or unsaved device dialog stays as it was, name and all.
                if self.device_ui.open && self.device_ui.panel == Panel::Worktrees {
                    self.device_ui.worktree_name = name;
                }
                task
            }
            SidebarRoute::DeviceTool(panel) => self.update_device(Action::OpenIn {
                path: self.active_workspace.clone(),
                panel,
            }),
            SidebarRoute::DeleteSelectedRows => {
                let visible = self.visible_sidebar_rows();
                let selected = self.sidebar_state.selected_rows(&visible);
                let worktrees = selected
                    .iter()
                    .filter(|row| matches!(row, sidebar::SelectionRow::Worktree(..)))
                    .count();
                if selected
                    .iter()
                    .any(|row| matches!(row, sidebar::SelectionRow::Project(_)))
                {
                    self.status = Some(TransientNotice::warning(REMOVE_PROJECT_UNAVAILABLE));
                    return Task::none();
                }
                match (worktrees, selected.as_slice()) {
                    // Channels and panes delete as they do locally.
                    (0, _) => self.begin_sidebar_group_delete(),
                    (1, [sidebar::SelectionRow::Worktree(project_id, worktree_id)]) => self
                        .route_device_sidebar_action(SidebarRoute::DeviceWorktrees {
                            project_id: *project_id,
                            remove: Some(*worktree_id),
                        }),
                    _ => {
                        self.status = Some(TransientNotice::warning(
                            "A device window deletes one worktree at a time. Select a single \
                             worktree, or only channels.",
                        ));
                        Task::none()
                    }
                }
            }
            SidebarRoute::PullRequest {
                project_id,
                worktree_id,
            } => {
                let url = self
                    .sidebar_model
                    .project(project_id)
                    .and_then(|project| {
                        project
                            .worktrees
                            .iter()
                            .find(|worktree| Some(worktree.id) == worktree_id)
                    })
                    .and_then(|row| row.pull_request())
                    .map(|pr| pr.url.clone());
                match url {
                    Some(url) => self.open_clicked_link(url),
                    None => {
                        self.status = Some(TransientNotice::warning(PULL_REQUESTS_UNAVAILABLE));
                        Task::none()
                    }
                }
            }
            SidebarRoute::OpenLink(url) => self.open_clicked_link(url),
            SidebarRoute::Unavailable(reason) => {
                self.status = Some(TransientNotice::warning(reason));
                Task::none()
            }
        }
    }

    pub(super) fn update_device(&mut self, action: Action) -> Task<Message> {
        if let Action::Menu(open) = action {
            self.device_ui.menu_open =
                open && self.settings.show_status_bar && !self.modal_is_open();
            if self.device_ui.menu_open {
                self.dismiss_chat_picker();
                self.phone_ui.open = false;
                match crate::device::list() {
                    Ok(devices) => {
                        self.device_ui.set_devices(devices);
                    }
                    Err(error) => self.status = Some(TransientNotice::warning(error.to_string())),
                }
            }
            return Task::none();
        }
        self.device_ui.menu_open = false;
        if self.device_ui.busy
            && !matches!(
                &action,
                Action::Loaded { .. }
                    | Action::ImageLoaded { .. }
                    | Action::Device(_)
                    | Action::OpenDeviceWindow(_)
                    | Action::DeviceOpened { .. }
                    | Action::ConfigSaved(_)
            )
            && !(matches!(&action, Action::Close) && self.device_ui.image.is_some())
        {
            return Task::none();
        }
        if matches!(
            &action,
            Action::Close
                | Action::Open
                | Action::OpenAt(_)
                | Action::OpenIn { .. }
                | Action::OpenFile { .. }
                | Action::Configure
                | Action::Browse
                | Action::Visit(_)
                | Action::Read(_)
                | Action::OpenProject
        ) && self.device_ui.dirty()
        {
            self.device_ui.notice = Some(
                "Unsaved changes: return to the file and save, or discard changes before continuing.".into(),
            );
            return Task::none();
        }
        let operation = match action {
            Action::Menu(_) => unreachable!("device menu handled above"),
            Action::Configure => {
                self.device_ui.open = true;
                self.device_ui.configure = true;
                self.device_ui.notice = None;
                self.device_ui.config_name.clear();
                self.device_ui.config_host.clear();
                self.device_ui.config_executable = "cagents".into();
                return Task::none();
            }
            Action::ConfigName(value) => {
                self.device_ui.config_name = value;
                return Task::none();
            }
            Action::ConfigHost(value) => {
                self.device_ui.config_host = value;
                return Task::none();
            }
            Action::ConfigExecutable(value) => {
                self.device_ui.config_executable = value;
                return Task::none();
            }
            Action::ConfigSave => {
                if let Some(current) = crate::device::selected()
                    && current.name == self.device_ui.config_name
                    && (current.host != self.device_ui.config_host
                        || current.executable != self.device_ui.config_executable)
                {
                    self.device_ui.notice = Some("This device is already open. Use a different device name for another connection.".into());
                    return Task::none();
                }
                let device = crate::device::Device {
                    name: self.device_ui.config_name.clone(),
                    host: self.device_ui.config_host.clone(),
                    executable: self.device_ui.config_executable.clone(),
                };
                self.device_ui.busy = true;
                return cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        crate::device::save(device).map_err(|error| error.to_string())
                    })
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()));
                    Message::Device(Action::ConfigSaved(result))
                });
            }
            Action::ConfigSaved(result) => {
                self.device_ui.busy = false;
                match result {
                    Ok(()) => {
                        match crate::device::list() {
                            Ok(devices) => self.device_ui.set_devices(devices),
                            Err(error) => {
                                self.device_ui.notice = Some(format!(
                                    "Saved, but the device list could not be read: {error}"
                                ));
                                return Task::none();
                            }
                        }
                        self.device_ui.open = false;
                        if let Some(index) = self
                            .device_ui
                            .devices
                            .iter()
                            .position(|name| name == &self.device_ui.config_name)
                        {
                            return self.update_device(Action::Device(index));
                        }
                    }
                    Err(error) => self.device_ui.notice = Some(error),
                }
                return Task::none();
            }
            Action::DeviceOpened { result, replace } => {
                self.device_ui.busy = false;
                match result {
                    Ok(()) if replace => return self.exit_after_language_servers(),
                    Ok(()) => {}
                    Err(error) => self.status = Some(TransientNotice::warning(error)),
                }
                return Task::none();
            }
            Action::Device(index) | Action::OpenDeviceWindow(index) => {
                let replace = matches!(action, Action::Device(_));
                let Some(name) = self.device_ui.devices.get(index).cloned() else {
                    return Task::none();
                };
                if replace
                    && name
                        == crate::device::selected()
                            .map(|device| device.name.as_str())
                            .unwrap_or("local")
                {
                    return Task::none();
                }
                if replace && (self.device_ui.dirty() || self.cards.values().any(|runtime| matches!(runtime, CardRuntime::Repository(editor) if editor.is_dirty() || editor.is_saving()))) {
                    self.status = Some(TransientNotice::warning("Save or discard your editor changes before switching devices."));
                    return Task::none();
                }
                let source = (name
                    == crate::device::selected()
                        .map(|device| device.name.as_str())
                        .unwrap_or("local"))
                .then(|| crate::instance::window_slot().to_owned());
                if replace && let Some(message) = self.device_switch_chat_blocker() {
                    self.status = Some(TransientNotice::warning(message));
                    return Task::none();
                }
                self.persist();
                self.device_ui.busy = true;
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let executable = crate::companion::Product::Cli.beside_current().map_err(|error| error.to_string())?;
                            let status = std::process::Command::new(executable).args(["--device", &name, "ui", "--new-window"]).args(source.as_ref().map(|source| ["--source", source.as_str()]).into_iter().flatten()).status().map_err(|error| error.to_string())?;
                            if status.success() { Ok(()) } else { Err(format!("Could not connect to {name}. Check its SSH connection and cagents daemon.")) }
                        }).await.unwrap_or_else(|error| Err(error.to_string()))
                    },
                    move |result| {
                        cosmic::Action::App(Message::Device(Action::DeviceOpened {
                            result,
                            replace,
                        }))
                    },
                );
            }
            Action::OpenFile { path, fragment } => {
                let Some((directory, file)) = path.parent().zip(path.file_name()) else {
                    self.status = Some(TransientNotice::warning(
                        "The link does not name a device file",
                    ));
                    return Task::none();
                };
                if crate::chat::is_image_path(&path) {
                    if self.refuse_in_deleting_worktree(&path) {
                        return Task::none();
                    }
                    return self.open_device_image(path);
                }
                self.device_ui.panel = Panel::Files;
                self.device_ui.linked_file = Some(file.into());
                self.device_ui.linked_fragment = fragment;
                self.device_ui.path = directory.display().to_string();
                self.device_ui.open = true;
                self.device_ui.configure = false;
                Some(Operation::List {
                    path: directory.to_owned(),
                })
            }
            Action::OpenIn { path, panel } => {
                if let Some(path) = path {
                    self.device_ui.path = path.display().to_string();
                }
                self.device_ui.panel = panel;
                return self.update_device(Action::Open);
            }
            Action::OpenAt(path) => {
                self.device_ui.path = path.display().to_string();
                return self.update_device(Action::Open);
            }
            Action::Open => {
                self.device_ui.open = true;
                self.device_ui.configure = false;
                self.device_ui.notice = None;
                if self.device_ui.path.is_empty() {
                    self.device_ui.path = crate::device::home()
                        .unwrap_or(&STARTING_DIRECTORY)
                        .display()
                        .to_string();
                }
                Some(Operation::List {
                    path: self.device_ui.path.clone().into(),
                })
            }
            Action::Panel(panel) => {
                if panel == Panel::Changes
                    && self.device_ui.pathspec_auto
                    && let Some(file) = &self.device_ui.file
                {
                    self.device_ui.pathspec = file.display().to_string();
                }
                self.device_ui.panel = panel;
                match panel {
                    Panel::Files => return Task::none(),
                    Panel::Changes => {
                        return self.update_device(Action::Git(GitOperation::Status));
                    }
                    Panel::Worktrees => {
                        return self.update_device(Action::Git(GitOperation::Worktrees));
                    }
                }
            }
            Action::BackToFile => {
                self.device_ui.panel = Panel::Files;
                return Task::none();
            }
            Action::DiscardChanges => {
                self.device_ui.content = text_editor::Content::with_text(&self.device_ui.original);
                self.device_ui.notice = Some("Unsaved changes discarded".into());
                return Task::none();
            }
            Action::Close => {
                let was_open = self.device_ui.open;
                if let Some(image) = self.device_ui.image.take() {
                    image.task.abort();
                    self.device_ui.generation = self.device_ui.generation.wrapping_add(1);
                    self.device_ui.pending = None;
                    self.device_ui.busy = false;
                }
                if !self.device_ui.busy {
                    self.device_ui.open = false;
                    if was_open && self.daemon_health_ui.confirmation {
                        return self.focus_daemon_health_after_cover();
                    }
                }
                return Task::none();
            }
            Action::Path(path) => {
                self.device_ui.path = path;
                return Task::none();
            }
            Action::WorktreeName(value) => {
                self.device_ui.worktree_name = value;
                return Task::none();
            }
            Action::WorktreeBranch(value) => {
                self.device_ui.worktree_branch = value;
                return Task::none();
            }
            Action::Pathspec(value) => {
                self.device_ui.pathspec_auto = false;
                self.device_ui.pathspec = value;
                return Task::none();
            }
            Action::CommitMessage(value) => {
                self.device_ui.commit_message = value;
                return Task::none();
            }
            Action::Edit(action) => {
                if !self.device_ui.busy {
                    self.device_ui.content.perform(action);
                }
                return Task::none();
            }
            Action::Browse => Some(Operation::List {
                path: self.device_ui.path.clone().into(),
            }),
            Action::Visit(path) => Some(Operation::List { path }),
            Action::Read(path) => {
                if crate::chat::is_image_path(&path) {
                    let path = self.device_ui.directory.join(path);
                    if self.refuse_in_deleting_worktree(&path) {
                        return Task::none();
                    }
                    return self.open_device_image(path);
                }
                Some(Operation::Read {
                    root: self.device_ui.directory.clone(),
                    path,
                })
            }
            Action::Save => self.device_ui.file.as_ref().map(|path| Operation::Write {
                root: self.device_ui.directory.clone(),
                path: path.clone(),
                expected: self.device_ui.original.clone(),
                content: self.device_ui.content.text(),
            }),
            Action::Git(operation) => Some(Operation::Git {
                root: self.device_ui.directory.clone(),
                operation,
            }),
            Action::OpenProject => Some(Operation::Discover {
                path: self.device_ui.path.clone().into(),
            }),
            Action::Loaded { generation, result } => {
                if generation != self.device_ui.generation || self.device_ui.image.is_some() {
                    return Task::none();
                }
                self.device_ui.busy = false;
                let pending = self.device_ui.pending.take();
                match result {
                    Err(error) => {
                        self.device_ui.linked_file = None;
                        self.device_ui.linked_fragment = None;
                        self.device_ui.notice = Some(error);
                    }
                    Ok(Reply::Directory { path, entries }) => {
                        self.device_ui.path = path.display().to_string();
                        if self.device_ui.directory != path {
                            self.device_ui.pathspec.clear();
                            self.device_ui.pathspec_auto = true;
                        }
                        self.device_ui.directory = path;
                        self.device_ui.entries = entries;
                        self.device_ui.file = None;
                        self.device_ui.output.clear();
                        self.device_ui.worktree_output.clear();
                        if let Some(file) = self.device_ui.linked_file.clone() {
                            // A linked file may resolve into a different worktree.
                            // Recheck removal against the owning device's path.
                            if self
                                .refuse_in_deleting_worktree(&self.device_ui.directory.join(&file))
                            {
                                self.device_ui.linked_file = None;
                                self.device_ui.linked_fragment = None;
                                return Task::none();
                            }
                            return self.update_device(Action::Read(file));
                        }
                        if self.device_ui.panel != Panel::Files {
                            return self.update_device(Action::Panel(self.device_ui.panel));
                        }
                    }
                    Ok(Reply::Text(text)) if matches!(&pending, Some(Operation::Read { .. })) => {
                        if let Some(Operation::Read { root, path }) = pending {
                            self.device_ui.directory = root;
                            if self.device_ui.pathspec_auto {
                                self.device_ui.pathspec = path.display().to_string();
                            }
                            self.device_ui.file = Some(path);
                        }
                        self.device_ui.original = text.clone();
                        self.device_ui.content = text_editor::Content::with_text(&text);
                        let linked = self.device_ui.linked_file.take().is_some();
                        if let Some(fragment) = self.device_ui.linked_fragment.take() {
                            self.device_ui.jump_to_fragment(&fragment);
                        }
                        self.device_ui.panel = Panel::Files;
                        if linked {
                            return iced::widget::operation::focus(file_editor_id())
                                .map(cosmic::Action::App);
                        }
                    }
                    Ok(Reply::Text(text)) => {
                        if matches!(
                            pending,
                            Some(Operation::Git {
                                operation: GitOperation::Worktrees
                                    | GitOperation::CreateWorktree { .. }
                                    | GitOperation::RemoveWorktree { .. },
                                ..
                            })
                        ) {
                            self.device_ui.worktree_output = text;
                        } else {
                            self.device_ui.output = text;
                        }
                    }
                    Ok(reply @ (Reply::Saved | Reply::SavedUnconfirmed { .. })) => {
                        self.device_ui.original = self.device_ui.content.text();
                        self.device_ui.notice = Some(match reply {
                            Reply::SavedUnconfirmed { error } => format!(
                                "Saved on device; durability could not be confirmed: {error}"
                            ),
                            _ => "Saved on device".into(),
                        });
                    }
                    Ok(Reply::Project { path, key, kind }) => {
                        self.device_ui.path = path.display().to_string();
                        self.device_ui.notice =
                            Some(format!("Project opened on device: {}", path.display()));
                        self.device_ui.directory = path.clone();
                        self.device_ui.pathspec.clear();
                        self.device_ui.pathspec_auto = true;
                        self.active_workspace = Some(path.clone());
                        self.device_ui.file = None;
                        self.device_ui.entries.clear();
                        self.device_ui.output.clear();
                        self.device_ui.worktree_output.clear();
                        self.device_ui.panel = Panel::Files;
                        let edit = crate::workspace_model::ModelEdit::ReopenProject {
                            repository: key,
                            name: path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned(),
                            kind,
                        };
                        self.publish_model_edit(edit);
                        return self.update_device(Action::Browse);
                    }
                    Ok(_) => self.device_ui.notice = Some("Unexpected device response".into()),
                }
                return Task::none();
            }
            Action::ImageLoaded { generation, result } => {
                if generation != self.device_ui.generation {
                    return Task::none();
                }
                let Some(image) = self.device_ui.image.take() else {
                    return Task::none();
                };
                self.device_ui.busy = false;
                self.device_ui.pending = None;
                match result {
                    Ok(handle) => {
                        self.device_ui.open = false;
                        self.chat_ui.image_dialog = Some(chat_ui::images::Gallery::file(
                            image.path,
                            image.device.as_deref(),
                            handle,
                        ));
                    }
                    Err(error) => self.device_ui.notice = Some(error),
                }
                return Task::none();
            }
        };
        let Some(operation) = operation else {
            return Task::none();
        };
        if self.device_ui.busy {
            return Task::none();
        }
        if let Operation::Git { operation, .. } = &operation {
            self.device_ui.panel = match operation {
                GitOperation::Worktrees
                | GitOperation::CreateWorktree { .. }
                | GitOperation::RemoveWorktree { .. } => Panel::Worktrees,
                _ => Panel::Changes,
            };
        }
        self.device_ui.busy = true;
        self.device_ui.pending = Some(operation.clone());
        self.device_ui.generation = self.device_ui.generation.wrapping_add(1);
        self.device_ui.notice = None;
        let generation = self.device_ui.generation;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || crate::device_service::request(operation))
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
            },
            move |result| {
                cosmic::Action::App(Message::Device(Action::Loaded { generation, result }))
            },
        )
    }
    fn open_device_image(&mut self, path: PathBuf) -> Task<Message> {
        if !path.is_absolute() {
            self.status = Some(TransientNotice::error(
                "The image needs an absolute device path.",
            ));
            return Task::none();
        }
        let device = crate::device::selected().map(|device| device.name.clone());
        self.device_ui.open = true;
        self.device_ui.configure = false;
        self.device_ui.notice = None;
        self.device_ui.linked_file = None;
        self.device_ui.linked_fragment = None;
        self.device_ui.generation = self.device_ui.generation.wrapping_add(1);
        let generation = self.device_ui.generation;
        self.device_ui.pending = Some(Operation::ReadImage {
            root: "/".into(),
            path: path.clone(),
        });
        self.device_ui.busy = true;
        let preview_path = path.clone();
        let preview_device = device.clone();
        let (task, handle) = Task::perform(
            chat_ui::images::file_preview(preview_path, preview_device),
            move |result| {
                cosmic::Action::App(Message::Device(Action::ImageLoaded { generation, result }))
            },
        )
        .abortable();
        self.device_ui.image = Some(ImageRequest {
            path,
            device,
            task: handle.abort_on_drop(),
        });
        task
    }
    pub(super) fn device_status_control(&self) -> Element<'_, Message> {
        let selected = crate::device::selected()
            .map(|device| device.name.as_str())
            .unwrap_or("local");
        let label = if selected == "local" {
            "This device"
        } else {
            selected
        };
        let open = self.device_ui.menu_open && !self.modal_is_open();
        let control = widget::button::custom(
            widget::row::with_children(vec![
                design::caption(label).into(),
                widget::icon(Icon::ChevronUp.handle()).size(12).into(),
            ])
            .spacing(space::XS)
            .align_y(Alignment::Center),
        )
        .name(format!("Device: {label}. Open device connections"))
        .selected(open)
        .class(crate::ui_controls::control_style(open, None))
        .height(20)
        .padding(Padding::from([0.0, design::GRID_UNIT * 1.5]))
        .on_press(Message::Device(Action::Menu(!open)));
        let anchor = crate::interaction::tooltip(
            crate::interaction::hover(control),
            "Open device connections",
        );
        crate::ui_controls::popover(
            anchor,
            open.then(|| {
                let enabled = !self.device_ui.busy;
                let mut rows: Vec<Element<'_, Message>> = vec![
                    widget::row::with_children(vec![
                        design::heading("Devices").width(Length::Fill).into(),
                        crate::ui_controls::icon_button(Icon::Plus, "Connect to a device")
                            .on_press_maybe(enabled.then_some(Message::Device(Action::Configure)))
                            .into(),
                    ])
                    .align_y(Alignment::Center)
                    .padding(Padding::from([0.0, space::S]))
                    .into(),
                ];
                let mut choices = Vec::new();
                for (index, name) in self.device_ui.devices.iter().enumerate() {
                    let label = if name == "local" {
                        "This device"
                    } else {
                        name.as_str()
                    };
                    let current = name == selected;
                    let detail = if name == "local" {
                        "Local workspace"
                    } else {
                        self.device_ui
                            .device_hosts
                            .get(name)
                            .map(String::as_str)
                            .unwrap_or("SSH device")
                    };
                    let mut contents: Vec<Element<'_, Message>> = vec![
                        widget::column::with_children(vec![
                            design::body(label).into(),
                            design::caption(detail).into(),
                        ])
                        .width(Length::Fill)
                        .into(),
                    ];
                    if current {
                        contents.push(design::caption("Current").into());
                    }
                    let switch = widget::button::custom(
                        widget::row::with_children(contents)
                            .spacing(space::S)
                            .align_y(Alignment::Center),
                    )
                    .name(format!("Switch to {label}"))
                    .selected(current)
                    .class(crate::ui_controls::control_style(current, None))
                    .width(Length::Fill)
                    .padding(space::S)
                    .on_press_maybe(enabled.then_some(Message::Device(Action::Device(index))));
                    let open = crate::ui_controls::icon_button(
                        Icon::PopOut,
                        format!("Open {label} in another window"),
                    )
                    .on_press_maybe(
                        enabled.then(|| Message::Device(Action::OpenDeviceWindow(index))),
                    );
                    choices.push(
                        widget::row::with_children(vec![
                            crate::interaction::tooltip(
                                crate::interaction::hover(switch),
                                if current {
                                    format!("{label} is open in this window")
                                } else {
                                    format!("Switch this window to {label}")
                                },
                            ),
                            open.into(),
                        ])
                        .align_y(Alignment::Center)
                        .spacing(space::XS)
                        .into(),
                    );
                }
                rows.push(
                    widget::container(crate::interaction::overflow_fade_y(
                        widget::scrollable(
                            widget::column::with_children(choices)
                                .padding(design::with_vertical_rail(Padding::ZERO)),
                        )
                        .direction(iced::widget::scrollable::Direction::Vertical(
                            design::scrollbar(),
                        ))
                        .height(Length::Shrink),
                        // The chooser is a popover, on the elevated surface.
                        design::component_base,
                    ))
                    .max_height(280)
                    .into(),
                );
                if crate::device::is_remote() {
                    rows.push(design::horizontal_rule());
                    rows.push(
                        widget::button::custom(
                            widget::row::with_children(vec![
                                widget::icon(Icon::Folder.handle())
                                    .size(design::ROW_ICON_SIZE)
                                    .into(),
                                design::body("Browse device files").into(),
                            ])
                            .spacing(space::S)
                            .align_y(Alignment::Center),
                        )
                        .name("Browse device files")
                        .class(crate::ui_controls::control_style(false, None))
                        .width(Length::Fill)
                        .padding(space::S)
                        .on_press_maybe(enabled.then_some(Message::Device(Action::Open)))
                        .into(),
                    );
                }
                widget::container(widget::column::with_children(rows).spacing(space::XS))
                    .width(304)
                    .into()
            }),
            widget::popover::Position::Top,
            Message::Device(Action::Menu(false)),
        )
    }

    pub(super) fn device_dialog(&self) -> Element<'_, Message> {
        // Use the tracked window geometry: Responsive hides its descendants from AccessKit.
        let padding = space::XXL;
        self.device_dialog_layout(iced::Size::new(
            (self.window_width - padding).max(1.0),
            (self.window_height - padding).max(1.0),
        ))
    }

    fn device_dialog_layout(&self, size: iced::Size) -> Element<'_, Message> {
        let state = &self.device_ui;
        if let Some(image) = &state.image {
            // However long the path, Cancel stays in reach.
            return dialog_ui::scrolling_surface_with_actions(
                widget::column::with_children(vec![
                    design::heading("Opening image").into(),
                    design::body(image.path.display().to_string()).into(),
                    design::caption(image.device.as_deref().unwrap_or("This device")).into(),
                ])
                .spacing(space::M),
                crate::ui_controls::button::secondary("Cancel")
                    .on_press(Message::Device(Action::Close)),
                600.0,
                dialog_ui::MAX_HEIGHT,
            );
        }
        let narrow = size.width < 900.0;
        let action = |action| Message::Device(action);
        let button = |label: &str, value: Action| {
            let mut button = crate::ui_controls::button::secondary(label.to_owned());
            if !state.busy {
                button = button.on_press(action(value));
            }
            Element::from(button)
        };
        if state.configure {
            let mut rows = vec![
                design::title("Connect to a device").into(),
                design::body("Use an SSH host alias already configured on this computer. Install matching cagents and cagentsd on the device.").into(),
                crate::ui_controls::field::text("Device name", &state.config_name)
                    .on_input(|value| Message::Device(Action::ConfigName(value)))
                    .into(),
                crate::ui_controls::field::text(
                    "SSH host alias (for example build-host)",
                    &state.config_host,
                )
                .on_input(|value| Message::Device(Action::ConfigHost(value)))
                .into(),
                crate::ui_controls::field::text(
                    "Remote cagents executable",
                    &state.config_executable,
                )
                .on_input(|value| Message::Device(Action::ConfigExecutable(value)))
                .into(),
            ];
            // What the last attempt said, such as an SSH error, sits above
            // the actions it is about; a long one scrolls with the fields.
            if let Some(notice) = &state.notice {
                rows.push(design::body(notice).into());
            }
            return dialog_ui::scrolling_surface_with_actions(
                widget::column::with_children(rows).spacing(space::M),
                widget::row::with_children(vec![
                    button("Close", Action::Close),
                    button("Save and switch device", Action::ConfigSave),
                ])
                .spacing(space::S),
                600.0,
                dialog_ui::MAX_HEIGHT,
            );
        }
        let icon = |name: Icon, label: &'static str, value: Action, enabled: bool| {
            let destructive = matches!(
                &value,
                Action::DiscardChanges | Action::Git(GitOperation::RemoveWorktree { .. })
            );
            let control = crate::ui_controls::icon_button(name, label)
                .on_press_maybe((!state.busy && enabled).then_some(action(value)));
            Element::from(if destructive {
                control.tone(design::Tone::Themed(design::destructive_color))
            } else {
                control
            })
        };
        let modes = crate::ui_controls::segmented_controls(
            [
                (Panel::Files, Icon::Folder, "Browse files"),
                (Panel::Changes, Icon::Diff, "Source control"),
                (Panel::Worktrees, Icon::Worktree, "Manage worktrees"),
            ]
            .into_iter()
            .map(|(panel, glyph, label)| {
                crate::ui_controls::icon_button(glyph, label)
                    .selected(state.panel == panel)
                    .on_press_maybe((!state.busy).then(|| action(Action::Panel(panel))))
                    .into()
            })
            .collect(),
        );
        let device = crate::device::selected()
            .map(|device| device.name.as_str())
            .unwrap_or("local");
        let mut rows: Vec<Element<'_, Message>> = vec![
            widget::row::with_children(vec![
                modes,
                design::heading(state.panel.label()).into(),
                design::caption(device).width(Length::Fill).into(),
                icon(Icon::Close, "Close device tools", Action::Close, true),
            ])
            .align_y(Alignment::Center)
            .spacing(space::M)
            .into(),
            widget::row::with_children(vec![
                icon(
                    Icon::ChevronUp,
                    "Parent directory",
                    Action::Visit(
                        state
                            .directory
                            .parent()
                            .unwrap_or(&state.directory)
                            .to_owned(),
                    ),
                    state.directory.parent().is_some(),
                ),
                crate::ui_controls::field::text("Directory", &state.path)
                    .on_input(|value| Message::Device(Action::Path(value)))
                    .on_submit(|_| Message::Device(Action::Browse))
                    .width(Length::Fill)
                    .into(),
                icon(Icon::Refresh, "Browse directory", Action::Browse, true),
                icon(
                    Icon::OpenFolder,
                    "Open as workspace",
                    Action::OpenProject,
                    true,
                ),
            ])
            .align_y(Alignment::Center)
            .spacing(space::XS)
            .into(),
        ];
        if state.path != state.directory.display().to_string()
            && !state.directory.as_os_str().is_empty()
        {
            rows.push(crate::interaction::tooltip(
                widget::row::with_children(vec![
                    design::caption("Current:").into(),
                    crate::ui_controls::FadeText::caption(state.directory.display().to_string())
                        .edge(crate::ui_controls::FadeEdge::Start)
                        .width(Length::Fill)
                        .into(),
                ])
                .spacing(space::XS)
                .width(Length::Fill),
                format!(
                    "File and Git operations use {} until navigation succeeds",
                    state.directory.display()
                ),
            ));
        }
        if let Some(notice) = &state.notice {
            rows.push(design::caption(notice).into());
        }
        if state.busy {
            rows.push(design::caption("Connecting…").into());
        }
        rows.push(design::horizontal_rule());
        let height = (size.height - 164.0).clamp(200.0, 620.0);
        // The output and the file list sit directly on the dialog, between
        // its controls: what scrolls past their edges fades into it.
        let output = |height| {
            crate::interaction::overflow_fade_y(
                widget::scrollable(
                    widget::container(
                        design::body(if state.panel == Panel::Worktrees {
                            &state.worktree_output
                        } else {
                            &state.output
                        })
                        .font(iced::Font::MONOSPACE)
                        .width(Length::Fill),
                    )
                    .padding(design::with_vertical_rail(Padding::ZERO)),
                )
                .direction(iced::widget::scrollable::Direction::Vertical(
                    design::scrollbar(),
                ))
                .height(height)
                .width(Length::Fill),
                design::dialog_base,
            )
        };
        let body: Element<'_, Message> = match state.panel {
            Panel::Files => {
                let files: Vec<Element<'_, Message>> = state
                    .entries
                    .iter()
                    .map(|entry| {
                        let value = if entry.directory {
                            Action::Visit(state.directory.join(&entry.name))
                        } else {
                            Action::Read(entry.name.clone().into())
                        };
                        let glyph = if entry.directory {
                            Icon::Folder
                        } else {
                            Icon::Edit
                        };
                        let selected = state
                            .file
                            .as_ref()
                            .is_some_and(|file| file == &PathBuf::from(&entry.name));
                        widget::button::custom(
                            widget::row::with_children(vec![
                                widget::icon(glyph.handle())
                                    .size(design::ROW_ICON_SIZE)
                                    .into(),
                                crate::ui_controls::fade_text(&entry.name)
                                    .width(Length::Fill)
                                    .into(),
                            ])
                            .spacing(space::S)
                            .align_y(Alignment::Center),
                        )
                        .name(entry.name.clone())
                        .selected(selected)
                        .class(crate::ui_controls::control_style(selected, None))
                        .padding([6.0, space::S])
                        .width(Length::Fill)
                        .on_press_maybe((!state.busy && !entry.symlink).then_some(action(value)))
                        .into()
                    })
                    .collect();
                let file_list = crate::interaction::overflow_fade_y(
                    widget::scrollable(
                        widget::column::with_children(files)
                            .spacing(space::XXS)
                            .padding(design::with_vertical_rail(Padding::ZERO)),
                    )
                    .direction(iced::widget::scrollable::Direction::Vertical(
                        design::scrollbar(),
                    ))
                    .width(if narrow { 176 } else { 224 })
                    .height(height),
                    design::dialog_base,
                );
                let editor: Element<'_, Message> = if let Some(file) = &state.file {
                    widget::column::with_children(vec![
                        widget::row::with_children(vec![
                            design::body(file.display().to_string())
                                .width(Length::Fill)
                                .into(),
                            if state.dirty() {
                                design::caption("Modified").into()
                            } else {
                                iced::widget::Space::new().into()
                            },
                            icon(Icon::Save, "Save on device", Action::Save, state.dirty()),
                            icon(
                                Icon::RevertFile,
                                "Discard changes",
                                Action::DiscardChanges,
                                state.dirty(),
                            ),
                        ])
                        .align_y(Alignment::Center)
                        .spacing(space::XS)
                        .into(),
                        iced::widget::text_editor(&state.content)
                            .font(crate::terminal::monospace_font(
                                self.settings.editor_font_family(),
                            ))
                            .size(self.settings.editor_font_size())
                            .id(file_editor_id())
                            .on_action(|value| Message::Device(Action::Edit(value)))
                            .height((height - 38.0).max(120.0))
                            .into(),
                    ])
                    .spacing(space::S)
                    .width(Length::Fill)
                    .into()
                } else {
                    widget::container(
                        widget::column::with_children(vec![
                            crate::ui_controls::passive_icon(Icon::Edit, 32),
                            design::caption("Select a file").into(),
                        ])
                        .spacing(space::M)
                        .align_x(Alignment::Center),
                    )
                    .center(Length::Fill)
                    .height(height)
                    .into()
                };
                widget::row::with_children(vec![file_list, editor])
                    .spacing(space::L)
                    .into()
            }
            Panel::Changes => widget::column::with_children(vec![
                widget::row::with_children(vec![
                    icon(
                        Icon::Refresh,
                        "Git status",
                        Action::Git(GitOperation::Status),
                        true,
                    ),
                    icon(
                        Icon::Diff,
                        "Working tree diff",
                        Action::Git(GitOperation::Diff { staged: false }),
                        true,
                    ),
                    icon(
                        Icon::Commit,
                        "Staged diff",
                        Action::Git(GitOperation::Diff { staged: true }),
                        true,
                    ),
                    crate::ui_controls::field::text("File path", &state.pathspec)
                        .on_input(|value| Message::Device(Action::Pathspec(value)))
                        .width(Length::Fill)
                        .into(),
                    icon(
                        Icon::Plus,
                        "Stage path",
                        Action::Git(GitOperation::Stage {
                            paths: vec![state.pathspec.clone().into()],
                        }),
                        !state.pathspec.trim().is_empty(),
                    ),
                    icon(
                        Icon::Minus,
                        "Unstage path",
                        Action::Git(GitOperation::Unstage {
                            paths: vec![state.pathspec.clone().into()],
                        }),
                        !state.pathspec.trim().is_empty(),
                    ),
                ])
                .align_y(Alignment::Center)
                .spacing(space::XS)
                .into(),
                widget::row::with_children(vec![
                    crate::ui_controls::field::text("Commit message", &state.commit_message)
                        .on_input(|value| Message::Device(Action::CommitMessage(value)))
                        .width(Length::Fill)
                        .into(),
                    icon(
                        Icon::Commit,
                        "Commit staged changes",
                        Action::Git(GitOperation::Commit {
                            message: state.commit_message.clone(),
                        }),
                        !state.commit_message.trim().is_empty(),
                    ),
                ])
                .align_y(Alignment::Center)
                .spacing(space::XS)
                .into(),
                output((height - 92.0).max(120.0)),
            ])
            .spacing(space::M)
            .into(),
            Panel::Worktrees => widget::column::with_children(vec![
                widget::row::with_children(vec![
                    icon(
                        Icon::Refresh,
                        "List worktrees",
                        Action::Git(GitOperation::Worktrees),
                        true,
                    ),
                    crate::ui_controls::field::text("Worktree name", &state.worktree_name)
                        .on_input(|value| Message::Device(Action::WorktreeName(value)))
                        .width(Length::Fill)
                        .into(),
                    crate::ui_controls::field::text("New branch", &state.worktree_branch)
                        .on_input(|value| Message::Device(Action::WorktreeBranch(value)))
                        .width(Length::Fill)
                        .into(),
                    icon(
                        Icon::WorktreeAdd,
                        "Create worktree under .worktrees",
                        Action::Git(GitOperation::CreateWorktree {
                            name: state.worktree_name.clone(),
                            branch: state.worktree_branch.clone(),
                        }),
                        !state.worktree_name.trim().is_empty()
                            && !state.worktree_branch.trim().is_empty(),
                    ),
                    icon(
                        Icon::Minus,
                        "Remove clean worktree",
                        Action::Git(GitOperation::RemoveWorktree {
                            name: state.worktree_name.clone(),
                        }),
                        !state.worktree_name.trim().is_empty(),
                    ),
                ])
                .align_y(Alignment::Center)
                .spacing(space::XS)
                .into(),
                output((height - 52.0).max(120.0)),
            ])
            .spacing(space::M)
            .into(),
        };
        rows.push(body);
        dialog_ui::scrolling_surface(
            widget::column::with_children(rows).spacing(space::M),
            1100.0,
            size.height.min(820.0),
            Length::Fill,
        )
    }
}

impl State {
    fn jump_to_fragment(&mut self, fragment: &str) {
        let Some(coordinates) = fragment.strip_prefix('L') else {
            self.notice = Some(format!(
                "Opened file; this viewer cannot jump to heading #{fragment}"
            ));
            return;
        };
        let (line, column) = coordinates
            .split_once(':')
            .or_else(|| coordinates.split_once('C'))
            .unwrap_or((coordinates, "1"));
        let (Ok(line), Ok(column)) = (line.parse::<usize>(), column.parse::<usize>()) else {
            return;
        };
        let line = line
            .saturating_sub(1)
            .min(self.content.line_count().saturating_sub(1));
        // The native iced editor's cursor column is a UTF-8 byte index.
        let column = self.content.line(line).map_or(0, |text| {
            text.text
                .char_indices()
                .nth(column.saturating_sub(1))
                .map_or(text.text.len(), |(byte, _)| byte)
        });
        self.content.move_to(text_editor::Cursor {
            position: text_editor::Position { line, column },
            selection: None,
        });
    }

    fn set_devices(&mut self, devices: Vec<crate::device::Device>) {
        self.devices = std::iter::once("local".into())
            .chain(devices.iter().map(|device| device.name.clone()))
            .collect();
        self.device_hosts = devices
            .into_iter()
            .map(|device| (device.name, device.host))
            .collect();
    }

    pub(super) fn dirty(&self) -> bool {
        self.file.is_some() && self.content.text() != self.original
    }
    pub(super) fn busy(&self) -> bool {
        self.busy
    }
    pub(super) fn can_close(&self) -> bool {
        !self.busy || self.image.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Provider;

    #[test]
    fn devices_menu_and_keyboard_chat_picker_transfer_keyboard_ownership() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.settings.show_status_bar = true;
        app.core.set_keyboard_nav(true);
        drop(app.new_native_chat(Provider::Codex, Some(PathBuf::from("/unused"))));
        drop(app.update(Message::RunCommand(
            workspace_widget::GlobalShortcut::ChatPermissions,
        )));
        assert!(app.chat_picker_owns_keyboard());
        assert!(!app.core.keyboard_nav());
        drop(app.update(Message::Device(Action::Menu(true))));
        assert!(app.device_ui.menu_open);
        assert!(!app.chat_picker_owns_keyboard());
        assert!(
            app.core.keyboard_nav(),
            "Devices retains ordinary Tab traversal"
        );
        drop(app.update(Message::RunCommand(
            workspace_widget::GlobalShortcut::ChatPermissions,
        )));
        assert!(!app.device_ui.menu_open);
        assert!(app.chat_picker_owns_keyboard());
        assert!(!app.core.keyboard_nav());
    }

    #[test]
    fn remote_row_navigation_and_metadata_keep_daemon_paths() {
        const CHILD: &str = "CAGENTS_TEST_REMOTE_NAVIGATION";
        const TEST: &str =
            "app::device_ui::tests::remote_row_navigation_and_metadata_keep_daemon_paths";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "1")
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        crate::device::save(crate::device::Device {
            name: "navigation-test".into(),
            host: "build-host".into(),
            executable: "cagents".into(),
        })
        .unwrap();
        crate::device::select(Some("navigation-test")).unwrap();
        let mut app = super::super::tests::context_inspector_test_app(false);
        drop(app.update(Message::GitUi(git_ui::Action::Changed)));
        drop(
            app.update(Message::ProjectUi(project_ui::Action::EditorsLoaded(Ok(
                Vec::new(),
            )))),
        );
        assert!(
            app.status.is_none(),
            "Background local completions must not warn in a device window"
        );
        let root = tempfile::tempdir().unwrap();
        let local_target = root.path().join("unrelated-local");
        std::fs::create_dir(&local_target).unwrap();
        let path = root.path().join("daemon-path");
        std::os::unix::fs::symlink(&local_target, &path).unwrap();
        let project_id = ProjectId::new();
        let repository = path.join(".git");
        *app.sidebar_model.projects = vec![SidebarProject::new(
            project_id,
            RepositoryId(project_id.as_uuid()),
            "Remote".into(),
            repository.clone(),
            Vec::new(),
            Vec::new(),
        )];
        app.repository_generations.insert(project_id, 2);
        let navigation = crate::device_service::Navigation {
            repository: repository.clone(),
            worktrees: vec![crate::device_service::NavigationWorktree {
                path: path.clone(),
                branch: Some("refs/heads/main".into()),
                ..Default::default()
            }],
        };
        drop(app.device_project_loaded(project_id, 1, Ok(navigation.clone())));
        assert!(
            app.sidebar_model
                .project(project_id)
                .unwrap()
                .worktrees
                .is_empty()
        );
        drop(app.device_project_loaded(project_id, 2, Ok(navigation)));
        let worktree = app.sidebar_model.project(project_id).unwrap().worktrees[0].id;
        drop(app.update(Message::Sidebar(SidebarAction::SelectRows {
            row: sidebar::SelectionRow::Worktree(project_id, worktree),
            intent: sidebar::SelectionIntent::Navigate,
        })));
        assert!(!app.device_ui.open);
        drop(app.update(Message::Sidebar(SidebarAction::OpenWorktree {
            project_id,
            worktree_id: worktree,
            path: path.clone(),
        })));
        assert!(!app.device_ui.open);
        assert!(app.sidebar_model.source_control.is_none());
        assert!(app.refresh_ui.source_cancel.is_none());
        assert_eq!(app.active_workspace, Some(path));
        assert_ne!(app.active_workspace.as_ref(), Some(&local_target));
        drop(app.update(Message::Sidebar(SidebarAction::ClearRowSelection)));
        assert!(!app.device_ui.open);
        // A closed project's late RPC cannot leave a global in-flight fence
        // that suppresses catalog refresh for all surviving projects.
        let closed = ProjectId::new();
        let other = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            other,
            RepositoryId(other.as_uuid()),
            "Other remote".into(),
            "/remote/other/.git".into(),
            Vec::new(),
            Vec::new(),
        ));
        app.refresh_ui
            .projects
            .insert(closed, CancellationToken::new());
        app.repository_generations.insert(closed, 2);
        drop(app.device_project_loaded(closed, 1, Err("late reply".into())));
        app.refresh_ui.focused = true;
        assert!(
            app.refresh_ui.projects.contains_key(&closed),
            "a late result must not erase a newer remote read's fence"
        );
        // Closing retires that generation. The scheduler ignores and prunes
        // its leftover marker before it chooses a surviving project.
        app.repository_generations.remove(&closed);
        app.refresh_ui.schedule_remote_started(project_id);
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Tick)));
        assert!(!app.refresh_ui.projects.contains_key(&closed));
        assert!(app.refresh_ui.projects.contains_key(&other));

        // Exercise the actual document-link entry point in the isolated remote process.
        let mut links = super::super::tests::context_inspector_test_app(false);
        let directory = PathBuf::from("/remote/chat-only-project");
        links.active_workspace = Some(directory.clone());
        drop(links.new_native_chat(Provider::Codex, Some(directory)));
        let chat = links.workspace.focused().unwrap();
        assert!(matches!(links.cards.get(&chat), Some(CardRuntime::Chat(_))));
        drop(links.open_document_link(Some(CardId::new()), "file:///remote/other.txt"));
        assert!(!links.device_ui.open);
        assert!(links.device_ui.pending.is_none());
        drop(links.open_document_link(Some(chat), "src/main.rs:2:3"));
        assert_eq!(
            links.device_ui.linked_file.as_deref(),
            Some(Path::new("main.rs"))
        );
        assert_eq!(links.device_ui.linked_fragment.as_deref(), Some("L2:3"));
        assert!(
            matches!(&links.device_ui.pending, Some(Operation::List { path })
            if path == Path::new("/remote/chat-only-project/src"))
        );

        let mut navigation = super::super::tests::context_inspector_test_app(false);
        drop(
            navigation.update(Message::Sidebar(SidebarAction::SearchChanged(
                "remote".into(),
            ))),
        );
        assert_eq!(navigation.sidebar_state.query(), "remote");
        assert!(!navigation.device_ui.open);
        drop(navigation.update(Message::Sidebar(SidebarAction::SetFilter(
            sidebar::SidebarFilter::Attention,
        ))));
        assert_eq!(
            navigation.sidebar_state.filter(),
            sidebar::SidebarFilter::Attention
        );
        assert!(!navigation.device_ui.open);
    }

    #[test]
    fn opening_a_project_replaces_the_previous_directory_listing_even_on_failure() {
        for succeeds in [true, false] {
            let mut app = loaded_file();
            drop(app.update_device(Action::DiscardChanges));
            app.device_ui.entries = vec![crate::device_service::Entry {
                name: "old-file".into(),
                directory: false,
                symlink: false,
            }];
            app.device_ui.output = "old Git output".into();
            drop(app.update_device(Action::Path("/remote/other".into())));
            drop(app.update_device(Action::OpenProject));
            drop(app.update_device(Action::Loaded {
                generation: app.device_ui.generation,
                result: Ok(Reply::Project {
                    path: "/remote/other".into(),
                    key: "/remote/other/.git".into(),
                    kind: ProjectKind::Git,
                }),
            }));
            assert_eq!(app.device_ui.directory, Path::new("/remote/other"));
            assert!(app.device_ui.file.is_none());
            assert!(app.device_ui.entries.is_empty());
            assert!(app.device_ui.output.is_empty());
            assert!(
                matches!(&app.device_ui.pending, Some(Operation::List { path })
                if path == Path::new("/remote/other"))
            );
            let result = if succeeds {
                Ok(Reply::Directory {
                    path: "/remote/other".into(),
                    entries: vec![crate::device_service::Entry {
                        name: "new-file".into(),
                        directory: false,
                        symlink: false,
                    }],
                })
            } else {
                Err("Cannot list the selected project".into())
            };
            drop(app.update_device(Action::Loaded {
                generation: app.device_ui.generation,
                result,
            }));
            if succeeds {
                assert_eq!(app.device_ui.entries[0].name, "new-file");
            } else {
                assert!(app.device_ui.entries.is_empty());
                assert!(
                    app.device_ui
                        .notice
                        .as_deref()
                        .unwrap()
                        .contains("Cannot list")
                );
            }
        }
    }

    #[test]
    fn git_uses_the_browsed_directory_while_the_address_is_a_draft() {
        let mut app = loaded_file();
        drop(app.update_device(Action::Path("/remote/other-checkout".into())));
        drop(app.update_device(Action::Git(GitOperation::Commit {
            message: "change".into(),
        })));
        assert!(
            matches!(&app.device_ui.pending, Some(Operation::Git { root, .. })
            if root == Path::new("/remote/project"))
        );
        assert_eq!(app.device_ui.directory, Path::new("/remote/project"));
        assert_eq!(app.device_ui.content.text(), "unsaved draft");
    }

    #[test]
    fn linked_file_browses_its_parent_then_reads_and_positions_the_file() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        drop(app.update_device(Action::OpenFile {
            path: "/remote/project/src/main.rs".into(),
            fragment: Some("L2:3".into()),
        }));
        assert!(app.device_ui.open);
        assert!(
            matches!(&app.device_ui.pending, Some(Operation::List { path })
            if path == Path::new("/remote/project/src"))
        );
        drop(app.update_device(Action::Loaded {
            generation: app.device_ui.generation,
            result: Ok(Reply::Directory {
                path: "/remote/project/src".into(),
                entries: vec![],
            }),
        }));
        assert!(
            matches!(&app.device_ui.pending, Some(Operation::Read { root, path })
            if root == Path::new("/remote/project/src") && path == Path::new("main.rs"))
        );
        assert_eq!(
            app.device_ui.linked_file.as_deref(),
            Some(Path::new("main.rs")),
            "The link keeps focus intent until its read succeeds"
        );
        drop(app.update_device(Action::Loaded {
            generation: app.device_ui.generation,
            result: Ok(Reply::Text("first\nsecond\nthird".into())),
        }));
        assert_eq!(app.device_ui.file.as_deref(), Some(Path::new("main.rs")));
        assert_eq!(
            app.device_ui.content.cursor().position,
            text_editor::Position { line: 1, column: 2 }
        );
        assert!(app.device_ui.linked_file.is_none());
        assert!(app.device_ui.linked_fragment.is_none());
        assert!(!app.device_ui.dirty());
    }

    #[test]
    fn linked_image_reads_the_exact_device_file_without_listing_its_parent() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let path = PathBuf::from("/remote/worktree/docs/界 #% literal-*.PNG");
        let task = app.update_device(Action::OpenFile {
            path: path.clone(),
            fragment: None,
        });
        assert!(app.device_ui.open && app.device_ui.busy && app.device_ui.can_close());
        assert!(
            matches!(&app.device_ui.pending, Some(Operation::ReadImage { root, path: sent })
            if root == Path::new("/") && *sent == path)
        );
        assert_eq!(app.device_ui.image.as_ref().unwrap().path, path);
        assert!(app.device_ui.linked_file.is_none());
        let handle = iced::widget::image::Handle::from_rgba(1, 1, vec![3, 240, 3, 255]);
        drop(app.update_device(Action::ImageLoaded {
            generation: app.device_ui.generation,
            result: Ok(handle),
        }));
        assert!(!app.device_ui.open && !app.device_ui.busy);
        assert!(app.device_ui.pending.is_none() && app.device_ui.image.is_none());
        assert!(app.chat_ui.image_dialog.is_some());
        assert!(
            app.device_ui.file.is_none(),
            "an image never becomes editable text"
        );
        drop(task);
    }

    #[test]
    fn cancelled_device_image_cannot_reopen_or_replace_a_newer_request() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let old = app.update_device(Action::OpenFile {
            path: "/remote/old.png".into(),
            fragment: None,
        });
        let old_generation = app.device_ui.generation;
        let aborted = app.device_ui.image.as_ref().unwrap().task.clone();
        drop(app.update_device(Action::Close));
        assert!(
            aborted.is_aborted(),
            "cancellation also removes queued work from the image workers"
        );
        assert!(!app.device_ui.open && !app.device_ui.busy);
        assert!(app.device_ui.pending.is_none() && app.device_ui.image.is_none());
        let handle = || iced::widget::image::Handle::from_rgba(1, 1, vec![3, 240, 3, 255]);
        drop(app.update_device(Action::ImageLoaded {
            generation: old_generation,
            result: Ok(handle()),
        }));
        assert!(app.chat_ui.image_dialog.is_none());
        let new = app.update_device(Action::OpenFile {
            path: "/remote/new.png".into(),
            fragment: None,
        });
        let new_generation = app.device_ui.generation;
        drop(app.update_device(Action::ImageLoaded {
            generation: old_generation,
            result: Err("old failure".into()),
        }));
        assert!(app.device_ui.busy && app.device_ui.notice.is_none());
        assert_eq!(
            app.device_ui.image.as_ref().unwrap().path,
            Path::new("/remote/new.png")
        );
        drop(app.update_device(Action::ImageLoaded {
            generation: new_generation,
            result: Err("Cannot preview /remote/new.png on FW16: missing".into()),
        }));
        assert!(!app.device_ui.busy && app.device_ui.open && app.device_ui.can_close());
        assert!(
            app.device_ui
                .notice
                .as_deref()
                .unwrap()
                .contains("/remote/new.png on FW16")
        );
        assert!(app.chat_ui.image_dialog.is_none());
        drop((old, new));
    }

    #[test]
    fn browsing_an_image_uses_its_device_directory_and_the_same_read_only_gallery() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.device_ui.directory = "/remote/selected-worktree/docs".into();
        let task = app.update_device(Action::Read("image #%.png".into()));
        assert!(
            matches!(&app.device_ui.pending, Some(Operation::ReadImage { path, .. })
            if path == Path::new("/remote/selected-worktree/docs/image #%.png"))
        );
        assert!(app.device_ui.file.is_none());
        drop(app.update_device(Action::Close));
        drop(task);
    }

    #[test]
    fn linked_image_preserves_unsaved_device_text_and_busy_operation_guards() {
        let mut app = loaded_file();
        let generation = app.device_ui.generation;
        drop(app.update_device(Action::OpenFile {
            path: "/remote/image.png".into(),
            fragment: None,
        }));
        assert!(app.device_ui.dirty() && app.device_ui.image.is_none());
        assert_eq!(app.device_ui.content.text(), "unsaved draft");
        assert_eq!(app.device_ui.generation, generation);
        assert!(
            app.device_ui
                .notice
                .as_deref()
                .unwrap()
                .contains("Unsaved changes")
        );

        drop(app.update_device(Action::DiscardChanges));
        drop(app.update_device(Action::Browse));
        let generation = app.device_ui.generation;
        drop(app.update_device(Action::OpenFile {
            path: "/remote/image.png".into(),
            fragment: None,
        }));
        assert_eq!(app.device_ui.generation, generation);
        assert!(app.device_ui.image.is_none());
        assert!(matches!(
            app.device_ui.pending,
            Some(Operation::List { .. })
        ));
        drop(app.update_device(Action::Close));
        assert!(
            app.device_ui.busy,
            "normal mutations cannot be cancelled using the image path"
        );
    }

    #[test]
    fn remote_sidebar_actions_run_use_their_device_equivalent_or_say_why() {
        let project_id = ProjectId::new();
        let worktree_id = crate::model::WorktreeId::new();
        let on = |worktree_id, command| SidebarAction::ProjectCommand {
            project_id,
            worktree_id,
            command,
        };
        let command = |command| on(Some(worktree_id), command);
        let channel = crate::model::ChannelId::new();
        let url = Url::parse("https://github.com/example/demo/pull/7").unwrap();
        for action in [
            SidebarAction::SearchChanged("project".into()),
            SidebarAction::SetFilter(sidebar::SidebarFilter::Attention),
            SidebarAction::HideReadAttention,
            SidebarAction::Resize { width: 300.0 },
            SidebarAction::ReorderMember {
                project: project_id,
                source: sidebar::SelectionRow::Channel(channel),
                before: None,
            },
            SidebarAction::ReorderProject {
                project: project_id,
                before: None,
            },
            SidebarAction::OpenRepositoryPicker,
            SidebarAction::OpenAutomations,
            SidebarAction::OpenWork,
            SidebarAction::NewChannel { project_id },
            SidebarAction::RenameChannel(channel),
            SidebarAction::MarkChannelRead(channel),
            SidebarAction::DeleteChannel {
                channel_id: channel,
            },
            SidebarAction::DeleteRow(sidebar::SelectionRow::Channel(channel)),
            command(sidebar::ProjectCommand::NewChannel),
            command(sidebar::ProjectCommand::NewTerminal),
            command(sidebar::ProjectCommand::CopyPath),
            command(sidebar::ProjectCommand::CopyBranch),
            command(sidebar::ProjectCommand::ResetSidebarOrder),
        ] {
            assert_eq!(sidebar_route(&action), None, "{action:?}");
        }
        for worktree_id in [Some(worktree_id), None] {
            assert_eq!(
                sidebar_route(&on(worktree_id, sidebar::ProjectCommand::Reveal)),
                Some(SidebarRoute::BrowseOnDevice {
                    project_id,
                    worktree_id,
                })
            );
        }
        for (action, remove) in [
            (command(sidebar::ProjectCommand::NewWorktree), None),
            (
                SidebarAction::NewWorktree {
                    project_id,
                    repository: "/srv/demo/.git".into(),
                },
                None,
            ),
            (command(sidebar::ProjectCommand::Delete), Some(worktree_id)),
            (
                SidebarAction::DeleteRow(sidebar::SelectionRow::Worktree(project_id, worktree_id)),
                Some(worktree_id),
            ),
        ] {
            assert_eq!(
                sidebar_route(&action),
                Some(SidebarRoute::DeviceWorktrees { project_id, remove }),
                "{action:?}"
            );
        }
        assert_eq!(
            sidebar_route(&SidebarAction::DeleteSelectedRows),
            Some(SidebarRoute::DeleteSelectedRows)
        );
        assert_eq!(
            sidebar_route(&SidebarAction::OpenPullRequest {
                project_id,
                number: 7,
                url: url.clone(),
            }),
            Some(SidebarRoute::OpenLink(url))
        );
        assert_eq!(
            sidebar_route(&command(sidebar::ProjectCommand::PullRequest)),
            Some(SidebarRoute::PullRequest {
                project_id,
                worktree_id: Some(worktree_id),
            })
        );
        let key = sidebar::ActiveWorktreeKey {
            project_id,
            worktree_id,
            canonical_path: "/srv/demo".into(),
        };
        assert_eq!(
            sidebar_route(&SidebarAction::RefreshWorktree { key: key.clone() }),
            Some(SidebarRoute::DeviceTool(Panel::Changes))
        );
        assert_eq!(
            sidebar_route(&SidebarAction::ToggleFilesFilter { key }),
            Some(SidebarRoute::DeviceTool(Panel::Files))
        );
        for action in [
            SidebarAction::DismissWorktreeOperation(crate::model::OperationId::new()),
            SidebarAction::CancelDiscard,
            SidebarAction::CancelPublish,
        ] {
            assert_eq!(sidebar_route(&action), None, "{action:?}");
        }
        for action in [
            command(sidebar::ProjectCommand::Remove),
            command(sidebar::ProjectCommand::RepositoryUrl),
            command(sidebar::ProjectCommand::NewWorktreeFromBranch),
            SidebarAction::RetryWorktreeOperation(crate::model::OperationId::new()),
            command(sidebar::ProjectCommand::OpenEditor("code".into())),
            SidebarAction::DeleteRow(sidebar::SelectionRow::Project(project_id)),
            SidebarAction::BrowsePullRequests { project_id },
        ] {
            assert!(
                matches!(sidebar_route(&action), Some(SidebarRoute::Unavailable(reason)) if !reason.is_empty()),
                "{action:?}"
            );
        }
    }

    #[test]
    fn remote_sidebar_commands_create_channels_and_open_the_matching_device_tool() {
        const CHILD: &str = "CAGENTS_TEST_REMOTE_SIDEBAR_COMMANDS";
        const TEST: &str = "app::device_ui::tests::remote_sidebar_commands_create_channels_and_open_the_matching_device_tool";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "1")
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        crate::device::save(crate::device::Device {
            name: "sidebar-test".into(),
            host: "build-host".into(),
            executable: "cagents".into(),
        })
        .unwrap();
        crate::device::select(Some("sidebar-test")).unwrap();
        let mut app = super::super::tests::context_inspector_test_app(false);
        // Paths that exist only on the device.
        let main = PathBuf::from("/srv/device-only/demo");
        let linked = main.join(".worktrees/feature");
        let project_id = ProjectId::new();
        *app.sidebar_model.projects = vec![SidebarProject::new(
            project_id,
            RepositoryId(project_id.as_uuid()),
            "demo".into(),
            main.join(".git"),
            Vec::new(),
            Vec::new(),
        )];
        app.repository_generations.insert(project_id, 1);
        drop(app.device_project_loaded(
            project_id,
            1,
            Ok(crate::device_service::Navigation {
                repository: main.join(".git"),
                worktrees: vec![
                    crate::device_service::NavigationWorktree {
                        path: main.clone(),
                        branch: Some("refs/heads/main".into()),
                        ..Default::default()
                    },
                    crate::device_service::NavigationWorktree {
                        path: linked.clone(),
                        branch: Some("refs/heads/feature".into()),
                        ..Default::default()
                    },
                ],
            }),
        ));
        let worktrees = app
            .sidebar_model
            .project(project_id)
            .unwrap()
            .worktrees
            .clone();
        let linked_id = worktrees
            .iter()
            .find(|worktree| worktree.path == linked)
            .unwrap()
            .id;

        // The project menu's New channel asks for `NewChannel`, which the
        // test delivers itself, since it doesn't run the returned task.
        let channels = app.durable.channels.len();
        drop(app.update(Message::Sidebar(SidebarAction::ProjectCommand {
            project_id,
            worktree_id: None,
            command: sidebar::ProjectCommand::NewChannel,
        })));
        assert!(
            !app.device_ui.open,
            "New channel must not open the device tools"
        );
        assert!(app.status.is_none());
        drop(app.update(Message::Sidebar(SidebarAction::NewChannel { project_id })));
        assert!(
            !app.device_ui.open,
            "New channel must not open the device tools"
        );
        assert_eq!(app.durable.channels.len(), channels + 1);
        let channel = app.durable.channels.last().unwrap();
        assert_eq!(channel.project_id, project_id);
        assert_eq!(app.durable.active_channel, Some(channel.id));

        // New terminal starts a terminal pane in the checkout.
        drop(app.update(Message::Sidebar(SidebarAction::ProjectCommand {
            project_id,
            worktree_id: Some(linked_id),
            command: sidebar::ProjectCommand::NewTerminal,
        })));
        assert!(!app.device_ui.open);
        assert_eq!(app.active_workspace.as_deref(), Some(linked.as_path()));
        let focused = app.durable.focused.expect("new terminal is focused");
        let card = app
            .durable
            .cards
            .iter()
            .find(|card| card.id == focused)
            .unwrap();
        assert!(
            matches!(&card.kind, CardKind::Terminal(session) if session.launch.working_directory == linked),
            "{:?}",
            card.kind
        );

        // Rename and delete the created channel through their own controls.
        let created = app.durable.channels.last().unwrap().id;
        drop(app.update(Message::Sidebar(SidebarAction::RenameChannel(created))));
        drop(
            app.update(Message::ChromeUi(chrome_ui::Action::RenameChanged(
                "Remote work".into(),
            ))),
        );
        drop(app.update(Message::ChromeUi(chrome_ui::Action::RenameSubmit)));
        assert!(!app.device_ui.open);
        assert_eq!(
            app.durable
                .channels
                .iter()
                .find(|channel| channel.id == created)
                .unwrap()
                .title,
            "Remote work"
        );
        drop(app.update(Message::Sidebar(SidebarAction::DeleteRow(
            sidebar::SelectionRow::Channel(created),
        ))));
        drop(app.update(Message::ChannelUi(channel_ui::Action::ConfirmDelete)));
        assert!(!app.device_ui.open);
        assert!(
            app.channel_ui.is_deleting(created)
                || !app
                    .durable
                    .channels
                    .iter()
                    .any(|channel| channel.id == created)
        );

        // Show in file manager browses the project's main checkout, not its
        // Git directory, and a worktree row's own checkout.
        drop(app.update(Message::Sidebar(SidebarAction::ProjectCommand {
            project_id,
            worktree_id: None,
            command: sidebar::ProjectCommand::Reveal,
        })));
        assert!(app.device_ui.open);
        assert_eq!(app.device_ui.path, main.display().to_string());
        let settle = |app: &mut Cagents| {
            let generation = app.device_ui.generation;
            drop(app.update_device(Action::Loaded {
                generation,
                result: Err("offline".into()),
            }));
            drop(app.update_device(Action::Close));
            assert!(!app.device_ui.open);
        };
        settle(&mut app);

        drop(app.update(Message::Sidebar(SidebarAction::ProjectCommand {
            project_id,
            worktree_id: Some(linked_id),
            command: sidebar::ProjectCommand::Reveal,
        })));
        assert!(app.device_ui.open);
        assert_eq!(app.device_ui.panel, Panel::Files);
        assert_eq!(app.device_ui.path, linked.display().to_string());
        settle(&mut app);

        // New worktree uses the device's worktree tools at the main checkout.
        drop(app.update(Message::Sidebar(SidebarAction::NewWorktree {
            project_id,
            repository: main.join(".git"),
        })));
        assert!(app.device_ui.open);
        assert_eq!(app.device_ui.panel, Panel::Worktrees);
        assert_eq!(app.device_ui.path, main.display().to_string());
        settle(&mut app);

        // Deleting a linked worktree names it for the device's remove action.
        drop(app.update(Message::Sidebar(SidebarAction::DeleteRow(
            sidebar::SelectionRow::Worktree(project_id, linked_id),
        ))));
        assert!(app.device_ui.open);
        assert_eq!(app.device_ui.panel, Panel::Worktrees);
        assert_eq!(app.device_ui.worktree_name, "feature");
        // While the listing is pending the dialog refuses another request,
        // and a Delete aimed at it leaves the name it shows alone.
        app.device_ui.worktree_name = "typed".into();
        drop(app.update(Message::Sidebar(SidebarAction::DeleteRow(
            sidebar::SelectionRow::Worktree(project_id, linked_id),
        ))));
        assert_eq!(app.device_ui.worktree_name, "typed");
        settle(&mut app);
        // New worktree starts with an empty name, not the last Delete's.
        drop(app.update(Message::Sidebar(SidebarAction::ProjectCommand {
            project_id,
            worktree_id: Some(linked_id),
            command: sidebar::ProjectCommand::Delete,
        })));
        assert_eq!(app.device_ui.worktree_name, "feature");
        settle(&mut app);
        drop(app.update(Message::Sidebar(SidebarAction::ProjectCommand {
            project_id,
            worktree_id: None,
            command: sidebar::ProjectCommand::NewWorktree,
        })));
        assert!(app.device_ui.open);
        assert!(app.device_ui.worktree_name.is_empty());
        settle(&mut app);

        // The main checkout is never offered for deletion.
        let main_id = app.sidebar_model.project(project_id).unwrap().worktrees[0].id;
        drop(app.update(Message::Sidebar(SidebarAction::DeleteRow(
            sidebar::SelectionRow::Worktree(project_id, main_id),
        ))));
        assert!(!app.device_ui.open);
        assert!(
            app.status
                .as_ref()
                .is_some_and(|notice| notice.message.contains("main checkout"))
        );

        // A selection of two worktrees says to delete one at a time.
        app.status = None;
        drop(
            app.update(Message::Sidebar(SidebarAction::SetWorktreeDisplay {
                project_id,
                mode: Some(crate::settings::SidebarWorktrees::Expanded),
            })),
        );
        drop(app.update(Message::Sidebar(SidebarAction::ClearRowSelection)));
        for (worktree, intent) in [
            (main_id, sidebar::SelectionIntent::Toggle),
            (linked_id, sidebar::SelectionIntent::Toggle),
        ] {
            drop(app.update(Message::Sidebar(SidebarAction::SelectRows {
                row: sidebar::SelectionRow::Worktree(project_id, worktree),
                intent,
            })));
        }
        assert_eq!(
            app.sidebar_state
                .selected_rows(&app.visible_sidebar_rows())
                .len(),
            2
        );
        drop(app.update(Message::Sidebar(SidebarAction::DeleteSelectedRows)));
        assert!(!app.device_ui.open);
        assert!(
            app.status
                .as_ref()
                .is_some_and(|notice| notice.message.contains("one worktree at a time")),
            "{:?}",
            app.status
        );
        // One selected worktree goes to its device action.
        drop(app.update(Message::Sidebar(SidebarAction::SelectRows {
            row: sidebar::SelectionRow::Worktree(project_id, main_id),
            intent: sidebar::SelectionIntent::Toggle,
        })));
        drop(app.update(Message::Sidebar(SidebarAction::DeleteSelectedRows)));
        assert_eq!(app.device_ui.panel, Panel::Worktrees);
        assert_eq!(app.device_ui.worktree_name, "feature");
        settle(&mut app);
        drop(app.update(Message::Sidebar(SidebarAction::ClearRowSelection)));

        // A routed command waits behind an open dialog, as it does locally.
        drop(app.update_device(Action::OpenIn {
            path: Some(main.clone()),
            panel: Panel::Files,
        }));
        drop(app.update(Message::Sidebar(SidebarAction::ProjectCommand {
            project_id,
            worktree_id: None,
            command: sidebar::ProjectCommand::NewWorktree,
        })));
        assert_eq!(app.device_ui.panel, Panel::Files);
        settle(&mut app);

        // Before its checkouts load, a project's worktree tools still open in
        // its main checkout, never its Git directory.
        let unloaded = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            unloaded,
            RepositoryId(unloaded.as_uuid()),
            "other".into(),
            PathBuf::from("/srv/device-only/other/.git"),
            Vec::new(),
            Vec::new(),
        ));
        drop(app.update(Message::Sidebar(SidebarAction::NewWorktree {
            project_id: unloaded,
            repository: "/srv/device-only/other/.git".into(),
        })));
        assert_eq!(app.device_ui.path, "/srv/device-only/other");
        settle(&mut app);

        // What a device can't do yet says so instead of opening the tools.
        drop(app.update(Message::Sidebar(SidebarAction::ProjectCommand {
            project_id,
            worktree_id: None,
            command: sidebar::ProjectCommand::Remove,
        })));
        assert!(!app.device_ui.open);
        assert!(
            app.status
                .as_ref()
                .is_some_and(|notice| notice.message == REMOVE_PROJECT_UNAVAILABLE)
        );
    }

    #[test]
    fn linked_coordinates_preserve_unicode_boundaries_and_clamp_to_the_file() {
        let mut state = State {
            content: text_editor::Content::with_text("first\néx"),
            ..State::default()
        };
        state.jump_to_fragment("L2:2");
        assert_eq!(
            state.content.cursor().position,
            text_editor::Position { line: 1, column: 2 }
        );
        state.jump_to_fragment("L99:99");
        assert_eq!(
            state.content.cursor().position,
            text_editor::Position { line: 1, column: 3 }
        );
    }

    fn loaded_file() -> Cagents {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.device_ui.open = true;
        app.device_ui.directory = "/remote/project".into();
        app.device_ui.path = "/remote/project".into();
        drop(app.update_device(Action::Read("first".into())));
        drop(app.update_device(Action::Loaded {
            generation: app.device_ui.generation,
            result: Ok(Reply::Text("original".into())),
        }));
        assert!(!app.device_ui.dirty());
        app.device_ui.content = text_editor::Content::with_text("unsaved draft");
        app
    }

    #[test]
    fn git_output_and_back_preserve_the_loaded_file_and_unsaved_draft() {
        let mut app = loaded_file();
        drop(app.update_device(Action::Git(GitOperation::Status)));
        assert_eq!(app.device_ui.panel, Panel::Changes);
        drop(app.update_device(Action::Loaded {
            generation: app.device_ui.generation,
            result: Ok(Reply::Text("git status output".into())),
        }));
        assert_eq!(app.device_ui.output, "git status output");
        drop(app.update_device(Action::BackToFile));
        assert_eq!(app.device_ui.panel, Panel::Files);
        assert_eq!(app.device_ui.file, Some("first".into()));
        assert_eq!(app.device_ui.directory, PathBuf::from("/remote/project"));
        assert_eq!(app.device_ui.original, "original");
        assert_eq!(app.device_ui.content.text(), "unsaved draft");
        drop(app.update_device(Action::Save));
        assert!(
            matches!(&app.device_ui.pending, Some(Operation::Write { path, expected, content, .. })
            if path == &PathBuf::from("first") && expected == "original" && content == "unsaved draft")
        );
    }

    #[test]
    fn derived_stage_path_follows_selection_but_keeps_an_explicit_path_within_its_root() {
        let mut app = loaded_file();
        drop(app.update_device(Action::DiscardChanges));
        drop(app.update_device(Action::Read("second".into())));
        drop(app.update_device(Action::Loaded {
            generation: app.device_ui.generation,
            result: Ok(Reply::Text("second content".into())),
        }));
        assert_eq!(app.device_ui.pathspec, "second");
        drop(app.update_device(Action::Pathspec("explicit".into())));
        drop(app.update_device(Action::Read("third".into())));
        drop(app.update_device(Action::Loaded {
            generation: app.device_ui.generation,
            result: Ok(Reply::Text("third content".into())),
        }));
        assert_eq!(app.device_ui.pathspec, "explicit");
        app.device_ui.path = "/another/project".into();
        drop(app.update_device(Action::Browse));
        drop(app.update_device(Action::Loaded {
            generation: app.device_ui.generation,
            result: Ok(Reply::Directory {
                path: "/another/project".into(),
                entries: Vec::new(),
            }),
        }));
        assert!(app.device_ui.pathspec.is_empty());
        assert!(app.device_ui.pathspec_auto);
    }

    #[test]
    fn opening_source_control_browses_the_requested_root_before_loading_status() {
        for (panel, expected) in [
            (Panel::Changes, GitOperation::Status),
            (Panel::Worktrees, GitOperation::Worktrees),
        ] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            drop(app.update_device(Action::OpenIn {
                path: Some("/remote/project".into()),
                panel,
            }));
            assert!(
                matches!(&app.device_ui.pending, Some(Operation::List { path }) if path == &PathBuf::from("/remote/project"))
            );
            drop(app.update_device(Action::Loaded {
                generation: app.device_ui.generation,
                result: Ok(Reply::Directory {
                    path: "/remote/project".into(),
                    entries: Vec::new(),
                }),
            }));
            assert_eq!(app.device_ui.panel, panel);
            assert!(
                matches!(&app.device_ui.pending, Some(Operation::Git { root, operation }) if root == &PathBuf::from("/remote/project") && operation == &expected)
            );
        }
    }

    #[test]
    fn tool_modes_request_their_context_without_losing_an_unsaved_editor() {
        let mut app = loaded_file();
        app.device_ui.output = "Old output".into();
        drop(app.update_device(Action::Panel(Panel::Changes)));
        assert_eq!(app.device_ui.panel, Panel::Changes);
        assert_eq!(app.device_ui.pathspec, "first");
        assert_eq!(app.device_ui.output, "Old output");
        assert!(matches!(
            app.device_ui.pending,
            Some(Operation::Git {
                operation: GitOperation::Status,
                ..
            })
        ));
        drop(app.update_device(Action::Loaded {
            generation: app.device_ui.generation,
            result: Ok(Reply::Text("Git status".into())),
        }));
        drop(app.update_device(Action::Panel(Panel::Worktrees)));
        assert_eq!(app.device_ui.panel, Panel::Worktrees);
        assert_eq!(app.device_ui.output, "Git status");
        assert!(app.device_ui.worktree_output.is_empty());
        assert!(matches!(
            app.device_ui.pending,
            Some(Operation::Git {
                operation: GitOperation::Worktrees,
                ..
            })
        ));
        drop(app.update_device(Action::Loaded {
            generation: app.device_ui.generation,
            result: Ok(Reply::Text("Worktrees".into())),
        }));
        drop(app.update_device(Action::Panel(Panel::Files)));
        assert_eq!(app.device_ui.panel, Panel::Files);
        assert_eq!(app.device_ui.content.text(), "unsaved draft");
        assert_eq!(app.device_ui.output, "Git status");
        assert_eq!(app.device_ui.worktree_output, "Worktrees");
        assert!(app.device_ui.pending.is_none());
    }

    #[test]
    fn window_close_preserves_device_edits_until_explicit_resolution() {
        let mut app = loaded_file();
        let window = cosmic::iced::window::Id::unique();
        app.core.set_main_window_id(Some(window));
        drop(app.update(Message::WindowCloseRequested(window)));
        assert!(app.exit.is_none());
        assert_eq!(app.transient_work_close, Some(window));
        assert_eq!(app.device_ui.content.text(), "unsaved draft");
        drop(app.update(Message::ReturnToDeviceBeforeClose));
        assert!(app.transient_work_close.is_none());
        assert!(app.device_ui.open);
        drop(app.update_device(Action::Save));
        drop(app.update(Message::WindowCloseRequested(window)));
        assert!(
            app.exit.is_none(),
            "an in-flight save must keep the UI alive"
        );
        drop(app.update_device(Action::Loaded {
            generation: app.device_ui.generation,
            result: Err("write failed".into()),
        }));
        drop(app.update(Message::CancelTransientWorkClose));
        assert!(
            app.device_ui.dirty(),
            "failed save must not clear the dirty buffer"
        );
        drop(app.update(Message::WindowCloseRequested(window)));
        assert!(app.exit.is_none());
        drop(app.update(Message::CancelTransientWorkClose));
        drop(app.update_device(Action::DiscardChanges));
        drop(app.update(Message::WindowCloseRequested(window)));
        assert!(app.exit.is_some());
    }

    #[test]
    fn unsaved_file_blocks_navigation_and_close_until_explicit_save_or_discard() {
        for action in [
            Action::Close,
            Action::Open,
            Action::OpenFile {
                path: "/other/file.rs".into(),
                fragment: Some("L2".into()),
            },
            Action::OpenIn {
                path: Some("/other".into()),
                panel: Panel::Changes,
            },
            Action::Configure,
            Action::Browse,
            Action::Visit("/elsewhere".into()),
            Action::Read("second".into()),
            Action::OpenProject,
        ] {
            let mut app = loaded_file();
            let generation = app.device_ui.generation;
            drop(app.update_device(action));
            assert_eq!(app.device_ui.generation, generation);
            assert!(!app.device_ui.busy);
            assert!(app.device_ui.open);
            assert_eq!(app.device_ui.file, Some("first".into()));
            assert_eq!(app.device_ui.content.text(), "unsaved draft");
            assert!(
                app.device_ui
                    .notice
                    .as_deref()
                    .unwrap()
                    .contains("Unsaved changes")
            );
        }
        let mut app = loaded_file();
        // Escape follows the application's shared modal cancellation path.
        drop(app.cancel_form_dialog());
        assert!(app.device_ui.open);
        assert_eq!(app.device_ui.content.text(), "unsaved draft");
        drop(app.update_device(Action::DiscardChanges));
        assert_eq!(app.device_ui.content.text(), "original");
        assert!(!app.device_ui.dirty());
        drop(app.update_device(Action::Close));
        assert!(!app.device_ui.open);
        let mut app = loaded_file();
        drop(app.update_device(Action::Save));
        drop(app.update_device(Action::Loaded {
            generation: app.device_ui.generation,
            result: Ok(Reply::Saved),
        }));
        assert!(!app.device_ui.dirty());
        drop(app.update_device(Action::Read("second".into())));
        assert!(
            matches!(&app.device_ui.pending, Some(Operation::Read { path, .. }) if path == &PathBuf::from("second"))
        );
    }

    #[test]
    fn unconfirmed_save_advances_baseline_and_shows_a_durability_warning() {
        let mut app = loaded_file();
        drop(app.update_device(Action::Save));
        drop(app.update_device(Action::Loaded {
            generation: app.device_ui.generation,
            result: Ok(Reply::SavedUnconfirmed {
                error: "directory sync failed".into(),
            }),
        }));
        assert!(!app.device_ui.busy);
        assert!(
            !app.device_ui.dirty(),
            "published bytes are the saved baseline"
        );
        assert_eq!(app.device_ui.original, "unsaved draft");
        assert!(
            app.device_ui.notice.as_deref().unwrap().contains(
                "Saved on device; durability could not be confirmed: directory sync failed"
            )
        );
        app.device_ui.content = text_editor::Content::with_text("next edit");
        drop(app.update_device(Action::Save));
        assert!(matches!(&app.device_ui.pending,
            Some(Operation::Write { expected, content, .. })
                if expected == "unsaved draft" && content == "next edit"
        ));
    }

    #[test]
    fn queued_reads_cannot_rebind_a_pending_file_or_accept_a_stale_reply() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.device_ui.directory = "/remote/project".into();
        drop(app.update_device(Action::Read("first".into())));
        let generation = app.device_ui.generation;
        drop(app.update_device(Action::Read("second".into())));
        assert!(app.device_ui.file.is_none());
        drop(app.update_device(Action::Loaded {
            generation: generation.wrapping_add(1),
            result: Ok(Reply::Text("wrong".into())),
        }));
        assert!(app.device_ui.busy);
        drop(app.update_device(Action::Loaded {
            generation,
            result: Ok(Reply::Text("first content".into())),
        }));
        assert_eq!(app.device_ui.file, Some("first".into()));
        assert_eq!(app.device_ui.original, "first content");
        drop(app.update_device(Action::Read("second".into())));
        let generation = app.device_ui.generation;
        drop(app.update_device(Action::Loaded {
            generation,
            result: Err("unreadable".into()),
        }));
        assert_eq!(app.device_ui.file, Some("first".into()));
        assert_eq!(app.device_ui.original, "first content");
    }
}
