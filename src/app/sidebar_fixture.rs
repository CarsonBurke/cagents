//! Fixed, development-only sidebar interaction data. No host or store exists.
//! Real sidebar widgets emit the actions below; no fixture command selects a
//! popup item or opens a confirmation. External work remains fenced off.
use super::*;
use crate::{
    agent::{Activity, ActivityState},
    model::{ProjectChannel, WorktreeId},
    repository::RepositoryId,
    sidebar::{CheckStatus, SelectionIntent, SelectionRow, SidebarEditor, SidebarWorktree},
};

const MODE: &str = "sidebar-row-2004";
const COMPACT: &str = "sidebar-row-2004-compact";
const ROOT: &str = "/offline-sidebar-fixture/original";
const TARGET: &str = "/offline-sidebar-fixture/merged-original";

fn selected(value: Option<&str>) -> bool {
    matches!(value, Some(MODE | COMPACT))
}

pub(super) fn enabled() -> bool {
    selected(std::env::var("CAGENTS_SIDEBAR_ROW_FIXTURE").ok().as_deref())
}

fn compact() -> bool {
    std::env::var("CAGENTS_SIDEBAR_ROW_FIXTURE").as_deref() == Ok(COMPACT)
}

fn project_id(value: u128) -> ProjectId {
    ProjectId::from_uuid(uuid::Uuid::from_u128(value))
}

fn worktree_id(value: u128) -> WorktreeId {
    WorktreeId::from_uuid(uuid::Uuid::from_u128(value))
}

fn channel_id(value: u128) -> ChannelId {
    ChannelId::from_uuid(uuid::Uuid::from_u128(value))
}

pub(super) fn bootstrap() -> Bootstrap {
    Bootstrap {
        workspace: WorkspaceState::default(),
        session: None,
        projects: Vec::new(),
        worktrees: Vec::new(),
        settings: AppSettings::default(),
        settings_path: None,
        settings_write_blocked: true,
        status: None,
        save_health: None,
        restore_warnings: Vec::new(),
        settings_warnings: Vec::new(),
    }
}

fn model() -> SidebarModel {
    let worktree = |id, path: &str, branch: &str| {
        SidebarWorktree::new(
            worktree_id(id),
            path.into(),
            Some(branch.into()),
            false,
            false,
            None,
            None,
            None,
        )
    };
    let mut merged = sidebar::SidebarPullRequest::new(
        project_id(1),
        2004,
        "Synthetic merged sidebar change".into(),
        "https://example.invalid/sidebar-fixture/2004"
            .parse()
            .unwrap(),
        "fix/sidebar-identifying-title-original".into(),
        None,
        false,
        CheckStatus::Unknown,
    );
    merged.state = Some(sidebar::WorktreePullRequestState::Merged);
    let original = SidebarProject::new(
        project_id(1),
        RepositoryId(uuid::Uuid::from_u128(11)),
        "Sidebar qualification — original project".into(),
        ROOT.into(),
        vec![
            worktree(2, ROOT, "main"),
            worktree(3, TARGET, "fix/sidebar-identifying-title-original"),
            worktree(
                4,
                "/offline-sidebar-fixture/ordinary",
                "fix/sidebar-identifying-title-neighbor",
            ),
        ],
        vec![merged],
    );
    let mut model = SidebarModel::ready(sidebar::SidebarMode::Expanded, vec![original]);
    // These are menu descriptions only. No editor executable is registered.
    model.external_editors = (0..6)
        .map(|index| SidebarEditor {
            id: format!("offline-editor-{index}"),
            name: format!("Synthetic editor {}", index + 1),
            icon: "text-editor-symbolic".into(),
        })
        .collect();
    model.projects[0].channels = [
        (
            20,
            "Sidebar identifying title — working original",
            Some(ActivityState::Working),
            false,
        ),
        (
            21,
            "Sidebar identifying title — waiting neighbor",
            Some(ActivityState::Waiting),
            true,
        ),
        (
            22,
            "Sidebar identifying title — unread finished",
            Some(ActivityState::Idle),
            true,
        ),
        (
            23,
            "Sidebar identifying title — quiet neighbor",
            None,
            false,
        ),
    ]
    .into_iter()
    .enumerate()
    .map(|(ordinal, (id, title, activity, unread))| {
        sidebar::SidebarChannel::new(
            channel_id(id),
            title.into(),
            false,
            activity,
            unread,
            0,
            true,
            ordinal as u32,
        )
        .with_attention(sidebar::AttentionSummary::card(activity, unread, id == 22))
    })
    .collect();
    model
}

