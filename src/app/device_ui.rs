//! Native tools for remote files and source control. They never resolve a
//! selected device's path through the local filesystem.
use super::*;
use crate::device_service::{GitOperation, Operation, Reply};
use cosmic::widget::text_editor;

#[derive(Debug, Clone)]
pub enum Action {
    Menu(bool),
    Open,
    Configure,
    ConfigName(String),
    ConfigHost(String),
    ConfigExecutable(String),
    ConfigSave,
    ConfigSaved(Result<(), String>),
    Close,
    BackToFile,
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
    Device(usize),
    DeviceOpened(Result<(), String>),
}
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    #[default]
    Editor,
    GitOutput,
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
    view: View,
    pathspec: String,
    worktree_name: String,
    worktree_branch: String,
    commit_message: String,
    busy: bool,
    generation: u64,
    pending: Option<Operation>,
    notice: Option<String>,
    pub devices: Vec<String>,
}
impl Default for State {
    fn default() -> Self {
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
            view: View::Editor,
            pathspec: String::new(),
            worktree_name: String::new(),
            worktree_branch: String::new(),
            commit_message: String::new(),
            busy: false,
            generation: 0,
            pending: None,
            notice: None,
            devices: std::iter::once("local".to_owned())
                .chain(
                    crate::device::list()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|device| device.name),
                )
                .collect(),
        }
    }
}
impl Cagents {
    pub(super) fn update_device(&mut self, action: Action) -> Task<Message> {
        if let Action::Menu(open) = action {
            self.device_ui.menu_open =
                open && self.settings.show_status_bar && !self.modal_is_open();
            if self.device_ui.menu_open {
                match crate::device::list() {
                    Ok(devices) => {
                        self.device_ui.devices = std::iter::once("local".into())
                            .chain(devices.into_iter().map(|device| device.name))
                            .collect()
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
                    | Action::Device(_)
                    | Action::DeviceOpened(_)
                    | Action::ConfigSaved(_)
            )
        {
            return Task::none();
        }
        if matches!(
            &action,
            Action::Close
                | Action::Open
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
                        self.device_ui.devices = std::iter::once("local".into())
                            .chain(
                                crate::device::list()
                                    .unwrap_or_default()
                                    .into_iter()
                                    .map(|device| device.name),
                            )
                            .collect();
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
            Action::DeviceOpened(result) => {
                if let Err(error) = result {
                    self.status = Some(TransientNotice::warning(error));
                }
                return Task::none();
            }
            Action::Device(index) => {
                let Some(name) = self.device_ui.devices.get(index).cloned() else {
                    return Task::none();
                };
                if name
                    == crate::device::selected()
                        .map(|device| device.name.as_str())
                        .unwrap_or("local")
                {
                    return Task::none();
                }
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                        let executable = crate::control::sibling_executable("cagents").map_err(|error| error.to_string())?;
                        let status = std::process::Command::new(executable).args(["--device", &name, "ui"]).status().map_err(|error| error.to_string())?;
                        if status.success() { Ok(Reply::Saved) } else { Err(format!("could not open device {name}; verify devices.json and SSH access")) }
                    }).await.unwrap_or_else(|error| Err(error.to_string()))
                    },
                    |result| {
                        cosmic::Action::App(Message::Device(Action::DeviceOpened(
                            result.map(|_| ()),
                        )))
                    },
                );
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
            Action::BackToFile => {
                self.device_ui.view = View::Editor;
                return Task::none();
            }
            Action::DiscardChanges => {
                self.device_ui.content = text_editor::Content::with_text(&self.device_ui.original);
                self.device_ui.notice = Some("Unsaved changes discarded".into());
                return Task::none();
            }
            Action::Close => {
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
            Action::Read(path) => Some(Operation::Read {
                root: self.device_ui.directory.clone(),
                path,
            }),
            Action::Save => self.device_ui.file.as_ref().map(|path| Operation::Write {
                root: self.device_ui.directory.clone(),
                path: path.clone(),
                expected: self.device_ui.original.clone(),
                content: self.device_ui.content.text(),
            }),
            Action::Git(operation) => Some(Operation::Git {
                root: self.device_ui.path.clone().into(),
                operation,
            }),
            Action::OpenProject => Some(Operation::Discover {
                path: self.device_ui.path.clone().into(),
            }),
            Action::Loaded { generation, result } => {
                if generation != self.device_ui.generation {
                    return Task::none();
                }
                self.device_ui.busy = false;
                let pending = self.device_ui.pending.take();
                match result {
                    Err(error) => self.device_ui.notice = Some(error),
                    Ok(Reply::Directory { path, entries }) => {
                        self.device_ui.path = path.display().to_string();
                        self.device_ui.directory = path;
                        self.device_ui.entries = entries;
                        self.device_ui.file = None;
                        self.device_ui.output.clear();
                        self.device_ui.view = View::Editor;
                    }
                    Ok(Reply::Text(text)) if matches!(&pending, Some(Operation::Read { .. })) => {
                        if let Some(Operation::Read { root, path }) = pending {
                            self.device_ui.directory = root;
                            self.device_ui.file = Some(path);
                        }
                        self.device_ui.original = text.clone();
                        self.device_ui.content = text_editor::Content::with_text(&text);
                        self.device_ui.view = View::Editor;
                    }
                    Ok(Reply::Text(text)) => self.device_ui.output = text,
                    Ok(Reply::Saved) => {
                        self.device_ui.original = self.device_ui.content.text();
                        self.device_ui.notice = Some("Saved on device".into());
                    }
                    Ok(Reply::Project { path, key, kind }) => {
                        self.device_ui.path = path.display().to_string();
                        self.device_ui.notice =
                            Some(format!("Project opened on device: {}", path.display()));
                        self.device_ui.directory = path.clone();
                        self.active_workspace = Some(path.clone());
                        self.device_ui.file = None;
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
                    }
                    Ok(_) => self.device_ui.notice = Some("Unexpected device response".into()),
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
        if matches!(operation, Operation::Git { .. }) {
            self.device_ui.view = View::GitOutput;
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
                widget::icon(crate::ui_controls::symbolic_icon("go-up-symbolic"))
                    .size(12)
                    .into(),
            ])
            .spacing(design::GRID_UNIT)
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
                let mut rows: Vec<Element<'_, Message>> =
                    vec![widget::text::title4("Devices").into()];
                let mut choices = Vec::new();
                for (index, name) in self.device_ui.devices.iter().enumerate() {
                    let label = if name == "local" {
                        "This device"
                    } else {
                        name.as_str()
                    };
                    choices.push(
                        widget::button::text(label)
                            .class(crate::ui_controls::control_style(name == selected, None))
                            .width(Length::Fill)
                            .on_press(Message::Device(Action::Device(index)))
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
                rows.push(design::horizontal_rule());
                if crate::device::is_remote() {
                    rows.push(
                        widget::button::text("Browse device files…")
                            .width(Length::Fill)
                            .on_press(Message::Device(Action::Open))
                            .into(),
                    );
                }
                rows.push(
                    widget::button::text("Connect to a device…")
                        .width(Length::Fill)
                        .on_press(Message::Device(Action::Configure))
                        .into(),
                );
                widget::container(widget::column::with_children(rows).spacing(design::GRID_UNIT))
                    .width(260)
                    .padding(design::GRID_UNIT * 2.0)
                    .into()
            }),
            widget::popover::Position::Top,
            Message::Device(Action::Menu(false)),
        )
    }

    pub(super) fn device_dialog(&self) -> Element<'_, Message> {
        let state = &self.device_ui;
        let action = |action| Message::Device(action);
        let button = |label: &str, value: Action| {
            let mut button = widget::button::standard(label.to_owned());
            if !state.busy {
                button = button.on_press(action(value));
            }
            Element::from(button)
        };
        if state.configure {
            let mut rows=vec![widget::text::title3("Connect to a device").into(),widget::text::body("Use an SSH host alias already configured on this computer. Install matching cagents and cagentsd on the device.").into(),widget::text_input("Device name",&state.config_name).on_input(|value|Message::Device(Action::ConfigName(value))).into(),widget::text_input("SSH host alias (for example fw16)",&state.config_host).on_input(|value|Message::Device(Action::ConfigHost(value))).into(),widget::text_input("Remote cagents executable",&state.config_executable).on_input(|value|Message::Device(Action::ConfigExecutable(value))).into(),widget::row::with_children(vec![button("Close",Action::Close),button("Save and open window",Action::ConfigSave)]).spacing(8).into()];
            if let Some(notice) = &state.notice {
                rows.push(widget::text::body(notice).into());
            }
            return dialog_ui::surface(widget::column::with_children(rows).spacing(12), 600.0);
        }
        let mut rows = vec![
            widget::row::with_children(vec![
                widget::text::title3(format!(
                    "Files · {}",
                    crate::device::selected()
                        .map(|device| device.name.as_str())
                        .unwrap_or("local")
                ))
                .width(Length::Fill)
                .into(),
                button("Close", Action::Close),
            ])
            .spacing(8)
            .into(),
            widget::row::with_children(vec![
                widget::text_input("Absolute directory on device", &state.path)
                    .on_input(|value| Message::Device(Action::Path(value)))
                    .width(Length::Fill)
                    .into(),
                button("Browse", Action::Browse),
                button("Use directory", Action::OpenProject),
            ])
            .spacing(8)
            .into(),
        ];
        if let Some(notice) = &state.notice {
            rows.push(widget::text::body(notice).into());
        }
        if state.busy {
            rows.push(widget::text::body("Waiting for device…").into());
        }
        let mut files = Vec::new();
        if let Some(parent) = state.directory.parent() {
            files.push(button("../", Action::Visit(parent.to_owned())));
        }
        for entry in &state.entries {
            let label = format!(
                "{}{}",
                entry.name,
                if entry.directory {
                    "/"
                } else if entry.symlink {
                    " →"
                } else {
                    ""
                }
            );
            let value = if entry.directory {
                Action::Visit(state.directory.join(&entry.name))
            } else {
                Action::Read(entry.name.clone().into())
            };
            let mut item = widget::button::text(label).width(Length::Fill);
            if !state.busy && !entry.symlink {
                item = item.on_press(action(value));
            }
            files.push(item.into());
        }
        let preview: Element<'_, Message> = if let Some(file) = &state.file
            && state.view == View::Editor
        {
            widget::column::with_children(vec![
                widget::row::with_children(vec![
                    widget::text::body(file.display().to_string())
                        .width(Length::Fill)
                        .into(),
                    button("Save on device", Action::Save),
                    button("Discard changes", Action::DiscardChanges),
                ])
                .into(),
                iced::widget::text_editor(&state.content)
                    .on_action(|value| Message::Device(Action::Edit(value)))
                    .height(420)
                    .into(),
            ])
            .spacing(8)
            .width(Length::Fill)
            .into()
        } else {
            let mut output = Vec::new();
            if state.file.is_some() {
                let mut controls = vec![button("Back to file", Action::BackToFile)];
                if state.dirty() {
                    controls.push(button("Discard changes", Action::DiscardChanges));
                }
                output.push(widget::row::with_children(controls).spacing(8).into());
            }
            output.push(
                widget::scrollable(widget::text::body(&state.output))
                    .height(420)
                    .width(Length::Fill)
                    .into(),
            );
            widget::column::with_children(output)
                .spacing(8)
                .width(Length::Fill)
                .into()
        };
        rows.push(
            widget::row::with_children(vec![
                widget::scrollable(widget::column::with_children(files))
                    .width(260)
                    .height(460)
                    .into(),
                preview,
            ])
            .spacing(12)
            .into(),
        );
        rows.push(
            widget::row::with_children(vec![
                button("Git status", Action::Git(GitOperation::Status)),
                button("Diff", Action::Git(GitOperation::Diff { staged: false })),
                button(
                    "Staged diff",
                    Action::Git(GitOperation::Diff { staged: true }),
                ),
                widget::text_input("Relative file path", &state.pathspec)
                    .on_input(|value| Message::Device(Action::Pathspec(value)))
                    .into(),
                button(
                    "Stage",
                    Action::Git(GitOperation::Stage {
                        paths: vec![state.pathspec.clone().into()],
                    }),
                ),
                button(
                    "Unstage",
                    Action::Git(GitOperation::Unstage {
                        paths: vec![state.pathspec.clone().into()],
                    }),
                ),
            ])
            .spacing(8)
            .into(),
        );
        rows.push(
            widget::row::with_children(vec![
                widget::text_input("Commit message", &state.commit_message)
                    .on_input(|value| Message::Device(Action::CommitMessage(value)))
                    .width(Length::Fill)
                    .into(),
                button(
                    "Commit staged changes",
                    Action::Git(GitOperation::Commit {
                        message: state.commit_message.clone(),
                    }),
                ),
            ])
            .spacing(8)
            .into(),
        );
        rows.push(
            widget::row::with_children(vec![
                button("List worktrees", Action::Git(GitOperation::Worktrees)),
                widget::text_input("Worktree name under .worktrees/", &state.worktree_name)
                    .on_input(|value| Message::Device(Action::WorktreeName(value)))
                    .into(),
                widget::text_input("New branch", &state.worktree_branch)
                    .on_input(|value| Message::Device(Action::WorktreeBranch(value)))
                    .into(),
                button(
                    "Create worktree",
                    Action::Git(GitOperation::CreateWorktree {
                        name: state.worktree_name.clone(),
                        branch: state.worktree_branch.clone(),
                    }),
                ),
                button(
                    "Remove clean worktree",
                    Action::Git(GitOperation::RemoveWorktree {
                        name: state.worktree_name.clone(),
                    }),
                ),
            ])
            .spacing(8)
            .into(),
        );
        dialog_ui::surface(widget::column::with_children(rows).spacing(12), 1100.0)
    }
}

impl State {
    fn dirty(&self) -> bool {
        self.file.is_some() && self.content.text() != self.original
    }
    pub(super) fn busy(&self) -> bool {
        self.busy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(app.device_ui.view, View::GitOutput);
        drop(app.update_device(Action::Loaded {
            generation: app.device_ui.generation,
            result: Ok(Reply::Text("git status output".into())),
        }));
        assert_eq!(app.device_ui.output, "git status output");
        drop(app.update_device(Action::BackToFile));
        assert_eq!(app.device_ui.view, View::Editor);
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
    fn unsaved_file_blocks_navigation_and_close_until_explicit_save_or_discard() {
        for action in [
            Action::Close,
            Action::Open,
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
