//! Labelled entry paths for an empty workspace. An unselected project is not
//! the process's working directory, and a home chat has an explicit root.
use super::*;
use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};

static HOME_DIRECTORY: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    crate::device::home()
        .cloned()
        .or_else(|| directories::UserDirs::new().map(|dirs| dirs.home_dir().to_owned()))
});

#[derive(Debug, Clone)]
pub enum Action {
    Command(workspace_widget::GlobalShortcut),
    HomeChat,
    Clone,
    Url(String),
    Destination(String),
    Submit,
    Cancel,
    Finished {
        id: uuid::Uuid,
        result: Result<PathBuf, String>,
    },
}

#[derive(Debug)]
pub(super) struct CloneRepository {
    id: uuid::Uuid,
    url: String,
    destination: String,
    running: bool,
    error: Option<String>,
    cancellation: CancellationToken,
}

impl CloneRepository {
    pub(super) fn request(&self) -> crate::model::OperationId {
        crate::model::OperationId::from_uuid(self.id)
    }
}

impl Drop for CloneRepository {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub(super) fn clone_focus_id() -> widget::Id {
    widget::Id::new("onboarding.clone.url")
}

/// Empty-workspace actions own workspace commands without borrowing a pane.
/// Clone form fields deliberately remain ordinary native text inputs.
pub(super) fn keyboard_focus_regions() -> Vec<(widget::Id, crate::input_trace::KeyboardRegion)> {
    [
        "onboarding.open-folder",
        "onboarding.clone",
        "onboarding.home-chat",
        "empty.new-conversation",
        "empty.resume-conversation",
        "empty.new-terminal",
        "empty.new-browser",
        "empty.accounts",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain(Provider::ALL.into_iter().map(|provider| {
        format!("onboarding.provider.{}", provider.executable())
    }))
    .map(|id| (widget::Id::new(id), workspace_widget::FocusedPane::Other.into()))
    .collect()
}

fn clone_command(url: &str, destination: &Path) -> Result<CommandSpec, String> {
    let url = url.trim();
    if url.is_empty() || url.starts_with('-') || url.contains(['\n', '\r', '\0']) {
        return Err("Enter a repository URL or an absolute local repository path.".into());
    }
    if !destination.is_absolute() || destination.file_name().is_none() {
        return Err("Choose an absolute path for the new repository folder.".into());
    }
    // Refuse an existing folder even when empty: Git must only create the
    // destination this action promises, never take over a user's folder.
    if destination.symlink_metadata().is_ok() {
        return Err("That destination already exists. Choose a new folder name.".into());
    }
    if !destination.parent().is_some_and(Path::is_dir) {
        return Err("The destination's parent folder does not exist.".into());
    }
    Ok(CommandSpec::new("git")
        .args(["-c", "core.fsmonitor=false", "clone", "--"])
        .arg(url)
        .arg(destination.as_os_str())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .timeout(Duration::from_secs(600))
        .output_limits(64 * 1024, 256 * 1024))
}

fn clone_failure_detail(error: &crate::process::ProcessFailure) -> String {
    if error.kind == crate::process::ProcessFailureKind::Exit {
        // Git writes a progress line before authentication and transport errors.
        // Keep the actionable diagnosis instead of displaying only "Cloning into".
        if let Some(line) = String::from_utf8_lossy(&error.stderr)
            .lines()
            .map(str::trim)
            .find(|line| {
                line.starts_with("fatal:")
                    || line.starts_with("error:")
                    || line.starts_with("remote: error:")
            })
        {
            return line
                .chars()
                .take(crate::process::FAILURE_DETAIL_LIMIT)
                .collect();
        }
    }
    error.reported_detail()
}

fn execute_clone(
    command: &CommandSpec,
    cancellation: &CancellationToken,
    destination: PathBuf,
) -> Result<PathBuf, String> {
    SystemProcessExecutor
        .execute(command, cancellation)
        .and_then(|output| output.require_success(&command.program))
        .map(|_| destination)
        .map_err(|error| clone_failure_detail(&error))
}

fn onboarding_shortcut_detail(
    command: workspace_widget::GlobalShortcut,
    shortcuts: &KeyboardShortcuts,
) -> String {
    let chords = workspace_widget::shortcut_chords(command, shortcuts);
    if chords.is_empty() {
        "Enter to open the focused action".into()
    } else {
        chords
    }
}

impl Cagents {
    fn onboarding_workspace_rows(&self) -> Vec<WorkspaceSwitcherRow> {
        let actions = sidebar::workspace_cycle(&self.sidebar_model, &self.sidebar_state);
        let mut order = self.workspace_history.recent().to_vec();
        let mut seen: HashSet<_> = order.iter().copied().collect();
        for action in &actions {
            if let Some(destination) = WorkspaceDestination::from_action(action)
                && seen.insert(destination)
            {
                order.push(destination);
            }
        }
        self.build_workspace_switcher_rows(&order, actions)
    }

    pub(super) fn empty_workspace_view(&self) -> Element<'_, Message> {
        self.empty_workspace_view_with_context(self.chrome_command_context())
    }

    fn empty_workspace_view_with_context(
        &self,
        context: workspace_widget::CommandContext,
    ) -> Element<'_, Message> {
        use workspace_widget::GlobalShortcut as Command;
        let scoped = self.active_workspace.is_some() || self.durable.active_channel.is_some();
        let first_run = self.sidebar_model.projects.is_empty() && !scoped;
        let enabled = !self.workspace_inputs_blocked();
        let action_enabled = |command| enabled
            && workspace_widget::shortcut_descriptor(command).available(&context);
        let mut content: Vec<Element<'_, Message>> = Vec::new();
        if scoped {
            let directory = self.terminal_directory();
            let title = self
                .durable
                .active_channel
                .and_then(|id| {
                    self.durable
                        .channels
                        .iter()
                        .find(|channel| channel.id == id)
                })
                .map(|channel| channel.title.clone())
                .or_else(|| {
                    directory
                        .as_ref()
                        .and_then(|path| path.file_name())
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| "Worktree".into());
            content.push(design::display(title).into());
            if let Some(path) = directory {
                content.push(design::caption(display_path(&path).into_owned()).into());
            }
            for (label, icon, id, command, message) in [
                (
                    "New conversation",
                    Icon::Chat,
                    "empty.new-conversation",
                    Some(Command::NewThread),
                    Message::Onboarding(Action::Command(Command::NewThread)),
                ),
                (
                    "Resume conversation",
                    Icon::History,
                    "empty.resume-conversation",
                    Some(Command::ResumeConversation),
                    Message::Onboarding(Action::Command(Command::ResumeConversation)),
                ),
                (
                    "New terminal",
                    Icon::Terminal,
                    "empty.new-terminal",
                    Some(Command::NewTerminal),
                    Message::Onboarding(Action::Command(Command::NewTerminal)),
                ),
                (
                    if cfg!(feature = "integrated-browser") {
                        "New browser"
                    } else {
                        "Open in system browser"
                    },
                    Icon::Browser,
                    "empty.new-browser",
                    Some(Command::NewBrowser),
                    Message::Onboarding(Action::Command(Command::NewBrowser)),
                ),
                (
                    "Agent accounts",
                    Icon::Avatar,
                    "empty.accounts",
                    Some(Command::OpenAgentAccounts),
                    Message::Onboarding(Action::Command(Command::OpenAgentAccounts)),
                ),
            ] {
                content.push(
                    crate::ui_controls::button::secondary(label)
                        .icon(icon)
                        .id(widget::Id::new(id))
                        .width(Length::Fill)
                        .on_press_maybe(command.map_or(enabled, action_enabled).then_some(message))
                        .into(),
                );
            }
        } else {
            content.push(
                design::display(if first_run {
                    "Welcome to Cagents"
                } else {
                    "Select a worktree"
                })
                .into(),
            );
            content.push(design::body(if first_run {
                "Open a project folder to work with agents, or start a conversation in your home folder."
            } else {
                "Choose a recent workspace to continue, or open another project folder."
            }).wrapping(iced::widget::text::Wrapping::WordOrGlyph).into());
            for (label, detail, icon, id, command, message) in [
                (
                    "Open folder",
                    onboarding_shortcut_detail(
                        workspace_widget::GlobalShortcut::OpenFolder,
                        &self.settings.keyboard_shortcuts,
                    ),
                    Icon::OpenFolder,
                    "onboarding.open-folder",
                    Command::OpenFolder,
                    Message::Onboarding(Action::Command(Command::OpenFolder)),
                ),
                (
                    "Clone repository",
                    onboarding_shortcut_detail(
                        workspace_widget::GlobalShortcut::CloneRepository,
                        &self.settings.keyboard_shortcuts,
                    ),
                    Icon::Tree,
                    "onboarding.clone",
                    Command::CloneRepository,
                    Message::Onboarding(Action::Command(Command::CloneRepository)),
                ),
                (
                    "Start a chat in ~",
                    workspace_widget::shortcut_tooltip(
                        workspace_widget::GlobalShortcut::NewThread,
                        &self.settings.keyboard_shortcuts,
                    ),
                    Icon::Chat,
                    "onboarding.home-chat",
                    Command::NewThread,
                    Message::Onboarding(Action::HomeChat),
                ),
            ] {
                let button = if id == "onboarding.open-folder" {
                    crate::ui_controls::button::primary(label)
                } else {
                    crate::ui_controls::button::secondary(label)
                };
                content.push(
                    widget::column::with_children(vec![
                        button
                            .icon(icon)
                            .id(widget::Id::new(id))
                            .width(Length::Fill)
                            .on_press_maybe(action_enabled(command).then_some(message))
                            .into(),
                        design::caption(detail).into(),
                    ])
                    .spacing(space::XXS)
                    .into(),
                );
            }
            content.push(design::caption(
                "Projects are folders. Worktrees keep branches separate. Channels group related conversations and terminals."
            ).wrapping(iced::widget::text::Wrapping::WordOrGlyph).into());
            let recent = self.onboarding_workspace_rows();
            if !recent.is_empty() {
                content.push(design::heading("Recent workspaces").into());
                for row in recent.into_iter().take(6) {
                    content.push(
                        widget::column::with_children(vec![
                            crate::ui_controls::button::quiet(row.title)
                                .width(Length::Fill)
                                .on_press_maybe(enabled.then_some(Message::Sidebar(row.action)))
                                .into(),
                            design::caption(row.detail)
                                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                                .into(),
                        ])
                        .spacing(space::XXS)
                        .into(),
                    );
                }
            } else if !self.workspace_model.closed_projects.is_empty() {
                content.push(design::heading("Recent folders").into());
                for path in self.workspace_model.closed_projects.iter().rev().take(5) {
                    content.push(
                        crate::ui_controls::button::quiet(display_path(path).into_owned())
                            .icon(Icon::OpenFolder)
                            .width(Length::Fill)
                            .on_press_maybe(
                                enabled.then(|| Message::RepositorySelected(Ok(path.clone()))),
                            )
                            .into(),
                    );
                }
            }
            content.push(design::heading("Agent providers").into());
            for provider in crate::agent::Provider::ALL {
                let status = self.account_ui.provider_status(provider);
                let action = self.account_ui.onboarding_action(provider);
                let label = action.as_ref().map_or("Checking…", |(label, _)| *label);
                content.push(
                    widget::row::with_children(vec![
                        widget::column::with_children(vec![
                            design::body(provider.label()).into(),
                            design::caption(status)
                                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                                .into(),
                        ])
                        .spacing(space::XXS)
                        .width(Length::Fill)
                        .into(),
                        crate::ui_controls::button::secondary(label)
                            .id(widget::Id::new(format!(
                                "onboarding.provider.{}",
                                provider.executable()
                            )))
                            .name(format!("{} · {status} · {label}", provider.label()))
                            .on_press_maybe(
                                action
                                    .filter(|_| enabled)
                                    .map(|(_, action)| Message::AccountUi(action)),
                            )
                            .into(),
                    ])
                    .align_y(Alignment::Center)
                    .spacing(space::M)
                    .into(),
                );
            }
        }
        let surface = widget::container(widget::column::with_children(content).spacing(space::M))
            .padding(space::L)
            .width(Length::Fill)
            .max_width(600);
        widget::scrollable(
            widget::container(surface)
                .center_x(Length::Fill)
                .padding(space::L),
        )
        .height(Length::Fill)
        .width(Length::Fill)
        .into()
    }

    pub(super) fn update_onboarding(&mut self, action: Action) -> Task<Message> {
        // A clone form can sit under a higher priority confirmation. Its
        // retained field must not edit or submit while that dialog owns input.
        if matches!(
            &action,
            Action::Url(_) | Action::Destination(_) | Action::Submit | Action::Cancel
        ) && (self.top_modal() != Some(modal_ui::Modal::Clone) || self.update_prepared)
        {
            return Task::none();
        }
        let required = match &action {
            Action::HomeChat => Some(workspace_widget::GlobalShortcut::NewThread),
            Action::Clone => Some(workspace_widget::GlobalShortcut::CloneRepository),
            _ => None,
        };
        if let Some(command) = required
            && !workspace_widget::shortcut_descriptor(command).available(&self.chrome_command_context())
        {
            // Reuse the registry's execution refusal if a previously rendered
            // Home action arrives after its device or save context changed.
            return self.run_chrome_workspace_command(command);
        }
        match action {
            Action::Command(command) => self.run_chrome_workspace_command(command),
            Action::HomeChat => {
                let Some(home) = HOME_DIRECTORY.clone() else {
                    self.status = Some(TransientNotice::error(
                        "Could not locate your home folder. Open a folder to start a conversation.",
                    ));
                    return Task::none();
                };
                // A home draft has an explicit root rather than the
                // launcher's working directory.
                self.new_native_chat(self.preferred_onboarding_provider(), Some(home))
            }
            Action::Clone => {
                if crate::device::is_remote() {
                    self.status = Some(TransientNotice::warning(
                        "Clone the repository in a device terminal, then choose Open folder to add it on that device.",
                    ));
                    return self.update_device(device_ui::Action::Open);
                }
                self.chrome_ui.clone_repository = Some(CloneRepository {
                    id: uuid::Uuid::new_v4(),
                    url: String::new(),
                    destination: String::new(),
                    running: false,
                    error: None,
                    cancellation: CancellationToken::new(),
                });
                self.modal_focus(clone_focus_id())
            }
            Action::Url(value) => {
                if let Some(dialog) = self
                    .chrome_ui
                    .clone_repository
                    .as_mut()
                    .filter(|dialog| !dialog.running)
                {
                    dialog.url = value;
                    dialog.error = None;
                }
                Task::none()
            }
            Action::Destination(value) => {
                if let Some(dialog) = self
                    .chrome_ui
                    .clone_repository
                    .as_mut()
                    .filter(|dialog| !dialog.running)
                {
                    dialog.destination = value;
                    dialog.error = None;
                }
                Task::none()
            }
            Action::Cancel => {
                self.chrome_ui.clone_repository = None;
                let target = if self.workspace.cards().is_empty()
                    && self.active_workspace.is_none()
                    && self.durable.active_channel.is_none()
                {
                    Some(widget::Id::new("onboarding.clone"))
                } else {
                    self.workspace
                        .focused()
                        .map(|card| self.card_keyboard_focus(card))
                        .or_else(|| Some(widget::Id::new(TOOLBAR_FOCUS_ID)))
                };
                target.map_or_else(Task::none, |target| self.modal_restore_focus(target))
            }
            Action::Submit => {
                let Some(dialog) = self
                    .chrome_ui
                    .clone_repository
                    .as_mut()
                    .filter(|dialog| !dialog.running)
                else {
                    return Task::none();
                };
                let url = dialog.url.clone();
                let destination = PathBuf::from(dialog.destination.trim());
                let id = dialog.id;
                let cancellation = dialog.cancellation.clone();
                dialog.running = true;
                dialog.error = None;
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        // Destination validation also touches the filesystem;
                        // a network parent must never stall the UI thread.
                        let command = clone_command(&url, &destination)?;
                        execute_clone(&command, &cancellation, destination)
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                    Message::Onboarding(Action::Finished { id, result })
                })
            }
            Action::Finished { id, result } => {
                let Some(dialog) = self
                    .chrome_ui
                    .clone_repository
                    .as_mut()
                    .filter(|dialog| dialog.id == id)
                else {
                    return Task::none();
                };
                match result {
                    Ok(path) => {
                        self.chrome_ui.clone_repository = None;
                        self.update(Message::RepositorySelected(Ok(path)))
                    }
                    Err(error) => {
                        dialog.running = false;
                        dialog.error = Some(format!(
                            "Could not clone: {error}. If Git created a partial folder, choose a new destination before retrying."
                        ));
                        Task::none()
                    }
                }
            }
        }
    }

    fn preferred_onboarding_provider(&self) -> crate::agent::Provider {
        crate::agent::Provider::ALL
            .into_iter()
            .find(|provider| self.account_ui.provider_ready(*provider))
            .unwrap_or(crate::agent::Provider::Codex)
    }

    pub(super) fn clone_repository_dialog(&self) -> Option<Element<'_, Message>> {
        let dialog = self.chrome_ui.clone_repository.as_ref()?;
        let can_submit = !dialog.running && !dialog.url.trim().is_empty()
            && !dialog.destination.trim().is_empty();
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::title("Clone repository").into(),
            design::caption("Use an HTTPS or SSH URL, or an absolute local repository path. Existing Git credentials are used.")
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph).into(),
            design::caption("Repository URL").into(),
            crate::ui_controls::field::text("https://github.com/owner/repository.git", &dialog.url)
                .id(clone_focus_id()).on_input_maybe((!dialog.running).then_some(|value| Message::Onboarding(Action::Url(value))))
                .on_submit_maybe(can_submit.then_some(|_| Message::Onboarding(Action::Submit)))
                .into(),
            design::caption("New folder (absolute path)").into(),
            crate::ui_controls::field::text("/home/you/projects/repository", &dialog.destination)
                .id(widget::Id::new("onboarding.clone.destination"))
                .on_input_maybe((!dialog.running).then_some(|value| Message::Onboarding(Action::Destination(value))))
                .on_submit_maybe(can_submit.then_some(|_| Message::Onboarding(Action::Submit)))
                .into(),
        ];
        if let Some(error) = &dialog.error {
            rows.push(notice_body(error));
        }
        if dialog.running {
            rows.push(design::body("Cloning repository…").into());
        }
        rows.push(dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary(if dialog.running { "Stop clone" } else { "Cancel" })
                .id(widget::Id::new("onboarding.clone.cancel"))
                .on_press(Message::Onboarding(Action::Cancel)),
            crate::ui_controls::button::primary("Clone and open")
                .on_press_maybe(can_submit.then_some(Message::Onboarding(Action::Submit))),
        ));
        Some(dialog_ui::overlay(
            dialog_ui::surface(widget::column::with_children(rows).spacing(space::S), 560.0),
            Some(Message::Onboarding(Action::Cancel)),
            1.0,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
    use cosmic::iced::{Event, Rectangle, Size};

    #[test]
    fn mounted_empty_workspace_actions_keep_workspace_shortcuts_after_native_focus() {
        use iced::advanced::widget::operation::focusable;
        use iced::runtime::user_interface::{Cache, UserInterface};
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut renderer = super::super::tests::headless_renderer();
        for (scoped, ids) in [
            (false, &["onboarding.open-folder", "onboarding.clone", "onboarding.home-chat"][..]),
            (true, &["empty.new-conversation", "empty.resume-conversation", "empty.new-terminal",
                "empty.new-browser", "empty.accounts"][..]),
        ] {
            for id in ids {
                let mut app = super::super::tests::context_inspector_test_app(false);
                if scoped { app.active_workspace = Some(root.path().to_owned()); }
                // Render the actual application observer, not a test-only
                // scope wrapper around a synthetic button.
                let mut ui = UserInterface::build(cosmic::Application::view(&app),
                    Size::new(1400., 1400.), Cache::new(), &mut renderer);
                ui.operate(&renderer, &mut focusable::focus::<()>(widget::Id::new(*id)));
                let key = keyboard::Key::Character("t".into());
                let event = keyboard::Event::KeyPressed {
                    key: key.clone(), modified_key: key,
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyT),
                    location: keyboard::Location::Standard, modifiers: keyboard::Modifiers::CTRL,
                    text: None, repeat: false,
                };
                let mut messages = Vec::new();
                let _ = ui.update(&[Event::Keyboard(event.clone())], mouse::Cursor::Unavailable,
                    &mut renderer, &mut clipboard::Null, &mut messages);
                let routed: Vec<_> = messages.iter().filter_map(|message| match message {
                    Message::RoutedKeyEvent { region, widget, .. } => Some((*region, widget.clone())),
                    _ => None,
                }).collect();
                assert_eq!(routed, vec![(workspace_widget::FocusedPane::Other.into(),
                    Some(widget::Id::new(*id)))], "actual native action focus must retain its identity: {id}");
                assert_eq!(workspace_widget::global_shortcut(&event, &app.settings.keyboard_shortcuts,
                    routed[0].0.pane), Some(workspace_widget::GlobalShortcut::NewThread),
                    "CtrlT must remain available after focusing {id}");
            }
        }
    }

    fn activate(mut element: Element<'_, Message>, id: &str) -> Vec<Message> {
        let renderer = super::super::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let viewport = Rectangle::with_size(Size::new(1200.0, 1400.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut iced::advanced::widget::operation::focusable::focus::<()>(widget::Id::new(
                id.to_owned(),
            )),
        );
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            }),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        messages
    }

    fn labels(mut element: Element<'_, Message>) -> Vec<String> {
        let renderer = super::super::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(1200.0, 1400.0)),
        );
        let nodes =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| node.node().label().map(str::to_owned))
            .collect()
    }

    #[test]
    fn first_run_open_and_clone_hints_follow_registered_bindings() {
        use workspace_widget::GlobalShortcut as Command;
        let mut app = super::super::tests::context_inspector_test_app(false);
        for command in [Command::OpenFolder, Command::CloneRepository] {
            assert_eq!(
                onboarding_shortcut_detail(command, &app.settings.keyboard_shortcuts),
                "Enter to open the focused action"
            );
        }
        app.settings.keyboard_shortcuts.open_folder = vec!["Ctrl+Alt+O".into()];
        app.settings.keyboard_shortcuts.clone_repository = vec!["Ctrl+Alt+L".into()];
        let rendered = labels(app.empty_workspace_view());
        for chord in ["Ctrl+Alt+O", "Ctrl+Alt+L"] {
            assert!(
                rendered.iter().any(|label| label == chord),
                "configured first-run hint is visible: {chord}"
            );
        }
        app.settings.keyboard_shortcuts.open_folder.clear();
        assert_eq!(
            onboarding_shortcut_detail(Command::OpenFolder, &app.settings.keyboard_shortcuts),
            "Enter to open the focused action"
        );
    }

    #[test]
    fn first_run_view_has_labelled_keyboard_actions_and_provider_status() {
        let app = super::super::tests::context_inspector_test_app(false);
        let labels = labels(app.empty_workspace_view());
        for label in ["Open folder", "Clone repository", "Start a chat in ~"] {
            assert!(
                labels.iter().any(|shown| shown == label),
                "missing visible action: {label}"
            );
        }
        for provider in crate::agent::Provider::ALL {
            let expected = format!(
                "{} · {} · Checking…",
                provider.label(),
                app.account_ui.provider_status(provider)
            );
            assert!(labels.iter().any(|label| label == &expected));
        }
        assert!(
            activate(app.empty_workspace_view(), "onboarding.open-folder")
                .iter()
                .any(|m| matches!(m, Message::Onboarding(Action::Command(workspace_widget::GlobalShortcut::OpenFolder))))
        );
        assert!(
            activate(app.empty_workspace_view(), "onboarding.clone")
                .iter()
                .any(|m| matches!(m, Message::Onboarding(Action::Command(workspace_widget::GlobalShortcut::CloneRepository))))
        );
        assert!(
            activate(app.empty_workspace_view(), "onboarding.home-chat")
                .iter()
                .any(|m| matches!(m, Message::Onboarding(Action::HomeChat)))
        );
        assert!(!labels.iter().any(|label| label.contains("Workspace /")));
    }

    #[test]
    fn home_buttons_match_registry_device_and_save_availability() {
        use workspace_widget::GlobalShortcut as Command;
        let app = super::super::tests::context_inspector_test_app(false);
        for (remote_device, save_in_flight) in [(false, false), (true, false), (false, true)] {
            for (id, command) in [
                ("onboarding.open-folder", Command::OpenFolder),
                ("onboarding.clone", Command::CloneRepository),
                ("onboarding.home-chat", Command::NewThread),
            ] {
                let mut context = app.chrome_command_context();
                context.remote_device = remote_device;
                context.save_in_flight = save_in_flight;
                let expected = workspace_widget::shortcut_descriptor(command).available(&context);
                let messages = activate(app.empty_workspace_view_with_context(context), id);
                let actions: Vec<_> = messages.iter().filter(|message| matches!(message,
                    Message::Onboarding(Action::Command(_) | Action::HomeChat))).collect();
                assert_eq!(actions.len(), usize::from(expected),
                    "actual focused Home button must agree with registry: {id}, remote={remote_device}, saving={save_in_flight}");
                if command != Command::NewThread && expected {
                    assert!(matches!(actions[0], Message::Onboarding(Action::Command(actual)) if *actual == command));
                }
            }
        }
    }

    #[test]
    fn scoped_empty_buttons_use_workspace_registry_without_sidebar_row_authority() {
        use workspace_widget::GlobalShortcut as Command;
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.active_workspace = Some(PathBuf::from("/workspace/selected"));
        for (remote_device, save_in_flight) in [(false, false), (true, false), (false, true)] {
            for (id, command) in [
                ("empty.new-conversation", Command::NewThread),
                ("empty.resume-conversation", Command::ResumeConversation),
                ("empty.new-terminal", Command::NewTerminal),
                ("empty.new-browser", Command::NewBrowser),
                ("empty.accounts", Command::OpenAgentAccounts),
            ] {
                let mut context = app.chrome_command_context();
                context.remote_device = remote_device;
                context.save_in_flight = save_in_flight;
                assert!(context.sidebar_commands.is_none());
                let expected = workspace_widget::shortcut_descriptor(command).available(&context);
                let messages = activate(app.empty_workspace_view_with_context(context), id);
                assert_eq!(messages.iter().filter(|message| matches!(message,
                    Message::Onboarding(Action::Command(actual)) if *actual == command)).count(),
                    usize::from(expected), "scoped button must agree with registry: {id}");
            }
        }
        assert!(activate(app.empty_workspace_view(), "empty.accounts").iter().any(|message|
            matches!(message, Message::Onboarding(Action::Command(Command::OpenAgentAccounts)))));
    }

    #[test]
    fn queued_home_commands_respect_update_freeze_without_a_confirmation_modal() {
        use workspace_widget::GlobalShortcut as Command;
        for command in [Command::NewThread, Command::CloneRepository, Command::ResumeConversation, Command::OpenAgentAccounts] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            assert!(!app.modal_is_open());
            assert!(!app.workspace_inputs_blocked());
            app.update_prepared = true;
            let message = Message::Onboarding(Action::Command(command));
            assert!(super::super::user_workspace_message_blocked_while_modal(&message));
            assert!(super::super::user_message_blocked_during_update(&message));
            let cards = app.durable.cards.len();
            let focused = app.workspace.focused();
            drop(app.update(message));
            assert_eq!(app.durable.cards.len(), cards);
            assert_eq!(app.workspace.focused(), focused);
            assert!(app.chrome_ui.clone_repository.is_none());
            assert!(!app.modal_is_open(), "a frozen command must not open a form");
        }
        let mut app = super::super::tests::context_inspector_test_app(false);
        drop(app.update_onboarding(Action::Clone));
        let form = app.chrome_ui.clone_repository.as_mut().unwrap();
        form.running = true;
        let id = form.id;
        app.update_prepared = true;
        let completed = Message::Onboarding(Action::Finished { id, result: Err("fixture failure".into()) });
        assert!(!super::super::user_message_blocked_during_update(&completed));
        drop(app.update(completed));
        let form = app.chrome_ui.clone_repository.as_ref().unwrap();
        assert!(!form.running, "background completion must settle during the freeze");
        assert!(form.error.as_ref().unwrap().contains("fixture failure"));
    }

    #[test]
    fn no_selection_lists_live_workspaces_in_recent_order() {
        use crate::sidebar::{SidebarProject, SidebarWorktree};
        let mut app = super::super::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        let first = crate::model::WorktreeId::new();
        let second = crate::model::WorktreeId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "Example".into(),
            "/example".into(),
            vec![
                SidebarWorktree::from_durable(first, "/example".into(), Some("main".into())),
                SidebarWorktree::from_durable(
                    second,
                    "/example-feature".into(),
                    Some("feature".into()),
                ),
            ],
            Vec::new(),
        ));
        app.workspace_history
            .observe(Some(WorkspaceDestination::Worktree(second)));
        let rows = app.onboarding_workspace_rows();
        assert_eq!(rows[0].destination, WorkspaceDestination::Worktree(second));
        assert!(
            rows.iter()
                .any(|row| row.destination == WorkspaceDestination::Worktree(first))
        );
        let labels = labels(app.empty_workspace_view());
        assert!(labels.iter().any(|label| label.contains("feature")));
        assert!(!labels.iter().any(|label| label.contains("Workspace /")));
    }

    #[test]
    fn home_chat_creates_a_draft_rooted_at_home_without_a_picker() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let home = HOME_DIRECTORY.as_ref().expect("test user has a home");
        let _ = app.update(Message::Onboarding(Action::HomeChat));
        let card = app
            .card_state(app.workspace.focused().expect("new chat card"))
            .unwrap();
        let CardKind::Chat(session) = &card.kind else {
            panic!("home action must create a conversation");
        };
        assert_eq!(&session.launch.recipe.working_directory, home);
    }

    #[test]
    fn clone_validation_preserves_existing_directories_and_literal_arguments() {
        let fixture = tempfile::tempdir().unwrap();
        let destination = fixture.path().join("repo with spaces");
        let command = clone_command("git@example.com:owner/repository.git", &destination).unwrap();
        assert_eq!(command.arguments[3], "--");
        assert_eq!(command.arguments[4], "git@example.com:owner/repository.git");
        assert_eq!(command.arguments[5], destination.as_os_str());
        assert!(clone_command("--upload-pack=bad", &destination).is_err());
        assert!(clone_command("https://example.com/repo", Path::new("relative")).is_err());
        std::fs::create_dir(&destination).unwrap();
        assert!(clone_command("https://example.com/repo", &destination).is_err());
    }

    #[test]
    fn clone_submit_defers_validation_and_reports_its_worker_result_inline() {
        let fixture = tempfile::tempdir().unwrap();
        let existing = fixture.path().join("existing");
        std::fs::create_dir(&existing).unwrap();
        for (url, destination, expected) in [
            (
                "https://example.invalid/repository.git",
                existing,
                "destination already exists",
            ),
            (
                "--upload-pack=invalid",
                fixture.path().join("new"),
                "Enter a repository URL",
            ),
        ] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            drop(app.update_onboarding(Action::Clone));
            let dialog = app.chrome_ui.clone_repository.as_mut().unwrap();
            dialog.url = url.into();
            dialog.destination = destination.to_string_lossy().into_owned();
            let id = dialog.id;
            let task = app.update_onboarding(Action::Submit);
            let dialog = app.chrome_ui.clone_repository.as_ref().unwrap();
            assert!(dialog.running, "Submit must schedule the worker before validation");
            assert!(dialog.error.is_none(), "validation must not run during Submit dispatch");
            let reply = super::super::tests::task_app_message(task);
            let Message::Onboarding(Action::Finished { id: actual, result }) = reply else {
                panic!("clone worker must return its owned result");
            };
            assert_eq!(actual, id);
            assert!(result.as_ref().unwrap_err().contains(expected));
            drop(app.update_onboarding(Action::Finished { id, result }));
            let dialog = app.chrome_ui.clone_repository.as_ref().unwrap();
            assert!(!dialog.running);
            assert!(dialog.error.as_ref().unwrap().contains(expected));
            assert!(app.sidebar_model.projects.is_empty());
        }
    }

    #[test]
    fn clone_failure_reports_git_diagnosis_without_hiding_timeouts() {
        let mut error = crate::process::ProcessFailure {
            kind: crate::process::ProcessFailureKind::Exit,
            program: "git".into(),
            exit_code: Some(128),
            signal: None,
            stdout: Vec::new(),
            stderr: b"Cloning into 'project'...\nfatal: Authentication failed\n".to_vec(),
            message: "exited with code 128".into(),
            started: true,
        };
        assert_eq!(clone_failure_detail(&error), "fatal: Authentication failed");
        error.kind = crate::process::ProcessFailureKind::TimedOut;
        error.message = "timed out".into();
        assert_eq!(clone_failure_detail(&error), "git: timed out");
    }

    #[test]
    fn clone_command_clones_a_local_repository_into_a_new_folder() {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source.git");
        let destination = fixture.path().join("clone with spaces");
        let cancellation = CancellationToken::new();
        let init = CommandSpec::new("git")
            .args(["init", "--bare"])
            .arg(source.as_os_str())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        assert!(
            SystemProcessExecutor
                .execute(&init, &cancellation)
                .unwrap()
                .success
        );
        let command = clone_command(source.to_str().unwrap(), &destination)
            .unwrap()
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        assert_eq!(
            execute_clone(&command, &cancellation, destination.clone()).unwrap(),
            destination
        );
        assert!(destination.join(".git").is_dir());
    }

    #[test]
    fn failed_clone_keeps_the_form_open_without_opening_a_project() {
        let fixture = tempfile::tempdir().unwrap();
        let destination = fixture.path().join("new-project");
        let missing_source = fixture.path().join("missing-source.git");
        let command = clone_command(missing_source.to_str().unwrap(), &destination)
            .unwrap()
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        let mut app = super::super::tests::context_inspector_test_app(false);
        let _ = app.update_onboarding(Action::Clone);
        let dialog = app.chrome_ui.clone_repository.as_mut().unwrap();
        dialog.running = true;
        let id = dialog.id;
        let result = execute_clone(&command, &dialog.cancellation, destination.clone());
        assert!(result.as_ref().unwrap_err().contains("fatal:"));
        let _ = app.update_onboarding(Action::Finished { id, result });
        let dialog = app.chrome_ui.clone_repository.as_ref().unwrap();
        assert!(!dialog.running);
        assert!(dialog.error.as_ref().unwrap().contains("fatal:"));
        assert!(app.sidebar_model.projects.is_empty());
        assert!(!destination.exists());
    }

    #[test]
    fn clone_form_cannot_mutate_or_cancel_beneath_another_confirmation() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let _ = app.update_onboarding(Action::Clone);
        app.settings_reset_confirm = true;
        let _ = app.update_onboarding(Action::Url("/unexpected".into()));
        let _ = app.update_onboarding(Action::Cancel);
        let dialog = app.chrome_ui.clone_repository.as_ref().unwrap();
        assert!(dialog.url.is_empty());
        assert!(!dialog.cancellation.is_cancelled());
    }

    #[test]
    fn clone_cancel_ignores_late_results_and_stops_the_owned_process() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let _ = app.update_onboarding(Action::Clone);
        let dialog = app.chrome_ui.clone_repository.as_ref().unwrap();
        let id = dialog.id;
        let cancellation = dialog.cancellation.clone();
        assert!(app.modal_is_open());
        let _ = app.cancel_top_modal();
        assert!(cancellation.is_cancelled());
        assert!(!app.modal_is_open());
        let _ = app.update_onboarding(Action::Clone);
        let new_id = app.chrome_ui.clone_repository.as_ref().unwrap().id;
        let _ = app.update_onboarding(Action::Finished {
            id,
            result: Ok("/stale/repository".into()),
        });
        assert_eq!(app.chrome_ui.clone_repository.as_ref().unwrap().id, new_id);
        assert!(app.sidebar_model.projects.is_empty());
    }

    #[test]
    fn window_controls_have_exactly_one_owner_across_empty_docked_closed_and_overlay() {
        let empty = super::super::tests::context_inspector_test_app(false);
        assert!(empty.context_inspector_open);
        assert!(
            !empty.inspector_owns_window_controls(),
            "unmounted inspector cannot own window controls"
        );
        let mut app = super::super::tests::context_inspector_test_app(true);
        assert!(app.inspector_owns_window_controls());
        app.window_width = 640.0;
        assert_eq!(
            app.context_inspector_mount(),
            Some(InspectorLayout::Overlay)
        );
        assert!(!app.inspector_owns_window_controls());
        app.context_inspector_open = false;
        assert_eq!(app.context_inspector_mount(), None);
        assert!(!app.inspector_owns_window_controls());
    }
}