pub(super) fn install(mut app: Cagents) -> (Cagents, Task<Message>) {
    app.sidebar_model = model();
    app.chrome_ui.projects_collapsed = compact();
    app.settings.sidebar_width = 260.0;
    app.context_inspector_open = false;
    // One inert terminal descriptor per channel makes the ordinary channel
    // Delete action ask its existing nonempty-channel confirmation. Nothing
    // initializes, starts, attaches, persists, or displays a terminal runtime.
    let durable = Arc::make_mut(&mut app.durable);
    for row in &app.sidebar_model.projects[0].channels {
        let card = CardId::from_uuid(row.id.as_uuid());
        let launch = LaunchRecipe {
            program: "/offline-sidebar-fixture/no-executable".into(),
            arguments: Vec::new(),
            working_directory: ROOT.into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        durable.cards.push(CardState {
            id: card,
            generation: 0,
            title: row.title.clone(),
            kind: CardKind::Terminal(TerminalSession {
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                id: TerminalId::from_uuid(row.id.as_uuid()),
                launch: launch.clone(),
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe: launch,
                    last_exit: None,
                },
            }),
            existence: Existence::Open,
            exposure: Exposure::Offscreen,
            residency: Residency::Hibernated,
            width_fraction: 1.0,
        });
        durable.channels.push(ProjectChannel {
            title_source: Default::default(),
            id: row.id,
            project_id: project_id(1),
            title: row.title.clone(),
            ordinal: row.ordinal,
            shared: false,
            cards: vec![card],
            focused: Some(card),
        });
    }
    app.sidebar_state.sync_active_worktree(
        &app.sidebar_model,
        &SidebarFocus::Repository {
            worktree: TARGET.into(),
        },
    );
    app.sidebar_state.set_agent_activity(
        CardId::from_uuid(uuid::Uuid::from_u128(40)),
        project_id(1),
        worktree_id(3),
        Activity {
            provider: crate::agent::Provider::Codex,
            state: ActivityState::Working,
        },
    );
    app.lifecycle = LifecycleManager::new(LifecycleBudget::default());
    app.status = Some(TransientNotice::warning(
        "Offline sidebar fixture: renderer and local interaction only. F8 inserts unrelated rows; F9 shows an empty project. No deletion, retry, provider, or saved workspace is connected.",
    ));
    (app, Task::none())
}

fn insert_neighbors(app: &mut Cagents) {
    if app.sidebar_model.project(project_id(99)).is_some() {
        return;
    }
    let neighbor = SidebarWorktree::new(
        worktree_id(90),
        "/offline-sidebar-fixture/inserted-neighbor".into(),
        Some("fix/sidebar-identifying-title-inserted".into()),
        false,
        false,
        None,
        None,
        None,
    );
    let Some(original) = app
        .sidebar_model
        .projects
        .iter_mut()
        .find(|project| project.id == project_id(1))
    else {
        return;
    };
    original.worktrees.insert(1, neighbor);
    app.sidebar_state.set_agent_activity(
        CardId::from_uuid(uuid::Uuid::from_u128(91)),
        project_id(1),
        worktree_id(90),
        Activity {
            provider: crate::agent::Provider::Codex,
            state: ActivityState::Waiting,
        },
    );
    app.sidebar_model.projects.insert(
        0,
        SidebarProject::new(
            project_id(99),
            RepositoryId(uuid::Uuid::from_u128(199)),
            "Unrelated inserted project".into(),
            "/offline-sidebar-fixture/inserted-project".into(),
            Vec::new(),
            Vec::new(),
        ),
    );
    // Do not replace SidebarState, NavigationCache, widget IDs, or popup state.
}

