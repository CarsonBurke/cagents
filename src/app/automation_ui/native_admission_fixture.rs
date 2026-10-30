//! Inert public-data scenes mounted in the real Automations utility card.
//! No fixture request may reach ControlClient. These receipts are synthetic;
//! they establish neither daemon acceptance nor provider/session authority.
use super::super::*;
use crate::agent_execution::{Agent, HostedSession, Origin, Spec, Surface, Target};
use crate::automation_ui::Action;
use crate::automations::{
    Automation, Definition, DirectoryIdentity, Firing, FiringState, HistoryCursor, HistoryPage,
    LifecyclePage, Request, Response, RunDetail, Schedule, SchedulePattern, SessionMode, Trigger,
};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use uuid::Uuid;

const SELECTOR: &str = "CAGENTS_DEVELOPMENT_AUTOMATION_FIXTURE";
const SCENES: &[&str] = &[
    "retry",
    "active",
    "open-agent",
    "closed-agent",
    "refresh",
    "paged",
    "read-error",
    "empty",
];
const A: Uuid = Uuid::from_u128(177_501);
const F: Uuid = Uuid::from_u128(177_511);
const G: Uuid = Uuid::from_u128(177_512);
const AGENT: Uuid = Uuid::from_u128(177_520);
const PROJECT: Uuid = Uuid::from_u128(177_530);
const ORIGINAL: Uuid = Uuid::from_u128(177_531);
const DISPLAY: Uuid = Uuid::from_u128(177_532);

pub(crate) struct Context {
    root: PathBuf,
    scene: String,
}