#[cfg(test)]
mod native_header_tests {
    use super::*;
    use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
    use cosmic::iced::{Event, Point, Rectangle, Size};

    fn close_messages(mut element: Element<'_, Message>) -> usize {
        let renderer = super::super::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let viewport = Rectangle::with_size(Size::new(1000.0, 900.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let mut messages = Vec::new();
        // Native header controls sit at the trailing edge. Exercise their
        // actual event handlers; a predicate assertion cannot catch a header
        // hidden beneath the inspector's modal foreground.
        let cursor = mouse::Cursor::Available(Point::new(980.0, 20.0));
        for event in [
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                cursor,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        messages
            .iter()
            .filter(|message| matches!(message, Message::WindowCloseRequested(_)))
            .count()
    }

    #[test]
    fn fresh_and_overlay_workspace_headers_render_working_close_control() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.core.set_main_window_id(Some(window::Id::RESERVED));
        app.core.window.show_close = true;
        assert_eq!(
            close_messages(app.workspace_chrome(widget::Space::new().into())),
            1
        );
        // The same workspace-owned header is mounted above the overlay scrim,
        // in the foreground's accessible/hit-tested subtree.
        assert_eq!(close_messages(app.overlay_window_header()), 1);
        let mut docked = super::super::tests::context_inspector_test_app(true);
        docked.core.set_main_window_id(Some(window::Id::RESERVED));
        docked.core.window.show_close = true;
        assert_eq!(
            close_messages(docked.workspace_chrome(widget::Space::new().into())),
            0
        );
        docked.window_width = 640.0;
        assert!(docked.context_inspector_is_overlay());
        assert_eq!(close_messages(docked.view()), 1);
    }

    #[test]
    fn clone_field_enter_matches_the_primary_buttons_enabled_state() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        drop(app.update_onboarding(Action::Clone));
        let renderer = super::super::tests::headless_renderer();
        for (running, filled, submits) in [(false, false, false), (false, true, true), (true, true, false)] {
            let form = app.chrome_ui.clone_repository.as_mut().unwrap();
            form.running = running;
            form.url = if filled { "https://example.invalid/repo.git".into() } else { String::new() };
            form.destination = if filled { "/fixture/clone".into() } else { String::new() };
            for id in [clone_focus_id(), widget::Id::new("onboarding.clone.destination")] {
                let mut element = app.clone_repository_dialog().unwrap();
                let mut tree = Tree::new(&element);
                let bounds = Rectangle::with_size(Size::new(800.0, 600.0));
                let node = element.as_widget_mut().layout(&mut tree, &renderer,
                    &layout::Limits::new(Size::ZERO, bounds.size()));
                element.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer,
                    &mut iced::advanced::widget::operation::focusable::focus::<()>(id));
                let mut messages = Vec::new();
                element.as_widget_mut().update(&mut tree, &Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Enter),
                    modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(), text: None, repeat: false,
                }), Layout::new(&node), mouse::Cursor::Unavailable, &renderer,
                    &mut clipboard::Null, &mut Shell::new(&mut messages), &bounds);
                assert_eq!(messages.iter().any(|message| matches!(message, Message::Onboarding(Action::Submit))), submits,
                    "running={running}, filled={filled}");
            }
        }
    }

    fn emitted_clone_form_message(app: &Cagents, focus: widget::Id, editing: bool) -> Message {
        let renderer = super::super::tests::headless_renderer();
        let mut element = app.dialog().expect("actual registered Clone surface");
        let mut tree = Tree::new(&element);
        let bounds = Rectangle::with_size(Size::new(800.0, 600.0));
        let node = element.as_widget_mut().layout(&mut tree, &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()));
        element.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer,
            &mut iced::advanced::widget::operation::focusable::focus::<()>(focus));
        let mut messages = Vec::new();
        let key = if editing { keyboard::Key::Character("x".into()) }
            else { keyboard::Key::Named(keyboard::key::Named::Enter) };
        element.as_widget_mut().update(&mut tree, &Event::Keyboard(keyboard::Event::KeyPressed {
            modified_key: key.clone(), key,
            physical_key: keyboard::key::Physical::Code(if editing { keyboard::key::Code::KeyX }
                else { keyboard::key::Code::Enter }),
            location: keyboard::Location::Standard, modifiers: keyboard::Modifiers::empty(),
            text: editing.then(|| "x".into()), repeat: false,
        }), Layout::new(&node), mouse::Cursor::Unavailable, &renderer,
            &mut clipboard::Null, &mut Shell::new(&mut messages), &bounds);
        messages.into_iter().find(|message| matches!(message,
            Message::ModalSurface { message, .. } if matches!(&**message, Message::Onboarding(_))))
            .expect("actual widget emits an owner-bound Clone action")
    }

    #[test]
    fn replaced_clone_request_rejects_held_widget_input_submit_and_cancel() {
        let directory = tempfile::tempdir().unwrap();
        let original_destination = directory.path().join("original").to_string_lossy().into_owned();
        let replacement_destination = directory.path().join("replacement").to_string_lossy().into_owned();
        let mut app = super::super::tests::context_inspector_test_app(false);
        drop(app.update_onboarding(Action::Clone));
        {
            let dialog = app.chrome_ui.clone_repository.as_mut().unwrap();
            dialog.url = "https://example.invalid/original.git".into();
            dialog.destination = original_destination.clone();
        }
        drop(app.finish_modal_update(Task::none()));
        let original = app.modal_surface_target(modal_ui::Modal::Clone);
        let held = [
            emitted_clone_form_message(&app, clone_focus_id(), true),
            emitted_clone_form_message(&app, clone_focus_id(), false),
            emitted_clone_form_message(&app, widget::Id::new("onboarding.clone.cancel"), false),
        ];
        // No intermediate reconciliation: the instance must fence delivery
        // before any prelude can discover the same-role replacement.
        drop(app.update_onboarding(Action::Clone));
        {
            let dialog = app.chrome_ui.clone_repository.as_mut().unwrap();
            dialog.url = "https://example.invalid/replacement.git".into();
            dialog.destination = replacement_destination.clone();
        }
        let replacement = app.modal_surface_target(modal_ui::Modal::Clone);
        assert_ne!(original, replacement);
        for decision in held {
            assert!(iced::runtime::task::into_stream(app.update(decision)).is_none());
            assert_eq!(app.modal_surface_target(modal_ui::Modal::Clone), replacement);
            let dialog = app.chrome_ui.clone_repository.as_ref().unwrap();
            assert_eq!(dialog.url, "https://example.invalid/replacement.git");
            assert_eq!(dialog.destination, replacement_destination);
            assert!(!dialog.running);
        }
        drop(app.finish_modal_update(Task::none()));
        let current = emitted_clone_form_message(&app, clone_focus_id(), true);
        drop(app.update(current));
        assert_ne!(app.chrome_ui.clone_repository.as_ref().unwrap().url,
            "https://example.invalid/replacement.git");
        let submit = emitted_clone_form_message(&app, clone_focus_id(), false);
        // The worker is deliberately not polled: this is user-consent
        // admission, not a live/network Git clone test.
        drop(app.update(submit));
        assert!(app.chrome_ui.clone_repository.as_ref().unwrap().running);
        let cancel = emitted_clone_form_message(&app, widget::Id::new("onboarding.clone.cancel"), false);
        drop(app.update(cancel));
        assert!(app.chrome_ui.clone_repository.is_none());
    }
}