fn sidebar_action(app: &mut Cagents, action: SidebarAction) -> Task<Message> {
    if app.modal_is_open() {
        return Task::none();
    }
    match action {
        SidebarAction::ProjectCommand {
            project_id,
            worktree_id,
            command: sidebar::ProjectCommand::Delete,
        } => app.handle_project_action(project_ui::Action::Command {
            project_id,
            worktree_id,
            command: sidebar::ProjectCommand::Delete,
        }),
        SidebarAction::DeleteRow(row @ (SelectionRow::Worktree(..) | SelectionRow::Channel(_))) => {
            app.delete_sidebar_row(row)
        }
        SidebarAction::DeleteChannel { channel_id: id } => {
            app.handle_channel_ui(channel_ui::Action::Delete(id))
        }
        SidebarAction::ToggleProjects => {
            app.chrome_ui.projects_collapsed = !app.chrome_ui.projects_collapsed;
            Task::none()
        }
        SidebarAction::Resize { width } => {
            app.settings.sidebar_width = sidebar::clamp_expanded_width(width);
            Task::none()
        }
        SidebarAction::ScrollProjects(y) => iced::widget::operation::scroll_by(
            widget::Id::new("sidebar.projects.scroll"),
            iced::widget::operation::AbsoluteOffset { x: 0.0, y },
        )
        .map(cosmic::Action::App),
        SidebarAction::NavigateRow { row, key } => {
            use keyboard::key::Named;
            if key == Named::Enter {
                return app
                    .sidebar_navigation_action(row)
                    .map_or_else(Task::none, |action| sidebar_action(app, action));
            }
            if let SelectionRow::Project(project) = row
                && matches!(key, Named::ArrowLeft | Named::ArrowRight)
            {
                if app.sidebar_state.project_expanded(project) == (key == Named::ArrowLeft) {
                    app.sidebar_state
                        .apply(&SidebarAction::ToggleProject(project));
                }
                return Task::none();
            }
            app.sidebar_keyboard_neighbor(row, key)
                .map_or_else(Task::none, |row| {
                    sidebar::focus_and_reveal(sidebar::selection_row_focus_id(row))
                        .map(cosmic::Action::App)
                })
        }
        SidebarAction::SelectRows { row, intent } => {
            app.select_sidebar_rows(row, intent);
            if matches!(
                intent,
                SelectionIntent::Toggle | SelectionIntent::Range { .. }
            ) {
                sidebar::focus_and_reveal(sidebar::selection_row_focus_id(row))
                    .map(cosmic::Action::App)
            } else {
                Task::none()
            }
        }
        SidebarAction::OpenWorktree {
            project_id,
            worktree_id,
            path,
            ..
        } => {
            app.sidebar_state.sync_active_worktree(
                &app.sidebar_model,
                &SidebarFocus::Repository { worktree: path },
            );
            app.select_sidebar_rows(
                SelectionRow::Worktree(project_id, worktree_id),
                SelectionIntent::Navigate,
            );
            Task::none()
        }
        SidebarAction::OpenChannel { channel_id } => {
            Arc::make_mut(&mut app.durable).active_channel = Some(channel_id);
            for project in &mut app.sidebar_model.projects {
                for row in &mut project.channels {
                    row.selected = row.id == channel_id;
                }
            }
            app.select_sidebar_rows(SelectionRow::Channel(channel_id), SelectionIntent::Navigate);
            Task::none()
        }
        action @ (SidebarAction::ToggleProject(_)
        | SidebarAction::ToggleControls
        | SidebarAction::SearchChanged(_)
        | SidebarAction::SetFilter(_)) => {
            app.sidebar_state.apply(&action);
            Task::none()
        }
        SidebarAction::ClearRowSelection => {
            app.sidebar_state.clear_row_selection();
            Task::none()
        }
        _ => Task::none(), // No external editors, Git, links, accounts, registry, or lifecycle actions.
    }
}

pub(super) fn update(app: &mut Cagents, message: Message) -> Task<Message> {
    match message {
        Message::Sidebar(action) => sidebar_action(app, action),
        Message::ProjectUi(project_ui::Action::Dismiss) => {
            app.handle_project_action(project_ui::Action::Dismiss)
        }
        Message::ChannelUi(channel_ui::Action::Dismiss) => {
            app.handle_channel_ui(channel_ui::Action::Dismiss)
        }
        Message::KeyEvent { event, captured } => {
            if let keyboard::Event::ModifiersChanged(modifiers) = &event {
                app.keyboard_modifiers = *modifiers;
            }
            if !app.modal_is_open()
                && let keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(key),
                    repeat: false,
                    ..
                } = &event
            {
                if *key == keyboard::key::Named::F8 {
                    insert_neighbors(app);
                }
                if *key == keyboard::key::Named::F9 {
                    app.sidebar_model.projects = vec![SidebarProject::new(
                        project_id(100),
                        RepositoryId(uuid::Uuid::from_u128(200)),
                        "Empty synthetic project".into(),
                        "/offline-sidebar-fixture/empty".into(),
                        Vec::new(),
                        Vec::new(),
                    )];
                    app.sidebar_state.clear_row_selection();
                    app.sidebar_state
                        .sync_active_worktree(&app.sidebar_model, &SidebarFocus::None);
                }
            }
            if !captured && is_escape_press(&event) {
                if app.project_ui.modal_open() {
                    return app.handle_project_action(project_ui::Action::Dismiss);
                }
                if app.channel_ui.modal_open() {
                    return app.handle_channel_ui(channel_ui::Action::Dismiss);
                }
            }
            Task::none()
        }
        Message::WindowResized(width, height) => {
            if width.is_finite() && width > 0.0 {
                app.window_width = width;
            }
            if height.is_finite() && height > 0.0 {
                app.window_height = height;
            }
            Task::none()
        }
        Message::WindowScaleFactorChanged(scale) if scale.is_finite() && scale > 0.0 => {
            app.window_scale_factor = scale;
            Task::none()
        }
        Message::WindowCloseRequested(id) => iced::window::close(id)
            .chain(iced::exit())
            .map(cosmic::Action::App),
        Message::WindowClosed(_) => iced::exit().map(cosmic::Action::App),
        _ => Task::none(), // Confirmations never execute deletions; unrelated callbacks cannot start work.
    }
}