/// Read-only recognition, also called by the binary BEFORE device preparation.
/// The caller owns display/clipboard isolation and proves initially empty roots;
/// WindowLease may already have made its own files by the later App recognition.
pub(crate) fn context() -> Result<Option<Context>, String> {
    let Some(scene) = std::env::var_os(SELECTOR) else {
        return Ok(None);
    };
    let scene = scene
        .into_string()
        .map_err(|_| "invalid native fixture selector")?;
    if !SCENES.contains(&scene.as_str()) {
        return Err("unknown inert automation fixture scene".into());
    }
    if std::env::var_os("CAGENTS_LATENCY_OUTPUT").is_some() {
        return Err("inert automation fixture cannot share a latency fixture".into());
    }
    let names = [
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
    ];
    let mut roots = Vec::new();
    for name in names {
        let path = PathBuf::from(
            std::env::var_os(name).ok_or("fixture needs all five private XDG roots")?,
        );
        roots.push(path);
    }
    let root = roots[0]
        .parent()
        .ok_or("missing fixture namespace")?
        .to_path_buf();
    // Admit the entire lexical scope before ANY filesystem access. In
    // particular, an absolute path containing `..` is not a direct child.
    if roots.iter().any(|path| {
        !path.is_absolute()
            || path.components().count() != 4
            || path.components().any(|component| {
                !matches!(
                    component,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
    }) || root.parent() != Some(Path::new("/tmp"))
        || !root
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("cagents-1775-native-"))
        || roots
            .iter()
            .any(|path| path.parent() != Some(root.as_path()))
        || roots.iter().collect::<HashSet<_>>().len() != names.len()
    {
        return Err("fixture XDG roots must be distinct children of one fresh /tmp/cagents-1775-native-* namespace".into());
    }
    private_directory(&root)?;
    for path in &roots {
        private_directory(path)?;
    }
    Ok(Some(Context { root, scene }))
}

fn private_directory(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("fixture directory must be absolute".into());
    }
    let metadata =
        std::fs::symlink_metadata(path).map_err(|e| format!("fixture directory: {e}"))?;
    // These are explicit fixture paths, never HOME, account paths or credentials.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o700
        || path.canonicalize().map_err(|e| e.to_string())? != path
    {
        return Err("fixture directory must be canonical, owned and mode 0700".into());
    }
    Ok(())
}

pub(crate) fn bootstrap(context: &Context) -> Result<Bootstrap, String> {
    use std::io::Write;
    let mut marker = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(context.root.join("automation-native-started"))
        .map_err(|e| format!("fixture namespace was already used or cannot be reserved: {e}"))?;
    writeln!(marker, "INERT automation native fixture: {}", context.scene)
        .map_err(|e| e.to_string())?;
    let directory = context.root.join("public-project");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .map_err(|e| e.to_string())?;
    let project = ProjectId::from_uuid(PROJECT);
    let mut workspace = WorkspaceState::default();
    workspace.active_channel = Some(ChannelId::from_uuid(DISPLAY));
    workspace.channels = vec![
        channel(ORIGINAL, project, "Original execution", 0),
        channel(DISPLAY, project, "INERT fixture · no backend", 1),
    ];
    let mut settings = AppSettings::default();
    settings.app_theme = AppTheme::Dark;
    Ok(Bootstrap {
        workspace,
        session: None,
        projects: vec![ProjectState {
            id: project,
            generation: 0,
            name: "INERT Automations · synthetic data only".into(),
            repository: directory,
            kind: ProjectKind::Folder,
            existence: Existence::Open,
        }],
        worktrees: Vec::new(),
        settings,
        settings_path: None,
        settings_write_blocked: true,
        status: None,
        save_health: None,
        restore_warnings: Vec::new(),
        settings_warnings: Vec::new(),
    })
}

fn channel(
    id: Uuid,
    project_id: ProjectId,
    title: &str,
    ordinal: u32,
) -> crate::model::ProjectChannel {
    crate::model::ProjectChannel {
        id: ChannelId::from_uuid(id),
        project_id,
        title: title.into(),
        title_source: Default::default(),
        ordinal,
        shared: false,
        cards: Vec::new(),
        focused: None,
    }
}

pub(crate) struct State {
    scene: String,
    directory: PathBuf,
    definitions: Vec<Automation>,
    original_definition: Definition,
    manual: Option<Request>,
    reads: u64,
    lists: u64,
    last_sequence: u64,
    cancelled: bool,
    error: Option<String>,
    agent: Option<Agent>,
}

impl State {
    pub(crate) fn new(context: Context) -> Self {
        let directory = context.root.join("public-project");
        let mut definitions = Vec::new();
        if context.scene != "empty" {
            for index in 0..if context.scene == "paged" { 60 } else { 3 } {
                let mut value = definition(&directory, index);
                if context.scene == "active" && index == 0 {
                    value.active_firing = Some(F);
                }
                definitions.push(value);
            }
        }
        Self {
            scene: context.scene,
            directory: directory.clone(),
            definitions,
            original_definition: definition(&directory, 0).definition,
            manual: None,
            reads: 0,
            lists: 0,
            last_sequence: 0,
            cancelled: false,
            error: None,
            agent: None,
        }
    }

    fn fail(&mut self, reason: &str) -> String {
        let error = self
            .error
            .get_or_insert_with(|| format!("INERT fixture stopped: {reason}"))
            .clone();
        receipt(serde_json::json!({"event":"fixture_error", "scene":self.scene, "error":error}));
        error
    }

    fn firing(&self, id: Uuid, key: Uuid, created_ms: u64) -> Firing {
        let active = self.scene == "active" && id == F && !self.cancelled;
        Firing { id, automation: A, name: "INERT · Daily public review".into(), revision: 3,
            trigger: Trigger::Manual { request: key }, state: if active { FiringState::Running } else if self.cancelled && id == F { FiringState::Cancelled } else { FiringState::Succeeded },
            release_pending: false, cancel_requested: self.cancelled && id == F,
            created_ms, started_ms: Some(created_ms), finished_ms: (!active).then_some(created_ms + 1),
            agent: self.agent.as_ref().map(|a| a.id), run: None, skip: None,
            diagnostic: Some("INERT native fixture receipt. No provider, daemon request or automation job was executed. This long public diagnostic exists to exercise wrapping and scrolling at the narrow viewport.".repeat(if self.scene == "paged" { 5 } else { 1 })),
        }
    }

    fn history(&self) -> Vec<Firing> {
        if self.scene == "empty" {
            return Vec::new();
        }
        let key = match &self.manual {
            Some(Request::RunNow { request, .. }) => *request,
            _ => Uuid::from_u128(177_540),
        };
        if self.scene == "retry" && self.manual.is_none() {
            return Vec::new();
        }
        let mut rows = vec![
            self.firing(G, Uuid::from_u128(177_541), 200),
            self.firing(F, key, 100),
        ];
        if self.scene == "paged" {
            rows.extend((0..58).map(|i| {
                self.firing(
                    Uuid::from_u128(177_600 + i),
                    Uuid::from_u128(177_800 + i),
                    98 - i as u64,
                )
            }));
        }
        rows
    }

    fn respond(&mut self, request: &Request) -> Result<Response, String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        match request {
            Request::List { after, limit } if *limit > 0 && *limit <= 100 => {
                self.lists += 1;
                if self.scene == "read-error" && self.lists == 1 {
                    return Err("INERT simulated list read failure. Use Refresh; no real daemon was contacted.".into());
                }
                Ok(Response::Listed(
                    self.definitions
                        .iter()
                        .filter(|d| after.is_none_or(|id| d.id > id))
                        .take(*limit)
                        .map(|d| d.summary(None))
                        .collect(),
                ))
            }
            Request::Show { id } => {
                self.reads += 1;
                let Some(value) = self.definitions.iter_mut().find(|d| d.id == *id) else {
                    return Err(self.fail("unknown definition"));
                };
                if self.scene == "refresh" && *id == A {
                    value.updated_ms += 1;
                    if self.reads == 3 {
                        value.revision += 1;
                        value.definition.name = "INERT · Revised public review".into();
                    }
                }
                Ok(Response::Automation(value.clone()))
            }
            Request::History { id, before, limit }
                if id.is_none_or(|id| self.definitions.iter().any(|d| d.id == id))
                    && *limit > 0
                    && *limit <= 100 =>
            {
                let rows: Vec<_> = self
                    .history()
                    .into_iter()
                    .filter(|f| id.is_none_or(|owner| f.automation == owner))
                    .filter(|f| {
                        before.as_ref().is_none_or(|cursor| {
                            (f.created_ms, f.id) < (cursor.created_ms, cursor.id)
                        })
                    })
                    .collect();
                let firings: Vec<_> = rows.iter().take(*limit).cloned().collect();
                let next = (rows.len() > *limit).then(|| {
                    let f = firings.last().expect("positive page size");
                    HistoryCursor {
                        created_ms: f.created_ms,
                        id: f.id,
                    }
                });
                Ok(Response::History(HistoryPage { firings, next }))
            }
            Request::RunNow {
                id,
                execution_revision,
                confirmed: true,
                ..
            } if self.scene == "retry" && *id == A && *execution_revision == 3 => {
                if let Some(original) = &self.manual {
                    if original != request {
                        return Err(self.fail("retry changed the captured manual request"));
                    }
                    let Request::RunNow { request: key, .. } = request else {
                        unreachable!()
                    };
                    Ok(Response::Firing(self.firing(F, *key, 100)))
                } else {
                    self.manual = Some(request.clone());
                    Err("INERT simulated lost reply after RunNow. The next public poll reports synthetic settled F and newer G. No request was sent to a daemon.".into())
                }
            }
            Request::SetEnabled {
                id,
                revision,
                enabled,
                confirmed,
            } => {
                let Some(value) = self
                    .definitions
                    .iter_mut()
                    .find(|d| d.id == *id && d.revision == *revision)
                else {
                    return Err(self.fail("wrong SetEnabled subject/revision"));
                };
                if *enabled && !confirmed {
                    return Err(self.fail("unconfirmed enable"));
                }
                value.enabled = *enabled;
                value.revision += 1;
                value.updated_ms += 1;
                Ok(Response::Automation(value.clone()))
            }
            Request::Remove { id, revision }
                if self.definitions.iter().any(|d| {
                    d.id == *id && d.revision == *revision && d.active_firing.is_none()
                }) =>
            {
                self.definitions.retain(|d| d.id != *id);
                Ok(Response::Removed)
            }
            Request::Cancel { id, firing }
                if self.scene == "active" && *id == A && *firing == F && !self.cancelled =>
            {
                self.cancelled = true;
                self.definitions[0].active_firing = None;
                Ok(Response::CancellationRequested)
            }
            Request::ShowFiring { id } if self.history().iter().any(|f| f.id == *id) => {
                let firing = self
                    .history()
                    .into_iter()
                    .find(|f| f.id == *id)
                    .expect("checked firing");
                Ok(Response::Detail(RunDetail {
                    definition: self.original_definition.clone(),
                    firing,
                    agent: self.agent.clone(),
                    run: None,
                }))
            }
            Request::Audit { id, .. } if self.definitions.iter().any(|d| d.id == *id) => {
                Ok(Response::Audit(LifecyclePage {
                    events: Vec::new(),
                    next: None,
                    earlier_events_unavailable: true,
                }))
            }
            _ => Err(self.fail("request outside fixed inert scene; transport refused")),
        }
    }
}

