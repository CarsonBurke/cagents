//! Native tools for remote files and source control. They never resolve a
//! selected device's path through the local filesystem.
use super::*;
use crate::device_service::{GitOperation, Operation, Reply};
use cosmic::widget::text_editor;
use std::collections::BTreeMap;

pub(super) fn file_editor_id() -> widget::Id {
    widget::Id::new("device-file-editor")
}

/// Navigation presentation and device-owned row opening never need desktop Git or files.
pub(super) fn supports_sidebar_action(action: &SidebarAction) -> bool {
    matches!(
        action,
        SidebarAction::ReorderMember { .. }
            | SidebarAction::ReorderProject { .. }
            | SidebarAction::ProjectCommand {
                command: crate::sidebar::ProjectCommand::ResetSidebarOrder
                    | crate::sidebar::ProjectCommand::MoveUp
                    | crate::sidebar::ProjectCommand::MoveDown,
                ..
            }
            | SidebarAction::SearchChanged(_)
            | SidebarAction::SetFilter(_)
            | SidebarAction::HideReadAttention
            | SidebarAction::ToggleControls
            | SidebarAction::OpenFirstSearchResult
            | SidebarAction::FocusFirstSearchResult
            | SidebarAction::Resize { .. }
            | SidebarAction::ResizeFinished
            | SidebarAction::ScrollProjects(_)
            | SidebarAction::NavigateRow { .. }
            | SidebarAction::OpenChannel { .. }
            | SidebarAction::SelectRows { .. }
            | SidebarAction::ClearRowSelection
            | SidebarAction::OpenRepository { .. }
            | SidebarAction::OpenWorktree { .. }
            | SidebarAction::Refresh
            | SidebarAction::RefreshProject { .. }
            | SidebarAction::ToggleProjects
            | SidebarAction::ToggleProject(_)
            | SidebarAction::DragWindow
            | SidebarAction::MaximizeWindow
            | SidebarAction::WindowMenu
            | SidebarAction::OpenSettings
    )
}

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
    reopen_origin: Option<super::workspace_model_sync::DeviceOrigin>,
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
            reopen_origin: None,
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
    pub(super) fn observe_device_reopen_close(&mut self, sequence: u64, repository: &Path) {
        if let Some(origin) = &mut self.device_ui.reopen_origin {
            origin.observe_close(
                sequence,
                repository,
                self.workspace_model.identity,
                self.workspace_model_source.as_ref(),
            );
        }
    }

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

    pub(super) fn update_device(&mut self, action: Action) -> Task<Message> {
        if let Action::Menu(open) = action {
            self.device_ui.menu_open =
                open && self.settings.show_status_bar && !self.modal_is_open();
            if self.device_ui.menu_open {
                self.dismiss_chat_picker();
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
                        let saved = crate::device::list().map_err(|error| error.to_string())?;
                        if saved.iter().any(|existing| {
                            existing.name == device.name
                                && (existing.host != device.host
                                    || existing.executable != device.executable)
                        }) {
                            return Err(
                                "That device name belongs to another connection. Use a new name."
                                    .into(),
                            );
                        }
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
                        self.device_ui
                            .set_devices(crate::device::list().unwrap_or_default());
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
                if panel == Panel::Changes && self.device_ui.pathspec_auto {
                    if let Some(file) = &self.device_ui.file {
                        self.device_ui.pathspec = file.display().to_string();
                    }
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
                if let Some(image) = self.device_ui.image.take() {
                    image.task.abort();
                    self.device_ui.generation = self.device_ui.generation.wrapping_add(1);
                    self.device_ui.pending = None;
                    self.device_ui.busy = false;
                }
                if !self.device_ui.busy {
                    self.device_ui.open = false;
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
                let reopen_origin = self.device_ui.reopen_origin.take();
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
                        let Some(origin) = reopen_origin
                            .filter(|_| matches!(pending, Some(Operation::Discover { .. })))
                        else {
                            self.device_ui.notice = Some("Project opening could not be confirmed. Open the project again to retry.".into());
                            return Task::none();
                        };
                        if let Err(error) = self.publish_device_reopen(
                            origin,
                            generation,
                            key,
                            path.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned(),
                            kind,
                        ) {
                            self.device_ui.notice = Some(error);
                            return Task::none();
                        }
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
                        self.chat_ui.image_viewer = Some(chat_ui::images::Gallery::file(
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
        self.device_ui.reopen_origin = matches!(operation, Operation::Discover { .. })
            .then(|| self.capture_device_origin(self.device_ui.generation));
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
                            )
                            .into(),
                            open.into(),
                        ])
                        .align_y(Alignment::Center)
                        .spacing(space::XS)
                        .into(),
                    );
                }
                rows.push(
                    widget::container(
                        widget::scrollable(
                            widget::column::with_children(choices)
                                .padding(design::with_vertical_rail(Padding::ZERO)),
                        )
                        .direction(iced::widget::scrollable::Direction::Vertical(
                            design::scrollbar(),
                        ))
                        .height(Length::Shrink),
                    )
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
            return dialog_ui::surface(
                widget::column::with_children(vec![
                    design::heading("Opening image").into(),
                    design::body(image.path.display().to_string()).into(),
                    design::caption(image.device.as_deref().unwrap_or("This device")).into(),
                    crate::ui_controls::button::secondary("Cancel")
                        .on_press(Message::Device(Action::Close))
                        .into(),
                ])
                .spacing(space::M),
                600.0,
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
            let mut rows=vec![design::title("Connect to a device").into(),design::body("Use an SSH host alias already configured on this computer. Install matching cagents and cagentsd on the device.").into(),crate::ui_controls::field::text("Device name",&state.config_name).on_input(|value|Message::Device(Action::ConfigName(value))).into(),crate::ui_controls::field::text("SSH host alias (for example build-host)",&state.config_host).on_input(|value|Message::Device(Action::ConfigHost(value))).into(),crate::ui_controls::field::text("Remote cagents executable",&state.config_executable).on_input(|value|Message::Device(Action::ConfigExecutable(value))).into(),widget::row::with_children(vec![button("Close",Action::Close),button("Save and switch device",Action::ConfigSave)]).spacing(space::S).into()];
            if let Some(notice) = &state.notice {
                rows.push(design::body(notice).into());
            }
            return dialog_ui::surface(
                widget::column::with_children(rows).spacing(space::M),
                600.0,
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
                (Panel::Worktrees, Icon::Branch, "Manage worktrees"),
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
                design::caption(format!("Current: {}", state.directory.display()))
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
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
        let output = |height| {
            widget::scrollable(
                design::body(if state.panel == Panel::Worktrees {
                    &state.worktree_output
                } else {
                    &state.output
                })
                .font(iced::Font::MONOSPACE)
                .width(Length::Fill),
            )
            .height(height)
            .width(Length::Fill)
            .into()
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
                                design::body(&entry.name)
                                    .wrapping(iced::widget::text::Wrapping::None)
                                    .ellipsize(iced::widget::text::Ellipsize::End(
                                        EllipsizeHeightLimit::Lines(1),
                                    ))
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
                let file_list =
                    widget::scrollable(widget::column::with_children(files).spacing(space::XXS))
                        .width(if narrow { 176 } else { 224 })
                        .height(height);
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
                            crate::ui_controls::passive_icon(Icon::Edit, 32).into(),
                            design::caption("Select a file").into(),
                        ])
                        .spacing(space::M)
                        .align_x(Alignment::Center),
                    )
                    .center(Length::Fill)
                    .height(height)
                    .into()
                };
                widget::row::with_children(vec![file_list.into(), editor])
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
        let content = widget::scrollable(
            widget::column::with_children(rows)
                .spacing(space::M)
                .padding(design::with_vertical_rail(Padding::new(space::L))),
        )
        .direction(iced::widget::scrollable::Direction::Vertical(
            design::scrollbar(),
        ))
        .height(Length::Fill);
        widget::container(crate::interaction::opaque(
            widget::container(content).class(cosmic::theme::Container::Dialog(false)),
        ))
        .height(Length::Fill)
        .max_height(size.height.min(820.0))
        .width(Length::Fill)
        .max_width(1100.0)
        .into()
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
        crate::device::select(Some("navigation-test".into())).unwrap();
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
        app.sidebar_model.projects = vec![SidebarProject::new(
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
        drop(app.device_project_loaded(closed, 1, Err("late reply".into())));
        app.refresh_ui.focused = true;
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
    fn device_project_reply_publishes_its_reopen_and_clears_the_registry_closed_marker() {
        use super::super::workspace_model_sync::ModelUpdate;
        use crate::workspace_model::{ModelEdit, WorkspaceModel};
        let mut app = super::super::tests::context_inspector_test_app(false);
        let key = PathBuf::from("/remote/reopened/.git");
        let mut closed = WorkspaceModel::default();
        closed.identity = uuid::Uuid::new_v4();
        closed.sequence = 4;
        closed.closed_projects.push(key.clone());
        let connected = ModelUpdate::connected_fixture(closed.clone());
        let ModelUpdate::Connected { proof, .. } = &connected else {
            unreachable!()
        };
        let proof = proof.clone();
        drop(app.update(Message::WorkspaceModelObserved(connected)));
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        app.device_ui.path = "/remote/reopened".into();
        drop(app.update(Message::Device(Action::OpenProject)));
        let generation = app.device_ui.generation;
        drop(app.update(Message::Device(Action::Loaded {
            generation,
            result: Ok(Reply::Project {
                path: "/remote/reopened".into(),
                key: key.clone(),
                kind: ProjectKind::Git,
            }),
        })));
        let request = rx
            .try_recv()
            .expect("device discovery queues a reopen without a local Git picker");
        assert!(
            matches!(&request.edit, ModelEdit::ReopenProject { repository, .. } if repository == &key)
        );
        assert!(
            request.reopen.is_some(),
            "the device request carries explicit local custody"
        );
        assert!(
            rx.try_recv().is_err(),
            "the callback queues exactly one reopen"
        );
        assert!(app.project_ui.is_reopening(&key));
        // The backend's canonical edit clears the marker; exercise its receipt
        // and the peer observer through the actual app reducers. Discovery and
        // this response are fixtures, not a live remote daemon/native test.
        let mut reopened = closed.clone();
        reopened.fold(closed.sequence + 1, &request.edit);
        assert!(!reopened.closed_projects.contains(&key));
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Reopened {
                repository: key.clone(),
                model: reopened.clone(),
                origin: request.reopen,
                destination: Some(proof),
            })),
        );
        assert_eq!(app.workspace_model.identity, closed.identity);
        assert!(!app.workspace_model.closed_projects.contains(&key));
        assert!(!app.project_ui.is_reopening(&key));
        assert!(!app.project_ui.closed.contains(&key));
        let mut peer = super::super::tests::context_inspector_test_app(false);
        drop(peer.update(Message::WorkspaceModelObserved(
            ModelUpdate::connected_fixture(closed),
        )));
        drop(
            peer.update(Message::WorkspaceModelObserved(ModelUpdate::Edited {
                sequence: reopened.sequence,
                edit: request.edit,
            })),
        );
        drop(peer.apply_closed_project_tombstones());
        assert!(!peer.workspace_model.closed_projects.contains(&key));
        assert!(!peer.project_ui.closed.contains(&key));
    }

    #[test]
    fn device_project_reply_preserves_dispatch_registry_host_and_generation() {
        use super::super::workspace_model_sync::ModelUpdate;
        use crate::workspace_model::WorkspaceModel;
        for replacement in [
            "registry",
            "host",
            "stale generation",
            "missing context",
            "close",
            "repeat close",
        ] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            let mut model = WorkspaceModel::default();
            model.identity = uuid::Uuid::new_v4();
            if replacement == "repeat close" {
                model.closed_projects.push("/remote/reopened/.git".into());
            }
            let connected = ModelUpdate::connected_fixture(model.clone());
            let ModelUpdate::Connected { proof, .. } = &connected else {
                unreachable!()
            };
            let mut proof = proof.clone();
            drop(app.update(Message::WorkspaceModelObserved(connected)));
            let (tx, rx) = async_channel::unbounded();
            app.workspace_model_tx = tx;
            app.device_ui.path = "/remote/reopened".into();
            if replacement != "missing context" {
                drop(app.update(Message::Device(Action::OpenProject)));
            }
            let generation = app.device_ui.generation;
            if replacement == "registry" {
                model.identity = uuid::Uuid::new_v4();
            }
            if replacement == "host" {
                proof.host_incarnation = "replacement-host".into();
            }
            if matches!(replacement, "registry" | "host") {
                drop(
                    app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected {
                        model: model.clone(),
                        proof,
                    })),
                );
            }
            if matches!(replacement, "close" | "repeat close") {
                drop(
                    app.update(Message::WorkspaceModelObserved(ModelUpdate::Edited {
                        sequence: model.sequence + 1,
                        edit: crate::workspace_model::ModelEdit::CloseProject {
                            repository: "/remote/reopened/.git".into(),
                        },
                    })),
                );
            }
            let reply = Reply::Project {
                path: "/remote/reopened".into(),
                key: "/remote/reopened/.git".into(),
                kind: ProjectKind::Git,
            };
            let active = app.active_workspace.clone();
            drop(app.update(Message::Device(Action::Loaded {
                generation: if replacement == "stale generation" {
                    generation.wrapping_sub(1)
                } else {
                    generation
                },
                result: Ok(reply.clone()),
            })));
            assert!(
                rx.try_recv().is_err(),
                "{replacement}: an old context cannot publish to a replacement"
            );
            assert_eq!(app.workspace_model.identity, model.identity);
            assert_eq!(
                app.active_workspace, active,
                "a rejected callback cannot switch the current workspace"
            );
            if replacement == "stale generation" {
                assert!(app.device_ui.reopen_origin.is_some());
                drop(app.update(Message::Device(Action::Loaded {
                    generation,
                    result: Ok(reply),
                })));
            } else {
                assert!(
                    app.device_ui
                        .notice
                        .as_deref()
                        .unwrap()
                        .contains("Open the project again to retry")
                );
                drop(app.update(Message::Device(Action::OpenProject)));
                drop(app.update(Message::Device(Action::Loaded {
                    generation: app.device_ui.generation,
                    result: Ok(reply),
                })));
            }
            assert!(
                rx.try_recv().unwrap().reopen.is_some(),
                "{replacement}: a current explicit request still works"
            );
        }
    }

    #[test]
    fn device_discovery_keeps_overtaken_closes_newer_than_its_dispatch() {
        use super::super::workspace_model_sync::ModelUpdate;
        use crate::workspace_model::{ModelEdit, WorkspaceModel};
        let key = PathBuf::from("/remote/reopened/.git");
        for connected_snapshot in [false, true] {
            for closed_after_dispatch in [false, true] {
                let mut app = super::super::tests::context_inspector_test_app(false);
                let mut model = WorkspaceModel::default();
                model.identity = uuid::Uuid::new_v4();
                model.sequence = if closed_after_dispatch { 4 } else { 6 };
                model.closed_projects.push(key.clone());
                drop(app.update(Message::WorkspaceModelObserved(
                    ModelUpdate::connected_fixture(model.clone()),
                )));
                let (tx, rx) = async_channel::unbounded();
                app.workspace_model_tx = tx;
                app.device_ui.path = "/remote/reopened".into();
                drop(app.update(Message::Device(Action::OpenProject)));
                let generation = app.device_ui.generation;
                let mut latest = model.clone();
                let reopened = ModelEdit::ReopenProject {
                    repository: key.clone(),
                    name: "reopened".into(),
                    kind: ProjectKind::Git,
                };
                let closed = ModelEdit::CloseProject {
                    repository: key.clone(),
                };
                if closed_after_dispatch {
                    // Two real changes return the key to its original state.
                    latest.fold(5, &reopened);
                    latest.fold(6, &closed);
                }
                let snapshot = if connected_snapshot {
                    ModelUpdate::connected_fixture(latest)
                } else {
                    ModelUpdate::Read(latest)
                };
                drop(app.update(Message::WorkspaceModelObserved(snapshot)));
                drop(
                    app.update(Message::WorkspaceModelObserved(ModelUpdate::Edited {
                        sequence: 5,
                        edit: reopened,
                    })),
                );
                drop(
                    app.update(Message::WorkspaceModelObserved(ModelUpdate::Edited {
                        sequence: 6,
                        edit: closed,
                    })),
                );
                let active = app.active_workspace.clone();
                drop(app.update(Message::Device(Action::Loaded {
                    generation,
                    result: Ok(Reply::Project {
                        path: "/remote/reopened".into(),
                        key: key.clone(),
                        kind: ProjectKind::Git,
                    }),
                })));
                if closed_after_dispatch {
                    assert!(
                        rx.try_recv().is_err(),
                        "a complete snapshot cannot hide a post-dispatch close"
                    );
                    assert_eq!(app.active_workspace, active);
                    assert!(app.workspace_model.closed_projects.contains(&key));
                    assert!(app.project_ui.closed.contains(&key));
                    assert!(
                        app.device_ui
                            .notice
                            .as_deref()
                            .unwrap()
                            .contains("closed while it was opening")
                    );
                    // A current explicit retry sees closed6 as its starting
                    // state, so a late copy of that edge must not veto it.
                    drop(app.update(Message::Device(Action::OpenProject)));
                    let generation = app.device_ui.generation;
                    drop(
                        app.update(Message::WorkspaceModelObserved(ModelUpdate::Edited {
                            sequence: 6,
                            edit: ModelEdit::CloseProject {
                                repository: key.clone(),
                            },
                        })),
                    );
                    drop(app.update(Message::Device(Action::Loaded {
                        generation,
                        result: Ok(Reply::Project {
                            path: "/remote/reopened".into(),
                            key: key.clone(),
                            kind: ProjectKind::Git,
                        }),
                    })));
                }
                assert!(
                    matches!(rx.try_recv().unwrap().edit, ModelEdit::ReopenProject { repository, .. } if repository == key),
                    "a close at or before dispatch does not veto an explicit reopen"
                );
                assert!(rx.try_recv().is_err());
            }
        }
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
        assert!(app.chat_ui.image_viewer.is_some());
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
        assert!(app.chat_ui.image_viewer.is_none());
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
        assert!(app.chat_ui.image_viewer.is_none());
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
    fn remote_sidebar_presentation_stays_in_the_sidebar() {
        for action in [
            SidebarAction::SearchChanged("project".into()),
            SidebarAction::SearchChanged(String::new()),
            SidebarAction::SetFilter(sidebar::SidebarFilter::Attention),
            SidebarAction::HideReadAttention,
            SidebarAction::ToggleControls,
            SidebarAction::FocusFirstSearchResult,
            SidebarAction::OpenFirstSearchResult,
            SidebarAction::Resize { width: 300.0 },
            SidebarAction::ResizeFinished,
            SidebarAction::ReorderMember {
                project: ProjectId::new(),
                source: sidebar::SelectionRow::Channel(crate::model::ChannelId::new()),
                before: None,
            },
            SidebarAction::ReorderProject {
                project: ProjectId::new(),
                before: None,
            },
            SidebarAction::ProjectCommand {
                project_id: ProjectId::new(),
                worktree_id: None,
                command: sidebar::ProjectCommand::ResetSidebarOrder,
            },
        ] {
            assert!(supports_sidebar_action(&action));
        }
        assert!(!supports_sidebar_action(
            &SidebarAction::OpenRepositoryPicker
        ));
    }

    #[test]
    fn linked_coordinates_preserve_unicode_boundaries_and_clamp_to_the_file() {
        let mut state = State::default();
        state.content = text_editor::Content::with_text("first\néx");
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