pub(super) fn subscription(_app: &Cagents) -> Subscription<Message> {
    event::listen_with(|event, status, event_window| match event {
        iced::Event::Keyboard(event) => Some(Message::KeyEvent {
            event,
            captured: matches!(status, event::Status::Captured),
        }),
        iced::Event::Window(window::Event::CloseRequested) => {
            Some(Message::WindowCloseRequested(event_window))
        }
        iced::Event::Window(window::Event::Closed) => Some(Message::WindowClosed(event_window)),
        iced::Event::Window(window::Event::Rescaled(scale)) => {
            Some(Message::WindowScaleFactorChanged(scale))
        }
        iced::Event::Window(event) => window_geometry_message(&event),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_fixture_is_exact_opt_in_and_has_no_store_or_executable_editors() {
        for value in [None, Some("1"), Some("sidebar-row-2004-extra")] {
            assert!(!selected(value));
        }
        assert!(selected(Some(MODE)) && selected(Some(COMPACT)));
        let bootstrap = bootstrap();
        assert!(bootstrap.session.is_none() && bootstrap.settings_path.is_none());
        assert!(bootstrap.settings_write_blocked && bootstrap.workspace.cards.is_empty());
        let data = model();
        assert_eq!(data.external_editors.len(), 6);
        let original = &data.projects[0];
        assert_eq!(original.worktrees[0].path, original.repository);
        assert_eq!(original.worktrees[1].id, worktree_id(3));
        assert_eq!(original.worktrees[1].path, PathBuf::from(TARGET));
        assert_eq!(
            original.worktrees[1].pull_request_state,
            Some(sidebar::WorktreePullRequestState::Merged)
        );
        assert_eq!(original.worktrees[2].pull_request_state, None);
    }

    #[test]
    fn inserted_neighbors_keep_the_original_guarded_confirmation_and_cancel_target() {
        let (mut app, _) = install(super::super::tests::context_inspector_test_app(false));
        let original = app.sidebar_model.projects[0].worktrees[1].clone();
        insert_neighbors(&mut app);
        insert_neighbors(&mut app);
        assert_eq!(app.sidebar_model.projects.len(), 2);
        let project = app.sidebar_model.project(project_id(1)).unwrap();
        assert_eq!(project.worktrees.len(), 4);
        assert_eq!(project.worktrees[1].id, worktree_id(90));
        assert_eq!(project.worktrees[2], original);
        drop(update(
            &mut app,
            Message::Sidebar(SidebarAction::ProjectCommand {
                project_id: project_id(1),
                worktree_id: Some(worktree_id(3)),
                command: sidebar::ProjectCommand::Delete,
            }),
        ));
        assert!(app.project_ui.modal_open() && app.session.is_none());
        drop(update(
            &mut app,
            Message::ProjectUi(project_ui::Action::ConfirmDelete),
        ));
        assert!(
            app.project_ui.modal_open(),
            "the inert fixture cannot run deletion"
        );
        assert!(!app.project_ui.deleting_directory(Path::new(TARGET)));
        drop(update(
            &mut app,
            Message::ProjectUi(project_ui::Action::Dismiss),
        ));
        assert!(!app.project_ui.modal_open());
        assert_eq!(
            app.sidebar_model.project(project_id(1)).unwrap().worktrees[2],
            original
        );
    }
}