fn definition(directory: &Path, index: u128) -> Automation {
    Automation {
        id: if index == 0 {
            A
        } else {
            Uuid::from_u128(177_501 + index)
        },
        revision: 4,
        execution_revision: 3,
        definition: Definition {
            name: match index {
                0 => "INERT · Daily public review".into(),
                1 => "INERT · Disabled public review".into(),
                _ => format!("INERT · Public review {:02}", index + 1),
            },
            prompt: "Review the public fixture changes. This is display data; never execute it."
                .into(),
            agent: Spec {
                provider: crate::agent::Provider::Claude,
                account: None,
                target: Target {
                    repository: directory.into(),
                    directory: directory.into(),
                    placement: Some(crate::agent_execution::Placement::Channel(
                        ChannelId::from_uuid(ORIGINAL),
                    )),
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            schedule: Schedule {
                pattern: SchedulePattern::Daily { hour: 9, minute: 0 },
                timezone: "UTC".into(),
            },
            session_mode: SessionMode::Fresh,
            precheck: None,
        },
        target_identity: DirectoryIdentity {
            device: 1,
            inode: 2,
        },
        enabled: index == 0,
        created_ms: 1,
        updated_ms: 2,
        next: None,
        active_firing: None,
        reuse_agent: None,
        channel: None,
        blocked: (index == 2)
            .then(|| "INERT · launch unavailable (synthetic blocked state)".into()),
    }
}

fn receipt(value: serde_json::Value) {
    println!("AUTOMATION_NATIVE {value}");
}

pub(crate) fn observe(app: &mut Cagents, action: &Action) {
    let Some(state) = app.automation_native_fixture.as_mut() else {
        return;
    };
    match action {
        Action::Confirm(generation) => receipt(
            serde_json::json!({"event":"actual_confirm_callback","scene":state.scene,"generation":generation}),
        ),
        Action::CancelConfirmation => receipt(
            serde_json::json!({"event":"actual_cancel_confirmation_callback","scene":state.scene}),
        ),
        Action::Received { sequence, result } => receipt(
            serde_json::json!({"event":"synthetic_result_delivered","scene":state.scene,"sequence":sequence,"ok":result.is_ok()}),
        ),
        _ => {}
    }
}

pub(crate) fn request(app: &mut Cagents, sequence: u64, request: Request) -> Task<Message> {
    let state = app
        .automation_native_fixture
        .as_mut()
        .expect("fixture request interception");
    let result = if sequence <= state.last_sequence || sequence > 4000 {
        Err(state.fail("duplicate/nonmonotonic or excessive request sequence"))
    } else {
        state.last_sequence = sequence;
        receipt(
            serde_json::json!({"event":"actual_reducer_request_intercepted","scene":state.scene,"sequence":sequence,"request":request,"backend_executed":false}),
        );
        state.respond(&request)
    };
    receipt(
        serde_json::json!({"event":"synthetic_result_prepared","scene":state.scene,"sequence":sequence,"result":result,"backend_executed":false}),
    );
    // Fixed synthetic service latency makes the real reducer's pending state
    // observable; it is not a daemon/provider responsiveness measurement.
    cosmic::task::future(async move {
        tokio::time::sleep(Duration::from_millis(180)).await;
        Message::AutomationUi(Action::Received {
            sequence,
            result: Arc::new(result),
        })
    })
}

pub(crate) fn install(app: &mut Cagents, context: Context) -> Task<Message> {
    let state = State::new(context);
    app.active_workspace = Some(state.directory.clone());
    app.automation_native_fixture = Some(state);
    app.refresh_ui.focused = true;
    app.context_inspector_open = false;
    if matches!(
        app.automation_native_fixture
            .as_ref()
            .unwrap()
            .scene
            .as_str(),
        "open-agent" | "closed-agent"
    ) {
        mount_inert_agent(app);
    }
    let label = "INERT Automations fixture · synthetic receipts · no daemon/provider";
    app.notices.record(TransientNotice::warning(label));
    // The same normal App utility-card route and reducer/view used in production.
    let open = app.open_automations();
    if let Some(card) = app.workspace.focused()
        && let Some(state) = app.card_state_mut(card)
    {
        state.title = label.into();
    }
    app.refresh_sidebar_channels();
    app.synchronize_sidebar_selection();
    let sidebar = sidebar::effective_width(app.projects_sidebar_mode(), app.settings.sidebar_width);
    let _ = app
        .workspace
        .resize_viewport((app.content_width() - sidebar).max(0.0));
    receipt(
        serde_json::json!({"event":"installed","scene":app.automation_native_fixture.as_ref().unwrap().scene,"backend_executed":false}),
    );
    let theme = app.settings.app_theme.resolve(app.core.system_theme());
    Task::batch([open, app.sync_chrome(), cosmic::command::set_theme(theme)])
}

fn mount_inert_agent(app: &mut Cagents) {
    let state = app.automation_native_fixture.as_ref().unwrap();
    let closed = state.scene == "closed-agent";
    let directory = state.directory.clone();
    let mut session = crate::chat::Session {
        id: TerminalId::from_uuid(Uuid::from_u128(177_521)),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            session_id: None,
            recipe: LaunchRecipe {
                program: "INERT-fixture-never-execute".into(),
                arguments: Vec::new(),
                working_directory: directory.clone(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
        },
        proof: None,
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let card = CardId::from_uuid(AGENT);
    let saved = CardState {
        id: card,
        generation: 0,
        title: "INERT exact agent pane · Draft runtime only".into(),
        width_fraction: 0.5,
        existence: Existence::Open,
        exposure: Exposure::Visible,
        residency: Residency::Hot,
        kind: CardKind::Chat(session.clone()),
    };
    Arc::make_mut(&mut app.durable).cards.push(saved.clone());
    Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|c| c.id == ChannelId::from_uuid(DISPLAY))
        .unwrap()
        .cards
        .push(card);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    // Public draft construction synchronously installs Connection::Draft. Its
    // lazy skill-loading task is deliberately DROPPED, never polled.
    assert!(session.is_unstarted());
    drop(app.open_chat(card, session.clone(), crate::chat::Intent::Start));
    // This is explicit SYNTHETIC display metadata, not a live host receipt. The
    // mounted runtime remains Draft, and all ChatUi messages are refused below.
    session.proof = Some(HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("INERT-host"),
        boot_id: crate::model::BootId::new("INERT-boot"),
        host_incarnation: crate::model::HostIncarnation::new("INERT-incarnation"),
    });
    if let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) {
        runtime.session = session.clone();
    }
    let saved = {
        let saved = app.card_state_mut(card).expect("exact inert card");
        saved.kind = CardKind::Chat(session.clone());
        if closed {
            saved.existence = Existence::Closed {
                tombstone: crate::model::Tombstone {
                    operation_id: None,
                    closed_at_unix_ms: 0,
                    reason: Some("INERT seeded local close".into()),
                },
            };
        }
        saved.clone()
    };
    let agent = Agent {
        name: Some("INERT public automation agent".into()),
        id: crate::model::AgentId::from_uuid(AGENT),
        origin: Origin::Automation { id: A },
        spec: app.automation_native_fixture.as_ref().unwrap().definitions[0]
            .definition
            .agent
            .clone(),
        placement: crate::agent_execution::Placement::Channel(ChannelId::from_uuid(ORIGINAL)),
        session: Some(HostedSession::Chat(session)),
        console: None,
        created_ms: 0,
        closed: false,
    };
    let mut model = crate::workspace_model::WorkspaceModel::default();
    model.identity = Uuid::from_u128(177_550);
    model.sequence = 1;
    let pane = crate::workspace_model::views::Pane {
        id: card,
        title: saved.title.clone(),
        user_title: Some(saved.title.clone()),
        pending_terminal_start: false,
        width_fraction: 0.5,
        kind: saved.kind.clone(),
        scope: Some(directory),
    };
    model.views.push(crate::workspace_model::views::View {
        scope: crate::agent_execution::Placement::Channel(ChannelId::from_uuid(DISPLAY)),
        revision: 2,
        panes: vec![pane],
        retired: Vec::new(),
    });
    model.views.push(crate::workspace_model::views::View {
        scope: agent.placement.clone(),
        revision: 2,
        panes: Vec::new(),
        retired: vec![card],
    });
    model.agents.push(agent.clone());
    app.workspace_model = model;
    app.automation_native_fixture.as_mut().unwrap().agent = Some(agent);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
}

/// Only these App messages/tasks are admitted in an inert scene. Toolkit button
/// keyboard/mouse handling and the real reducer/view are unchanged. Global App
/// shortcuts and operational chat/session lifecycle are deliberately unexercised.
pub(crate) fn update(app: &mut Cagents, message: Message) -> Task<Message> {
    let message = match message {
        Message::UtilityPaneUi(message) => *message,
        message => message,
    };
    let task = match message {
        Message::AutomationUi(action) => app.update_automation_ui(action),
        Message::Sidebar(SidebarAction::OpenAutomations) => app.open_automations(),
        Message::ChromeUi(
            action @ (chrome_ui::Action::ToggleNotices
            | chrome_ui::Action::ShowNoticesFor(_)
            | chrome_ui::Action::Close
            | chrome_ui::Action::HoverTab(_)
            | chrome_ui::Action::TabsScrolled(_)),
        ) => app.update_chrome(action),
        Message::FocusCard(card) if app.card_state(card).is_some_and(|c| c.existence.is_open()) => {
            app.focus_workspace_card(card)
        }
        Message::Viewport { generation, event } if generation == app.workspace_view_generation => {
            if matches!(event, WorkspaceViewportEvent::Reordered { .. })
                || (app.workspace_inputs_blocked()
                    && matches!(
                        event,
                        WorkspaceViewportEvent::DragScrolled(_)
                            | WorkspaceViewportEvent::PointerFocused(_)
                            | WorkspaceViewportEvent::PointerRested(_)
                    ))
                || (matches!(event, WorkspaceViewportEvent::PointerRested(_))
                    && !app.settings.focus_follows_mouse)
            {
                Task::none()
            } else {
                let (update, synchronize) = update_workspace_viewport(&mut app.workspace, event);
                app.apply_viewport(update, synchronize, false)
            }
        }
        Message::Frame(at) => match app.frame_clock.elapsed(at) {
            Some(elapsed) => {
                let update = app.workspace.tick(elapsed);
                app.apply_viewport(update, true, false)
            }
            None => Task::none(),
        },
        Message::WindowResized(width, height) => {
            if width.is_finite() && width > 0.0 {
                app.window_width = width;
            }
            if height.is_finite() && height > 0.0 {
                app.window_height = height;
            }
            app.content_width_changed()
        }
        Message::WindowScaleFactorChanged(scale) => {
            if scale.is_finite() && scale > 0.0 {
                app.window_scale_factor = scale;
            }
            Task::none()
        }
        Message::WindowEdgeState {
            window,
            filled,
            right_edge_tiled,
        } if app.core.main_window_id() == Some(window) => {
            app.window_state_observed = true;
            app.window_filled = filled;
            app.right_edge_tiled = right_edge_tiled;
            app.content_width_changed()
        }
        Message::WindowFilled(window, filled) if app.core.main_window_id() == Some(window) => {
            app.window_filled = filled;
            app.content_width_changed()
        }
        Message::RefreshUi(refresh_ui::Action::Focused(window, focused))
            if app.core.main_window_id() == Some(window) =>
        {
            app.refresh_ui.focused = focused;
            Task::none()
        }
        Message::KeyEvent { event, .. } => {
            if let Some(task) = app.chrome_key_event(&event) {
                return task;
            }
            if let keyboard::Event::ModifiersChanged(modifiers) = event {
                app.keyboard_modifiers = modifiers;
            }
            Task::none()
        }
        Message::WindowCloseRequested(window) if app.core.main_window_id() == Some(window) => {
            iced::window::close(window).map(cosmic::Action::App)
        }
        Message::WindowClosed(window) if app.core.main_window_id() == Some(window) => iced::exit(),
        Message::DismissNotice(id) => {
            app.notices.dismiss(id);
            Task::none()
        }
        Message::DismissStatus | Message::DismissAllNotices => {
            app.status = None;
            app.notices.clear();
            Task::none()
        }
        // Explicitly dropped continuations from the presentation helpers above.
        // No subscriptions create operational completions, no fallback exists.
        _ => Task::none(),
    };
    if let Some(notice) = app.status.take() {
        app.notices.record(notice);
    }
    Task::batch([task, app.sync_chrome()])
}

pub(crate) fn subscription(app: &Cagents) -> Subscription<Message> {
    let mut subscriptions = vec![
        app.automation_subscription(),
        event::listen_with(|event, status, window| match event {
            iced::Event::Keyboard(event) => Some(Message::KeyEvent {
                event,
                captured: matches!(status, event::Status::Captured),
            }),
            iced::Event::Window(window::Event::CloseRequested) => {
                Some(Message::WindowCloseRequested(window))
            }
            iced::Event::Window(window::Event::Closed) => Some(Message::WindowClosed(window)),
            iced::Event::Window(window::Event::Focused) => Some(Message::RefreshUi(
                refresh_ui::Action::Focused(window, true),
            )),
            iced::Event::Window(window::Event::Unfocused) => Some(Message::RefreshUi(
                refresh_ui::Action::Focused(window, false),
            )),
            iced::Event::Window(window::Event::Rescaled(scale)) => {
                Some(Message::WindowScaleFactorChanged(scale))
            }
            iced::Event::Window(event) => window_geometry_message(&event),
            _ => None,
        }),
    ];
    if let Some(window) = app.core.main_window_id()
        && app.workspace.is_animating()
    {
        subscriptions.push(workspace_widget::animation_frames(window, Message::Frame));
    }
    Subscription::batch(subscriptions)
}
