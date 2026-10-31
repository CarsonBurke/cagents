use super::*;
use crate::{
    chat::{Launch, Session},
    terminal_ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
};
use std::{
    os::unix::{fs::PermissionsExt, net::UnixListener},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// The real startup Tasks call the public chat client. Keep their daemon and
/// every application path in a separate process, away from the user's services
/// and tests running concurrently in this binary.
fn isolated(name: &str) -> bool {
    assert!(
        std::env::var_os("CAGENTS_AGENT_AUTHORITY").is_none(),
        "These private startup fixtures require a genuine User invocation; inherited caller authority must not be cleared"
    );
    const CHILD: &str = "CAGENTS_STARTUP_ADMISSION_TEST";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return false;
    }
    let root = tempfile::tempdir().unwrap();
    for leaf in ["home", "data", "config", "state", "cache", "run", "tmp"] {
        std::fs::create_dir(root.path().join(leaf)).unwrap();
        std::fs::set_permissions(
            root.path().join(leaf),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("app::shared_views::tests::startup_admission::{name}"),
            "--nocapture",
        ])
        .env_clear()
        .env(CHILD, name)
        .env("HOME", root.path().join("home"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_RUNTIME_DIR", root.path().join("run"))
        .env("TMPDIR", root.path().join("tmp"))
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated startup failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    true
}

#[derive(Clone)]
struct UpgradeScript {
    source: Session,
    canonical: Session,
    refuse_lineage: bool,
}

fn scripted_proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: "live-host".into(),
        boot_id: "boot".into(),
        host_incarnation: "live-incarnation".into(),
    }
}

fn current_owner(pane: &mut Pane) -> Session {
    let CardKind::Chat(session) = &mut pane.kind else {
        unreachable!()
    };
    session.proof = Some(scripted_proof());
    session.clone()
}

struct ScriptedDaemon {
    providers: Arc<Mutex<Vec<Session>>>,
    resolutions: Arc<Mutex<Vec<Session>>>,
    controls: Arc<Mutex<Vec<ClientRequest>>>,
    inventory: Arc<Mutex<Vec<Session>>>,
    connections: Arc<Mutex<Vec<std::os::unix::net::UnixStream>>>,
    stopped: Arc<AtomicBool>,
    visibility_cancellations: Arc<AtomicUsize>,
    visibility_canceled: std::sync::mpsc::Receiver<u64>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ScriptedDaemon {
    fn start() -> Self {
        Self::with_upgrade(None)
    }

    fn with_upgrade(upgrade: Option<UpgradeScript>) -> Self {
        Self::with_attachment(upgrade, false)
    }

    fn with_owner(session: Session) -> Self {
        assert_eq!(session.proof, Some(scripted_proof()));
        // Ordinary startup attaches this exact current owner. The dedicated
        // upgrade fixtures below retain old-proof lineage and writer coverage.
        Self::with_upgrade(Some(UpgradeScript {
            source: session.clone(),
            canonical: session,
            refuse_lineage: false,
        }))
    }

    fn with_image_owner(session: Session) -> Self {
        assert_eq!(session.proof, Some(scripted_proof()));
        Self::with_attachment_capabilities(
            Some(UpgradeScript {
                source: session.clone(),
                canonical: session,
                refuse_lineage: false,
            }),
            false,
            true,
        )
    }

    fn with_attachment(upgrade: Option<UpgradeScript>, refuse_attachment: bool) -> Self {
        Self::with_attachment_capabilities(upgrade, refuse_attachment, false)
    }

    fn with_attachment_capabilities(
        upgrade: Option<UpgradeScript>,
        refuse_attachment: bool,
        attachments: bool,
    ) -> Self {
        let socket = crate::terminal_ipc::daemon_socket_path().unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let providers = Arc::new(Mutex::new(Vec::new()));
        let resolutions = Arc::new(Mutex::new(Vec::new()));
        let controls = Arc::new(Mutex::new(Vec::new()));
        let inventory = Arc::new(Mutex::new(
            upgrade
                .as_ref()
                .map(|script| vec![script.canonical.clone()])
                .unwrap_or_default(),
        ));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let visibility_cancellations = Arc::new(AtomicUsize::new(0));
        let (visibility_canceled_tx, visibility_canceled) = std::sync::mpsc::channel();
        let cancellations = visibility_cancellations.clone();
        let recorded = providers.clone();
        let resolved = resolutions.clone();
        let control_requests = controls.clone();
        let listed_inventory = inventory.clone();
        let sockets = connections.clone();
        let stop = stopped.clone();
        let worker = std::thread::spawn(move || {
            let proof = scripted_proof();
            let mut clients = Vec::new();
            while !stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("scripted daemon accept: {error}"),
                };
                // Register cancellation under the same lock used by Drop.
                // A successful attachment stays alive until the test ends.
                let mut held = sockets.lock().unwrap();
                if stop.load(Ordering::Acquire) {
                    break;
                }
                held.push(stream.try_clone().unwrap());
                drop(held);
                let recorded = recorded.clone();
                let resolved = resolved.clone();
                let control_requests = control_requests.clone();
                let listed_inventory = listed_inventory.clone();
                let proof = proof.clone();
                let upgrade = upgrade.clone();
                let stop = stop.clone();
                let cancellations = cancellations.clone();
                let visibility_canceled_tx = visibility_canceled_tx.clone();
                clients.push(std::thread::spawn(move || {
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    loop {
                        let request = match read_frame::<ClientEnvelope>(&mut stream) {
                            Ok(Some(request)) => request,
                            Ok(None) => break,
                            Err(_) if stop.load(Ordering::Acquire) => break,
                            Err(error) => panic!("scripted client read: {error}"),
                        };
                        let replaceable_visibility = matches!(
                            &request.request,
                            ClientRequest::ChatVisibility { chat_id, .. }
                                if upgrade.as_ref().is_some_and(|script| *chat_id == script.canonical.id)
                        );
                        control_requests.lock().unwrap().push(request.request.clone());
                        let message = match request.request {
                            ClientRequest::Connect { .. } => ServerMessage::Response {
                                response: ServerResponse::Connected {
                                    proof: proof.clone(),
                                    resumed: Vec::new(),
                                },
                            },
                            ClientRequest::List => ServerMessage::Response { response: ServerResponse::Listed { terminals: Vec::new() } },
                            ClientRequest::ChatList => ServerMessage::Response { response: ServerResponse::ChatListed { sessions: listed_inventory.lock().unwrap().clone() } },
                            ClientRequest::ChatClose { chat_id } => ServerMessage::Response { response: ServerResponse::ChatClosed { chat_id, snapshot: Default::default() } },
                            ClientRequest::ChatCloseRetained { .. } => ServerMessage::Response { response: ServerResponse::Accepted },
                            ClientRequest::CloseTerminalChecked { terminal_id, .. } => ServerMessage::Response { response: ServerResponse::TerminalCloseChecked { terminal_id, outcome: crate::terminal::ipc::TerminalCloseResult::Closed } },
                            ClientRequest::ResolveUpgradeChat { source } => {
                                resolved.lock().unwrap().push(source.clone());
                                if let Some(script) = &upgrade {
                                    assert_eq!(source, script.source);
                                    if !script.refuse_lineage {
                                        ServerMessage::Response {
                                            response: ServerResponse::UpgradeChatResolved {
                                                session: script.canonical.clone(),
                                            },
                                        }
                                    } else {
                                        ServerMessage::Error(ProtocolError::new(
                                            ProtocolErrorCode::TerminalUnavailable,
                                            "No upgrade lineage in this fixture",
                                            false,
                                        ))
                                    }
                                } else {
                                    ServerMessage::Error(ProtocolError::new(
                                        ProtocolErrorCode::TerminalUnavailable,
                                        "No upgrade lineage in this fixture",
                                        false,
                                    ))
                                }
                            }
                            ClientRequest::ChatStart { session, .. } => {
                                assert!(
                                    upgrade.is_none(),
                                    "automatic restore cannot start a provider"
                                );
                                recorded.lock().unwrap().push(session);
                                ServerMessage::Error(ProtocolError::new(
                                    ProtocolErrorCode::TerminalUnavailable,
                                    "Scripted provider admission recorded",
                                    false,
                                ))
                            }
                            ClientRequest::ChatAttach { session } => {
                                recorded.lock().unwrap().push(session.clone());
                                if let Some(script) = &upgrade {
                                    assert_eq!(session, script.canonical);
                                    if refuse_attachment {
                                        ServerMessage::Error(ProtocolError::new(
                                            ProtocolErrorCode::TerminalUnavailable,
                                            "Scripted exact attachment refused",
                                            false,
                                        ))
                                    } else {
                                        let response = ServerResponse::ChatAttached {
                                            snapshot: crate::chat::Snapshot {
                                                session_id: session.launch.session_id.clone(),
                                                ready: true,
                                                capabilities: crate::chat::Capabilities {
                                                    attachments,
                                                    ..Default::default()
                                                },
                                                ..Default::default()
                                            },
                                            session,
                                        };
                                        ServerMessage::Response { response }
                                    }
                                } else {
                                    ServerMessage::Error(ProtocolError::new(
                                        ProtocolErrorCode::TerminalUnavailable,
                                        "Scripted provider admission recorded",
                                        false,
                                    ))
                                }
                            }
                            ClientRequest::ChatRead { chat_id, .. } if upgrade.is_some() => {
                                ServerMessage::Response {
                                    response: ServerResponse::ChatSnapshot {
                                        chat_id,
                                        snapshot: None,
                                    },
                                }
                            }
                            ClientRequest::ChatVisibility { .. }
                            | ClientRequest::ChatAcknowledge { .. }
                            | ClientRequest::ChatDetach { .. }
                                if upgrade.is_some() =>
                            {
                                ServerMessage::Response {
                                    response: ServerResponse::Accepted,
                                }
                            }
                            other => panic!("unexpected startup request: {other:?}"),
                        };
                        if let Err(error) = write_frame(
                            &mut stream,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id: Some(request.request_id),
                                proof: proof.clone(),
                                message,
                            },
                        ) {
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                            if replaceable_visibility
                                && matches!(&error, crate::terminal_ipc::FrameError::Io(io)
                                    if matches!(io.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset))
                            {
                                // The offscreen attachment queues replaceable visibility
                                // before Ready. A retired target drops that late handle,
                                // shutting down its socket without awaiting this reply.
                                cancellations.fetch_add(1, Ordering::AcqRel);
                                let _ = visibility_canceled_tx.send(request.request_id);
                                break;
                            }
                            panic!("scripted client write: {error}");
                        }
                    }
                }));
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            providers,
            resolutions,
            controls,
            inventory,
            connections,
            stopped,
            visibility_cancellations,
            visibility_canceled,
            worker: Some(worker),
        }
    }

    fn requests(&self) -> Vec<Session> {
        self.providers.lock().unwrap().clone()
    }

    fn resolutions(&self) -> Vec<Session> {
        self.resolutions.lock().unwrap().clone()
    }
}

impl Drop for ScriptedDaemon {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        for stream in self.connections.lock().unwrap().iter() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        self.worker.take().unwrap().join().unwrap();
    }
}

fn settle(app: &mut Cagents, task: Task<Message>) {
    settle_observing(app, task, |_, message| Some(message));
}

pub(super) fn settle_observing(
    app: &mut Cagents,
    task: Task<Message>,
    mut observe: impl FnMut(&mut Cagents, Message) -> Option<Message>,
) {
    use cosmic::iced::futures::StreamExt;
    // Renderer initialization owns an executor. Build it before entering the
    // task executor; widget effects must not start a nested Tokio runtime.
    let renderer = crate::app::tests::headless_renderer();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut pending = VecDeque::from([task]);
        while let Some(task) = pending.pop_front() {
            let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) else {
                continue;
            };
            while let Some(action) = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("startup Task must settle against the scripted daemon")
            {
                match action {
                    cosmic::iced::runtime::Action::Output(cosmic::Action::App(message)) => {
                        // Scope selection also schedules lifecycle/focus work.
                        // Drive those actual reducers, not only chat replies.
                        if let Some(message) = observe(app, message) {
                            pending.push_back(app.update(message));
                        }
                    }
                    cosmic::iced::runtime::Action::Widget(mut operation) => {
                        use cosmic::iced::advanced::{
                            Layout, layout,
                            widget::{Tree, operation::Outcome},
                        };
                        let mut element = cosmic::Application::view(app);
                        let mut tree = Tree::new(element.as_widget());
                        let node = element.as_widget_mut().layout(
                            &mut tree,
                            &renderer,
                            &layout::Limits::new(
                                cosmic::iced::Size::ZERO,
                                cosmic::iced::Size::new(1600.0, 1200.0),
                            ),
                        );
                        loop {
                            element.as_widget_mut().operate(
                                &mut tree,
                                Layout::new(&node),
                                &renderer,
                                &mut operation,
                            );
                            match operation.finish() {
                                Outcome::Chain(next) => operation = next,
                                _ => break,
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    });
}

fn cached_chat() -> (Cagents, ChannelId, ChannelId, Pane, uuid::Uuid) {
    let (mut app, active, hidden, mut pane) = fixture();
    // Navigation schedules the configured offscreen maintenance grace. Keep
    // it bounded in this fixture while still executing its real reducer;
    // provider admission retains its independent five-second timeout.
    app.settings.hibernation_delay_ms = 0;
    app.lifecycle = LifecycleManager::new(LifecycleBudget {
        max_hot_cards: app.settings.max_hot_cards,
        offscreen_grace: Duration::ZERO,
    });
    let identity = app.workspace_model.identity;
    pane.kind = CardKind::Chat(Session {
        id: TerminalId::new(),
        launch: Launch {
            provider: crate::agent::Provider::Claude,
            session_id: Some("00000000-0000-4000-8000-000000000001".into()),
            recipe: LaunchRecipe {
                program: "claude".into(),
                arguments: vec![],
                working_directory: PathBuf::from("/fixture-project"),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
        },
        proof: Some(HostSessionProof {
            host_session_id: "old-host".into(),
            boot_id: "boot".into(),
            host_incarnation: "old-incarnation".into(),
        }),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    });
    let durable = Arc::make_mut(&mut app.durable);
    durable.workspace_model_identity = Some(identity);
    durable.active_channel = Some(active);
    durable
        .channels
        .iter_mut()
        .find(|c| c.id == hidden)
        .unwrap()
        .cards
        .push(pane.id);
    durable.cards.push(CardState {
        id: pane.id,
        generation: durable.generation,
        title: pane.title.clone(),
        width_fraction: pane.width_fraction,
        kind: pane.kind.clone(),
        existence: Existence::Open,
        exposure: Exposure::Offscreen,
        residency: Residency::Cold,
    });
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.shared_views = State::for_workspace(&app.durable);
    app.workspace_model = crate::workspace_model::WorkspaceModel::default();
    (app, active, hidden, pane, identity)
}

fn model(
    identity: uuid::Uuid,
    active: ChannelId,
    hidden: ChannelId,
    view: View,
) -> crate::workspace_model::WorkspaceModel {
    crate::workspace_model::WorkspaceModel {
        identity,
        sequence: 1,
        channels: [active, hidden]
            .into_iter()
            .map(|id| crate::workspace_model::ModelChannel {
                id,
                repository: PathBuf::from("/fixture-project"),
                title: "view".into(),
                title_source: Default::default(),
                ordinal: 1,
            })
            .collect(),
        views: vec![View::empty(active), view],
        ..Default::default()
    }
}

fn managed_owner(pane: &Pane, channel: ChannelId) -> crate::agent_execution::Agent {
    use crate::agent_execution::{Agent, HostedSession, Origin, Placement, Spec, Surface, Target};
    let CardKind::Chat(session) = &pane.kind else {
        panic!("chat fixture")
    };
    let placement = Placement::Channel(channel);
    Agent {
        id: crate::model::AgentId::from_uuid(pane.id.as_uuid()),
        name: None,
        origin: Origin::User,
        spec: Spec {
            provider: session.launch.provider,
            account: None,
            target: Target {
                repository: "/fixture-project".into(),
                directory: session.launch.recipe.working_directory.clone(),
                placement: Some(placement.clone()),
            },
            surface: Surface::Chat,
            model: None,
            permission_mode: None,
            reasoning_effort: None,
        },
        placement,
        session: Some(HostedSession::Chat(session.clone())),
        console: None,
        created_ms: 0,
        closed: false,
    }
}

fn validated_retirement_fixture() -> (Cagents, crate::workspace_model::WorkspaceModel, Pane) {
    let (mut app, active, hidden, pane, identity) = cached_chat();
    let mut view = View::empty(hidden);
    view.revision = 7;
    view.panes.push(pane.clone());
    let mut full = model(identity, active, hidden, view);
    full.sequence = 10;
    full.agents.push(managed_owner(&pane, hidden));
    assert!(app.install_workspace_model_snapshot(full.clone()));
    assert!(!app.pane_is_quarantined(pane.id));
    assert!(app.shared_views.validated_managed.contains_key(&pane.id));
    let view = &mut full.views[1];
    view.apply(&Change::Remove { id: pane.id }).unwrap();
    view.revision += 1;
    full.sequence += 1;
    (app, full, pane)
}

#[test]
fn verified_retirement_survives_repeated_refresh_without_borrowing_metadata_sequence() {
    let (mut app, mut retired, pane) = validated_retirement_fixture();
    // A previous transfer's source tombstone is unrelated to this exact
    // destination membership and its later authoritative removal.
    retired.views[0].retired.push(pane.id);
    // An unrelated folded metadata edge advances only the outer sequence.
    app.workspace_model.sequence = 100;
    let folded = app.workspace_model.clone();
    app.refresh_managed_pane_quarantine(&folded);
    assert!(app.install_workspace_model_snapshot(retired.clone()));
    for _ in 0..3 {
        app.refresh_managed_pane_quarantine(&retired);
        assert!(!app.pane_is_quarantined(pane.id));
    }
    // Passive retirement retains ordinary closed history across a restart;
    // an explicit durable rejection still wins over the same valid source.
    app.card_state_mut(pane.id).unwrap().existence = Existence::Closed {
        tombstone: crate::model::Tombstone {
            operation_id: None,
            closed_at_unix_ms: 1,
            reason: Some("removed by peer".into()),
        },
    };
    app.shared_views.validated_managed.clear();
    app.refresh_managed_pane_quarantine(&retired);
    assert!(!app.pane_is_quarantined(pane.id));
    Arc::make_mut(&mut app.durable)
        .quarantined_panes
        .insert(pane.id, pane.kind.clone());
    app.refresh_managed_pane_quarantine(&retired);
    assert!(app.pane_is_quarantined(pane.id));
}

#[test]
fn verified_retirement_never_borrows_rejected_ambiguous_or_changed_sources() {
    for case in 0..24 {
        let (mut app, mut retired, pane) = validated_retirement_fixture();
        match case {
            0 => {
                app.shared_views.quarantined.insert(pane.id);
            }
            1 => {
                Arc::make_mut(&mut app.durable)
                    .quarantined_panes
                    .insert(pane.id, pane.kind.clone());
            }
            2 => app.shared_views.validated_managed.clear(),
            3..=6 => {
                let CardKind::Chat(source) = &mut app.card_state_mut(pane.id).unwrap().kind else {
                    unreachable!()
                };
                match case {
                    3 => source.proof.as_mut().unwrap().boot_id = "replaced-boot".into(),
                    4 => source.launch.recipe.arguments.push("changed".into()),
                    5 => source.id = TerminalId::new(),
                    6 => source.launch.session_id = Some("changed-thread".into()),
                    _ => unreachable!(),
                }
            }
            7 => retired.identity = uuid::Uuid::new_v4(),
            8 => retired.views[1].scope = Scope::Channel(ChannelId::new()),
            9 => retired.agents.push(retired.agents[0].clone()),
            10 => {
                let mut duplicate = retired.agents[0].clone();
                duplicate.id = crate::model::AgentId::new();
                retired.agents.push(duplicate);
            }
            11 => {
                let mut alias = pane.clone();
                alias.id = CardId::new();
                retired.views[0].panes.push(alias);
            }
            12 => app.shared_views.pending.push((
                uuid::Uuid::new_v4(),
                retired.views[1].scope.clone(),
                Change::Insert {
                    pane: pane.clone(),
                    before: None,
                },
            )),
            13 => retired.views[1].revision -= 1,
            14 => {
                let durable = Arc::make_mut(&mut app.durable);
                for channel in &mut durable.channels {
                    channel.cards.retain(|id| *id != pane.id);
                }
                durable.channels[0].cards.push(pane.id);
            }
            15 => retired.agents[0].placement = Scope::Channel(ChannelId::new()),
            16 => retired
                .views
                .push(View::empty(retired.views[1].scope.clone())),
            17 => retired.sequence -= 1,
            18 => {
                let Some(crate::agent_execution::HostedSession::Chat(source)) =
                    &mut retired.agents[0].session
                else {
                    unreachable!()
                };
                source.proof.as_mut().unwrap().host_incarnation = "replaced-owner".into();
            }
            19 => retired.views[1].retired.push(pane.id),
            20 => {
                let Some(crate::agent_execution::HostedSession::Chat(source)) =
                    &mut retired.agents[0].session
                else {
                    unreachable!()
                };
                source.launch.session_id = Some("replaced-issuer-thread".into());
            }
            21 => retired.channels[1].repository = "/other-repository".into(),
            22 => {
                retired.channels.remove(1);
            }
            23 => retired.channels.push(retired.channels[1].clone()),
            _ => unreachable!(),
        }
        assert!(app.install_workspace_model_snapshot(retired.clone()));
        assert!(app.pane_is_quarantined(pane.id), "case {case}");
        assert!(
            !app.shared_views.validated_managed.contains_key(&pane.id),
            "case {case}"
        );
        assert_eq!(app.card_state(pane.id).unwrap().existence, Existence::Open);
        assert!(matches!(
            app.cards.get(&pane.id),
            Some(CardRuntime::Failed(_))
        ));
        app.refresh_managed_pane_quarantine(&retired);
        assert!(app.pane_is_quarantined(pane.id), "repeat case {case}");
    }
}

#[test]
fn closed_retirement_history_still_requires_exact_proof_and_scope() {
    for case in 0..6 {
        let (mut app, mut retired, pane) = validated_retirement_fixture();
        app.card_state_mut(pane.id).unwrap().existence = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: None,
            },
        };
        app.shared_views.validated_managed.clear();
        match case {
            0 => retired.channels[1].repository = "/other-repository".into(),
            1 => {
                retired.channels.remove(1);
            }
            2 => retired.channels.push(retired.channels[1].clone()),
            3 => retired.views[1].scope = Scope::Channel(ChannelId::new()),
            4 | 5 => {
                let CardKind::Chat(local) = &mut app.card_state_mut(pane.id).unwrap().kind else {
                    unreachable!()
                };
                if case == 4 {
                    local.proof = None;
                } else {
                    local.proof.as_mut().unwrap().boot_id = "".into();
                }
                retired.agents[0].session =
                    Some(crate::agent_execution::HostedSession::Chat(local.clone()));
            }
            _ => unreachable!(),
        }
        assert!(app.install_workspace_model_snapshot(retired.clone()));
        assert!(app.pane_is_quarantined(pane.id), "closed case {case}");
        assert!(!app.shared_views.validated_managed.contains_key(&pane.id));
    }
}

#[test]
fn verified_retirement_keeps_unsaved_work_without_manufacturing_failed_proof() {
    for closed_owner in [false, true] {
        let (mut app, mut retired, pane) = validated_retirement_fixture();
        let Scope::Channel(channel) = retired.views[1].scope else {
            unreachable!()
        };
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(
            pane.id,
            "Keep this unsent draft after peer removal".into(),
        )]));
        retired.agents[0].closed = closed_owner;
        assert!(app.install_workspace_model_snapshot(retired.clone()));
        assert!(!app.pane_is_quarantined(pane.id));
        drop(app.project_shared_view(&retired.views[1], &HashSet::new()));
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
        assert!(app.durable.local_only_cards.contains(&pane.id));
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![pane.id]);
        assert_eq!(
            app.persistent_workspace()
                .chat_drafts
                .get(&pane.id)
                .map(String::as_str),
            Some("Keep this unsent draft after peer removal")
        );
        app.refresh_managed_pane_quarantine(&retired);
        assert!(!app.pane_is_quarantined(pane.id));
    }
}

#[test]
fn retirement_witness_binds_canonical_metadata_and_issuer_separately() {
    for metadata in 0..2 {
        let (mut app, mut full, mut pane) = validated_retirement_fixture();
        // Provider observations enrich the shared pane without rewriting the
        // Agent's issued descriptor. Both records must keep their own identity.
        let CardKind::Chat(source) = &mut pane.kind else {
            unreachable!()
        };
        if metadata == 0 {
            source.launch.session_id = Some("00000000-0000-4000-8000-000000000099".into());
        } else {
            source.native_continuity = crate::chat::NativeContinuity::ReconnectOnly;
        }
        app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
        let Some(crate::agent_execution::HostedSession::Chat(issuer)) = &mut full.agents[0].session
        else {
            unreachable!()
        };
        issuer.launch.session_id = None;
        full.views[1].panes.push(pane.clone());
        full.views[1].retired.clear();
        assert!(app.install_workspace_model_snapshot(full.clone()));
        assert!(!app.pane_is_quarantined(pane.id));
        assert!(app.shared_views.validated_managed.contains_key(&pane.id));
        full.views[1]
            .apply(&Change::Remove { id: pane.id })
            .unwrap();
        full.views[1].revision += 1;
        full.sequence += 1;
        assert!(app.install_workspace_model_snapshot(full.clone()));
        app.refresh_managed_pane_quarantine(&full);
        assert!(!app.pane_is_quarantined(pane.id));
        app.card_state_mut(pane.id).unwrap().existence = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: None,
            },
        };
        app.shared_views.validated_managed.clear();
        app.refresh_managed_pane_quarantine(&full);
        assert!(!app.pane_is_quarantined(pane.id));
    }
}

#[test]
fn verified_terminal_retirement_preserves_exact_ended_and_archived_sources() {
    use crate::agent_execution::{HostedSession, Origin, Surface};
    for end in 0..3 {
        for console in [false, true] {
            for invalid in 0..4 {
                let (mut app, active, hidden, mut pane, identity) = cached_chat();
                let mut owner = managed_owner(&pane, hidden);
                let mut issued = attention_terminal_source();
                issued.launch.working_directory = "/fixture-project".into();
                let mut shown = issued.clone();
                let exit = crate::model::TerminalExit {
                    code: Some(0),
                    signal: None,
                    occurred_at_unix_ms: 1,
                };
                shown.continuity = match end {
                    0 => issued.continuity.clone(),
                    1 => TerminalContinuity::Exited { last_exit: exit },
                    2 => TerminalContinuity::RelaunchAvailable {
                        recipe: issued.launch.clone(),
                        last_exit: Some(exit),
                    },
                    _ => unreachable!(),
                };
                owner.spec.surface = Surface::Terminal;
                if console {
                    owner.origin = Origin::Automation {
                        id: uuid::Uuid::new_v4(),
                    };
                    owner.session = None;
                    owner.console = Some(issued.clone());
                } else {
                    owner.session = Some(HostedSession::Terminal(issued));
                }
                pane.kind = CardKind::Terminal(shown);
                app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
                let mut view = View::empty(hidden);
                view.revision = 7;
                view.panes.push(pane.clone());
                let mut full = model(identity, active, hidden, view);
                full.sequence = 10;
                full.agents.push(owner);
                assert!(app.install_workspace_model_snapshot(full.clone()));
                assert!(!app.pane_is_quarantined(pane.id));
                assert!(app.shared_views.validated_managed.contains_key(&pane.id));
                // An unchanged archived console may close its owner before
                // the separately ordered pane-removal snapshot arrives.
                if console && end != 0 {
                    full.agents[0].closed = true;
                    full.sequence += 1;
                    assert!(app.install_workspace_model_snapshot(full.clone()));
                    assert!(!app.pane_is_quarantined(pane.id));
                    assert!(app.shared_views.validated_managed.contains_key(&pane.id));
                }
                match invalid {
                    0 => {}
                    1 | 2 => {
                        let issuer = if console {
                            full.agents[0].console.as_mut().unwrap()
                        } else {
                            let Some(HostedSession::Terminal(issuer)) = &mut full.agents[0].session
                            else {
                                unreachable!()
                            };
                            issuer
                        };
                        if invalid == 1 {
                            issuer.continuity = TerminalContinuity::RelaunchAvailable {
                                recipe: issuer.launch.clone(),
                                last_exit: None,
                            };
                        } else {
                            issuer.transcript_path = Some("/changed-terminal-transcript".into());
                        }
                    }
                    3 => {
                        let CardKind::Terminal(local) =
                            &mut app.card_state_mut(pane.id).unwrap().kind
                        else {
                            unreachable!()
                        };
                        local.transcript_path = Some("/changed-local-transcript".into());
                    }
                    _ => unreachable!(),
                }
                full.views[1]
                    .apply(&Change::Remove { id: pane.id })
                    .unwrap();
                full.views[1].revision += 1;
                full.sequence += 1;
                assert!(app.install_workspace_model_snapshot(full.clone()));
                for _ in 0..2 {
                    app.refresh_managed_pane_quarantine(&full);
                    assert_eq!(
                        app.pane_is_quarantined(pane.id),
                        invalid != 0,
                        "end {end}, console {console}, invalid {invalid}"
                    );
                }
                if invalid == 0 {
                    app.card_state_mut(pane.id).unwrap().existence = Existence::Closed {
                        tombstone: crate::model::Tombstone {
                            operation_id: None,
                            closed_at_unix_ms: 1,
                            reason: None,
                        },
                    };
                    app.shared_views.validated_managed.clear();
                    app.refresh_managed_pane_quarantine(&full);
                    assert!(!app.pane_is_quarantined(pane.id));
                    assert!(app.terminal_starts.lock().unwrap().is_empty());
                }
            }
        }
    }
}

fn install_held_pane(app: &mut Cagents, channel: ChannelId, pane: &Pane) {
    let durable = Arc::make_mut(&mut app.durable);
    durable
        .channels
        .iter_mut()
        .find(|owner| owner.id == channel)
        .unwrap()
        .cards
        .push(pane.id);
    durable.cards.push(CardState {
        id: pane.id,
        generation: durable.generation,
        title: pane.title.clone(),
        width_fraction: pane.width_fraction,
        kind: pane.kind.clone(),
        existence: Existence::Open,
        exposure: Exposure::Offscreen,
        residency: Residency::Cold,
    });
}

fn sibling_chat(pane: &Pane, title: &str) -> Pane {
    let mut sibling = pane.clone();
    sibling.id = CardId::new();
    sibling.title = title.into();
    let CardKind::Chat(session) = &mut sibling.kind else {
        panic!("chat fixture")
    };
    session.id = TerminalId::new();
    session.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
    sibling
}

#[test]
fn ranked_snapshot_quarantines_raw_invalid_sibling_before_seed_projection() {
    use crate::app::workspace_model_sync::ModelUpdate;
    for invalid in ["wrong-proof", "duplicate-issuer", "masked-membership"] {
        let (mut app, active, hidden, mut rejected, identity) = cached_chat();
        current_owner(&mut rejected);
        app.card_state_mut(rejected.id).unwrap().kind = rejected.kind.clone();
        let healthy = sibling_chat(&rejected, "Healthy ranked sibling");
        install_held_pane(&mut app, hidden, &healthy);
        app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(
            rejected.id,
            "Keep the rejected ranked draft intact".into(),
        )]));
        app.shared_views = State::for_workspace(&app.durable);
        app.workspace_model.identity = identity;
        let mut view = View::empty(hidden);
        view.revision = 7;
        view.panes = vec![rejected.clone(), healthy.clone()];
        let mut full = model(identity, active, hidden, view);
        full.sequence = 10;
        full.channels[1].ordinal = 17;
        let mut issuer = managed_owner(&rejected, hidden);
        match invalid {
            "wrong-proof" => {
                let Some(crate::agent_execution::HostedSession::Chat(source)) = &mut issuer.session
                else {
                    unreachable!()
                };
                source.proof.as_mut().unwrap().host_incarnation = "different-issued-host".into();
            }
            "duplicate-issuer" => full.agents.push(issuer.clone()),
            "masked-membership" => {
                full.views[0].panes.push(rejected.clone());
                // Optimism may hide one membership from projection, but must
                // never hide raw ambiguity from proof admission.
                app.shared_views.pending.push((
                    uuid::Uuid::new_v4(),
                    Scope::Channel(active),
                    Change::Remove { id: rejected.id },
                ));
            }
            _ => unreachable!(),
        }
        full.agents
            .extend([issuer, managed_owner(&healthy, hidden)]);
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.apply_workspace_model_update(ModelUpdate::Ranked {
            channel: hidden,
            model: full.clone(),
        }));
        assert_eq!(app.shared_views.snapshot_sequence, 10, "{invalid}");
        assert_eq!(app.workspace_model.views, full.views, "{invalid}");
        assert_eq!(app.workspace_model.agents, full.agents, "{invalid}");
        let channel = app
            .durable
            .channels
            .iter()
            .find(|entry| entry.id == hidden)
            .unwrap();
        assert_eq!(channel.ordinal, 17, "{invalid}");
        assert!(channel.shared, "{invalid}");
        assert_eq!(
            app.shared_views.quarantined,
            HashSet::from([rejected.id]),
            "{invalid}"
        );
        assert!(app.shared_views.blocked.is_empty(), "{invalid}");
        assert_eq!(
            app.card_state(rejected.id).unwrap().kind,
            rejected.kind,
            "{invalid}"
        );
        assert_eq!(
            app.quarantined_pane_draft(rejected.id),
            Some("Keep the rejected ranked draft intact")
        );
        let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(edits.iter().any(|edit| matches!(edit,
            ModelEdit::SeedView { scope, panes, closed }
                if *scope == Scope::Channel(hidden) && panes == &vec![healthy.clone()] && closed.is_empty()
        )), "healthy-only SeedView must be real, not an empty publication: {invalid}");
        assert!(!edits.iter().any(|edit| match edit {
            ModelEdit::SeedView { panes, closed, .. } => closed.contains(&rejected.id)
                || panes.iter().any(|pane| pane.id == rejected.id),
            ModelEdit::ChangeView { change, .. } => change_card(change) == Some(rejected.id),
            _ => false,
        }), "{invalid}");
        drop(app.initialize_runtimes());
        drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: Scope::Channel(hidden),
            operation: None,
            model: full,
        }));
        let CardKind::Chat(healthy_source) = &healthy.kind else {
            unreachable!()
        };
        assert!(
            app.chat_start_matches(healthy.id, healthy_source),
            "{invalid}"
        );
        assert!(!app.chat_awaiting_admission(healthy.id), "{invalid}");
        assert!(!app.chat_start_pending(rejected.id), "{invalid}");
        assert!(
            !app.shared_views
                .validated_managed
                .contains_key(&rejected.id),
            "{invalid}"
        );
    }
}

/// Shared Draft option adoption and managed-proof admission must compose:
/// rejecting one raw sibling must not strand another pane's settled image.
#[test]
fn mixed_saved_scope_keeps_an_image_draft_through_peer_options_and_chooser_cancel() {
    use crate::{
        app::{
            chat_ui::{self, Action, managed_image_retention_test_support as custody},
            workspace_model_sync::ModelUpdate,
        },
        chat::{Attachment, attachment_assets},
        session::SessionRuntime,
    };
    if isolated("mixed_saved_scope_keeps_an_image_draft_through_peer_options_and_chooser_cancel") {
        return;
    }

    fn assert_stored(
        app: &mut Cagents,
        draft: &Pane,
        source: &Session,
        rejected: &Pane,
        image: &Attachment,
        rejected_image: &Attachment,
    ) {
        app.persist();
        let writer = app.session.as_ref().unwrap();
        writer.flush(Duration::from_secs(5)).unwrap();
        assert_eq!(writer.last_error(), None);
        let saved = writer
            .operation_store()
            .load_workspace(app.durable.id, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            saved
                .cards
                .iter()
                .find(|card| card.id == draft.id)
                .unwrap()
                .kind,
            CardKind::Chat(source.clone())
        );
        assert_eq!(
            saved
                .cards
                .iter()
                .find(|card| card.id == rejected.id)
                .unwrap()
                .kind,
            rejected.kind
        );
        assert!(
            saved
                .cards
                .iter()
                .filter(|card| card.id == draft.id || card.id == rejected.id)
                .all(|card| card.existence.is_open())
        );
        assert_eq!(
            saved.quarantined_panes,
            HashMap::from([(rejected.id, rejected.kind.clone())])
        );
        assert!(!saved.chat_drafts.contains_key(&draft.id));
        assert_eq!(saved.chat_draft_attachments[&draft.id], vec![image.clone()]);
        assert_eq!(
            saved.chat_drafts[&rejected.id],
            "Keep the rejected sibling's draft"
        );
        assert_eq!(
            saved.chat_draft_attachments[&rejected.id],
            vec![rejected_image.clone()]
        );
        attachment_assets::validate(&saved.chat_draft_attachments[&draft.id][0]).unwrap();
    }

    attachment_assets::with_test_image(|image| {
        let rejected_image = attachment_assets::test_image("Rejected sibling image");
        let (mut app, active, channel, mut rejected, identity) = cached_chat();
        current_owner(&mut rejected);
        app.card_state_mut(rejected.id).unwrap().kind = rejected.kind.clone();
        let mut draft = sibling_chat(&rejected, "Unused shared image draft");
        let CardKind::Chat(original) = &mut draft.kind else {
            unreachable!()
        };
        original.launch.session_id = None;
        original.proof = None;
        original.terminal = None;
        original.owned_codex_listener = None;
        original.native_continuity = Default::default();
        assert!(original.is_unstarted());
        let original = original.clone();
        install_held_pane(&mut app, channel, &draft);
        let durable = Arc::make_mut(&mut app.durable);
        durable.active_channel = Some(channel);
        durable.focused = Some(draft.id);
        let owner = durable
            .channels
            .iter_mut()
            .find(|entry| entry.id == channel)
            .unwrap();
        owner.cards = vec![draft.id, rejected.id];
        owner.focused = Some(draft.id);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.shared_views = State::for_workspace(&app.durable);
        app.chat_ui = chat_ui::State::with_draft_attachments(
            HashMap::from([(rejected.id, "Keep the rejected sibling's draft".into())]),
            HashMap::from([(rejected.id, vec![rejected_image.clone()])]),
        );
        let daemon = ScriptedDaemon::start();
        let startup = app.initialize_runtimes();
        settle(&mut app, startup);
        // A cached shared descriptor waits for its scope even when its
        // conversation is unstarted. Only the actual seed ACK admits it.
        assert!(app.chat_awaiting_admission(draft.id));
        assert!(app.chat_awaiting_admission(rejected.id));
        assert_eq!(
            app.durable.pending_chat_admissions.get(&draft.id),
            Some(&original)
        );
        let CardKind::Chat(rejected_source) = &rejected.kind else {
            unreachable!()
        };
        assert_eq!(
            app.durable.pending_chat_admissions.get(&rejected.id),
            Some(rejected_source)
        );
        assert!(daemon.requests().is_empty());
        assert!(daemon.resolutions().is_empty());
        let rejected_target = match app.cards.get(&rejected.id) {
            Some(CardRuntime::Chat(runtime)) => runtime.target,
            _ => panic!("cached rejected source awaits its real model"),
        };
        let viewport = app.update(Message::Viewport {
            generation: app.workspace_view_generation,
            event: WorkspaceViewportEvent::Resized(1200.0),
        });
        settle(&mut app, viewport);
        let mut view = View::empty(channel);
        view.revision = 7;
        view.panes = vec![draft.clone(), rejected.clone()];
        let mut full = model(identity, active, channel, view);
        full.sequence = 10;
        let mut issuer = managed_owner(&rejected, channel);
        let Some(crate::agent_execution::HostedSession::Chat(issued)) = &mut issuer.session else {
            unreachable!()
        };
        issued.proof.as_mut().unwrap().host_incarnation = "different-owner".into();
        full.agents = vec![issuer];
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        assert_eq!(
            app.workspace_model.identity,
            crate::workspace_model::NO_REGISTRY
        );
        let connected = app.apply_workspace_model_update(ModelUpdate::Connected(full.clone()));
        settle(&mut app, connected);
        let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        let seeds = edits
            .iter()
            .filter_map(|edit| match edit {
                ModelEdit::SeedView {
                    scope,
                    panes,
                    closed,
                } => Some((scope.clone(), panes.clone(), closed.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        let shared = seeds
            .iter()
            .filter(|(scope, _, _)| *scope == Scope::Channel(channel))
            .collect::<Vec<_>>();
        assert_eq!(shared.len(), 1, "one actual Draft-only seed: {edits:?}");
        assert_eq!(shared[0].1, vec![draft.clone()]);
        assert!(shared[0].2.is_empty());
        assert!(app.shared_views.seeding.contains(&Scope::Channel(channel)));
        assert!(
            !app.shared_views
                .initialized
                .contains(&Scope::Channel(channel))
        );
        assert!(
            !edits
                .iter()
                .any(|edit| matches!(edit, ModelEdit::ChangeView { .. }))
        );
        assert!(!edits.iter().any(
            |edit| matches!(edit, ModelEdit::SeedView { panes, closed, .. }
            if panes.iter().any(|pane| pane.id == rejected.id) || closed.contains(&rejected.id))
        ));
        assert_eq!(app.shared_views.quarantined, HashSet::from([rejected.id]));
        assert!(app.chat_awaiting_admission(draft.id));
        assert_eq!(
            app.durable.pending_chat_admissions.get(&draft.id),
            Some(&original)
        );
        // ACK only the scopes the production Connected reducer actually seeded.
        for (scope, panes, closed) in seeds {
            let admits_draft = scope == Scope::Channel(channel);
            let draft_still_held = !app
                .shared_views
                .initialized
                .contains(&Scope::Channel(channel));
            if draft_still_held {
                assert!(app.chat_awaiting_admission(draft.id));
            }
            assert!(closed.is_empty());
            if scope != Scope::Channel(channel) {
                assert_eq!(scope, Scope::Channel(active));
                assert!(panes.is_empty());
            }
            let ack = app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope,
                operation: None,
                model: full.clone(),
            });
            settle(&mut app, ack);
            if admits_draft {
                assert!(!app.chat_awaiting_admission(draft.id));
                assert!(!app.durable.pending_chat_admissions.contains_key(&draft.id));
            } else if draft_still_held {
                assert!(app.chat_awaiting_admission(draft.id));
            }
        }
        assert!(
            app.shared_views
                .initialized
                .contains(&Scope::Channel(channel))
        );
        assert!(!app.shared_views.seeding.contains(&Scope::Channel(channel)));
        assert!(!app.chat_awaiting_admission(draft.id));
        assert!(!app.shared_views.validated_managed.contains_key(&draft.id));
        assert_eq!(chat_ui::draft_launch_for_test(&app, draft.id).1, original);
        assert!(matches!(
            app.cards.get(&rejected.id),
            Some(CardRuntime::Failed(_))
        ));
        assert!(daemon.requests().is_empty());
        assert!(daemon.resolutions().is_empty());

        let project = app
            .durable
            .channels
            .iter()
            .find(|entry| entry.id == channel)
            .unwrap()
            .project_id;
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("mixed-draft-options.sqlite3");
        let (writer, _) =
            SessionRuntime::open(&database, crate::model::BootId::new("boot")).unwrap();
        writer.save_project(ProjectState {
            id: project,
            generation: 0,
            name: "Mixed shared Draft".into(),
            repository: "/fixture-project".into(),
            kind: ProjectKind::Folder,
            existence: Existence::Open,
        });
        app.session = Some(writer);
        let mut chosen = original.clone();
        chosen
            .launch
            .recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), "/accounts/claude/peer".into());
        chosen
            .launch
            .recipe
            .environment
            .insert("SHARED_DRAFT".into(), "peer value".into());
        chosen
            .launch
            .recipe
            .unset_environment
            .insert("OLD_DRAFT".into());
        chosen.launch.recipe.working_directory = "/fixture-project/peer".into();
        chosen
            .launch
            .recipe
            .arguments
            .extend(["--append-system-prompt".into(), "Peer draft options".into()]);
        crate::chat::apply_claude_permission_mode(&mut chosen.launch.recipe, "plan");
        assert!(chosen.is_unstarted());
        assert_eq!(chosen.id, original.id);
        custody::with_unstarted_image_only(&mut app, draft.id, &image, |app, target, mut late| {
            assert_stored(app, &draft, &original, &rejected, &image, &rejected_image);
            chat_ui::with_attachment_chooser_open_for_test(app, draft.id, |app| {
                full.sequence += 1;
                full.views[1].revision += 1;
                full.views[1].panes[0].kind = CardKind::Chat(chosen.clone());
                let peer = app.apply_workspace_model_update(ModelUpdate::Read(full.clone()));
                settle(app, peer);
                assert_eq!(
                    app.workspace_model.views, full.views,
                    "keep the untouched raw rejected sibling"
                );
                assert_eq!(app.workspace_model.agents, full.agents);
                assert_eq!(
                    app.card_state(draft.id).unwrap().kind,
                    CardKind::Chat(chosen.clone())
                );
                let Some(CardRuntime::Chat(runtime)) = app.cards.get(&draft.id) else {
                    panic!("same Draft runtime")
                };
                assert_eq!(runtime.target, target);
                assert_eq!(
                    runtime.session, original,
                    "chooser owns the earlier runtime until its cancellation callback"
                );
                assert_stored(app, &draft, &chosen, &rejected, &image, &rejected_image);
            });
            let (footer, shown) = chat_ui::draft_launch_for_test(app, draft.id);
            assert_eq!(footer.as_deref(), Some("plan"));
            assert_eq!(shown, chosen);
            assert_eq!(
                app.card_state(draft.id).unwrap().kind,
                CardKind::Chat(chosen.clone())
            );
            assert_eq!(app.shared_views.quarantined, HashSet::from([rejected.id]));
            assert!(matches!(
                app.cards.get(&rejected.id),
                Some(CardRuntime::Failed(_))
            ));
            // A late image completion with B's captured pre-quarantine Target
            // cannot install payload, reopen it, or affect A's settled image.
            let Message::ChatUi(Action::ImageSaved(reply, ..)) = &mut late else {
                unreachable!()
            };
            *reply = rejected_target;
            let stale = app.update(late);
            settle(app, stale);
            assert_eq!(app.shared_views.quarantined, HashSet::from([rejected.id]));
            assert!(matches!(
                app.cards.get(&rejected.id),
                Some(CardRuntime::Failed(_))
            ));
            assert_stored(app, &draft, &chosen, &rejected, &image, &rejected_image);
        });
        let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(!edits.iter().any(|edit| match edit {
            ModelEdit::SeedView { panes, closed, .. } =>
                panes.iter().any(|pane| pane.id == rejected.id) || closed.contains(&rejected.id),
            ModelEdit::ChangeView { change, .. } => change_card(change) == Some(rejected.id),
            _ => false,
        }));
        assert!(daemon.requests().is_empty());
        assert!(daemon.resolutions().is_empty());
        assert!(
            !daemon
                .controls
                .lock()
                .unwrap()
                .iter()
                .any(|request| matches!(
                    request,
                    ClientRequest::ChatStart { .. }
                        | ClientRequest::ChatAttach { .. }
                        | ClientRequest::ResolveUpgradeChat { .. }
                        | ClientRequest::ChatPrepareResume { .. }
                        | ClientRequest::ChatClose { .. }
                        | ClientRequest::ChatCloseRetained { .. }
                        | ClientRequest::CloseTerminalChecked { .. }
                ))
        );
        assert!(!app.chat_start_pending(draft.id));
        assert!(!app.chat_start_pending(rejected.id));
        assert!(
            !app.shared_views
                .validated_managed
                .contains_key(&rejected.id)
        );
        let mut writer = app.session.take().unwrap();
        writer.shutdown();
        assert_eq!(writer.last_error(), None);
        drop(writer);
        // Read the real saved database again without a second snapshot/save.
        let reopened =
            crate::persistence::StateStore::open(&database, crate::model::BootId::new("boot"))
                .unwrap();
        let saved = reopened
            .load_workspace(app.durable.id, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            saved
                .cards
                .iter()
                .find(|card| card.id == draft.id)
                .unwrap()
                .kind,
            CardKind::Chat(chosen)
        );
        assert_eq!(
            saved
                .cards
                .iter()
                .find(|card| card.id == rejected.id)
                .unwrap()
                .kind,
            rejected.kind
        );
        assert_eq!(
            saved.quarantined_panes,
            HashMap::from([(rejected.id, rejected.kind.clone())])
        );
        assert_eq!(
            saved.chat_drafts,
            HashMap::from([(rejected.id, "Keep the rejected sibling's draft".into())])
        );
        assert_eq!(
            saved.chat_draft_attachments,
            HashMap::from([
                (draft.id, vec![image.clone()]),
                (rejected.id, vec![rejected_image.clone()])
            ])
        );
        attachment_assets::validate(&saved.chat_draft_attachments[&draft.id][0]).unwrap();
        attachment_assets::validate(&saved.chat_draft_attachments[&rejected.id][0]).unwrap();
    });
}

#[test]
fn mixed_scope_verified_retirement_preserves_runtime_only_managed_images() {
    use crate::{
        app::{
            chat_ui::managed_image_retention_test_support as custody,
            workspace_model_sync::ModelUpdate,
        },
        chat::{Attachment, attachment_assets},
        session::SessionRuntime,
    };
    if isolated("mixed_scope_verified_retirement_preserves_runtime_only_managed_images") {
        return;
    }

    fn assert_saved(
        saved: &WorkspaceState,
        healthy: &Pane,
        rejected: &Pane,
        image: &Attachment,
        channel: ChannelId,
    ) {
        for pane in [healthy, rejected] {
            let card = saved.cards.iter().find(|card| card.id == pane.id).unwrap();
            assert_eq!(card.kind, pane.kind);
            assert!(card.existence.is_open());
        }
        assert_eq!(saved.local_only_cards, HashSet::from([healthy.id]));
        assert_eq!(
            saved.quarantined_panes,
            HashMap::from([(rejected.id, rejected.kind.clone())])
        );
        assert!(saved.chat_drafts.is_empty());
        assert_eq!(
            saved.chat_draft_attachments,
            HashMap::from([(healthy.id, vec![image.clone()])])
        );
        assert!(!saved.pending_chat_admissions.contains_key(&healthy.id));
        let CardKind::Chat(rejected_source) = &rejected.kind else {
            unreachable!()
        };
        assert_eq!(
            saved.pending_chat_admissions.get(&rejected.id),
            Some(rejected_source)
        );
        assert_eq!(
            saved
                .channels
                .iter()
                .find(|entry| entry.id == channel)
                .unwrap()
                .cards,
            vec![healthy.id, rejected.id]
        );
        attachment_assets::validate(&saved.chat_draft_attachments[&healthy.id][0]).unwrap();
    }

    attachment_assets::with_test_image(|image| {
        for parked in [false, true] {
            for closed_owner in [false, true] {
                let (mut app, active, channel, mut healthy, identity) = cached_chat();
                let healthy_source = current_owner(&mut healthy);
                app.card_state_mut(healthy.id).unwrap().kind = healthy.kind.clone();
                let rejected = sibling_chat(&healthy, "Unverified image-draft sibling");
                install_held_pane(&mut app, channel, &rejected);
                let durable = Arc::make_mut(&mut app.durable);
                durable.active_channel = Some(channel);
                durable.focused = Some(healthy.id);
                durable
                    .channels
                    .iter_mut()
                    .find(|entry| entry.id == channel)
                    .unwrap()
                    .focused = Some(healthy.id);
                app.workspace = Cagents::rebuild_workspace(&app.durable);
                app.shared_views = State::for_workspace(&app.durable);
                let daemon = ScriptedDaemon::with_image_owner(healthy_source.clone());
                let startup = app.initialize_runtimes();
                settle(&mut app, startup);
                assert!(app.chat_awaiting_admission(healthy.id));
                assert!(app.chat_awaiting_admission(rejected.id));
                assert!(daemon.requests().is_empty());

                // Mount the measured composer before its real attachment:
                // a hidden-first handle intentionally evicts its resident body.
                let viewport = app.update(Message::Viewport {
                    generation: app.workspace_view_generation,
                    event: WorkspaceViewportEvent::Resized(1200.0),
                });
                settle(&mut app, viewport);
                assert_eq!(app.workspace.viewport_width(), 1200.0);
                assert_eq!(
                    app.workspace.visibility(healthy.id),
                    Some(VisibilityClass::Visible)
                );
                assert!(app.workspace_surfaces_visible());
                assert!(app.chat_awaiting_admission(healthy.id));
                assert!(app.chat_awaiting_admission(rejected.id));
                assert!(daemon.requests().is_empty());

                let mut view = View::empty(channel);
                view.revision = 7;
                view.panes = vec![healthy.clone(), rejected.clone()];
                let mut full = model(identity, active, channel, view);
                full.sequence = 10;
                let mut issuer = managed_owner(&healthy, channel);
                let Some(crate::agent_execution::HostedSession::Chat(issued)) = &mut issuer.session
                else {
                    unreachable!()
                };
                // Canonical learned metadata and the original issued source
                // remain separately bound throughout this managed retirement.
                issued.launch.session_id = None;
                let mut invalid = managed_owner(&rejected, channel);
                let Some(crate::agent_execution::HostedSession::Chat(source)) =
                    &mut invalid.session
                else {
                    unreachable!()
                };
                source.proof.as_mut().unwrap().host_incarnation = "unrelated-sibling-owner".into();
                full.agents = vec![issuer, invalid];
                let (tx, rx) = async_channel::unbounded();
                app.workspace_model_tx = tx;
                assert_eq!(
                    app.workspace_model.identity,
                    crate::workspace_model::NO_REGISTRY
                );
                // A publisher rank cannot establish the initial registry or
                // admit cached sources before the real connection snapshot.
                let ranked = app.apply_workspace_model_update(ModelUpdate::Ranked {
                    channel,
                    model: full.clone(),
                });
                settle(&mut app, ranked);
                assert_eq!(
                    app.workspace_model.identity,
                    crate::workspace_model::NO_REGISTRY
                );
                assert!(
                    rx.try_recv().is_err(),
                    "an early rank publishes no view edits"
                );
                assert!(daemon.requests().is_empty());
                assert!(app.chat_awaiting_admission(healthy.id));
                assert!(app.chat_awaiting_admission(rejected.id));
                assert!(app.shared_views.seeding.is_empty());
                assert!(app.shared_views.initialized.is_empty());
                assert!(app.shared_views.pending.is_empty());

                let connected =
                    app.apply_workspace_model_update(ModelUpdate::Connected(full.clone()));
                settle(&mut app, connected);
                let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
                let seeds = edits
                    .iter()
                    .filter_map(|edit| match edit {
                        ModelEdit::SeedView {
                            scope,
                            panes,
                            closed,
                        } => Some((scope, panes, closed)),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let healthy_seeds = seeds
                    .iter()
                    .filter(|(scope, _, _)| **scope == Scope::Channel(channel))
                    .collect::<Vec<_>>();
                assert_eq!(healthy_seeds.len(), 1, "one actual managed seed: {edits:?}");
                assert_eq!(healthy_seeds[0].1, &vec![healthy.clone()]);
                assert!(healthy_seeds[0].2.is_empty());
                assert!(app.shared_views.seeding.contains(&Scope::Channel(channel)));
                assert!(
                    !app.shared_views
                        .initialized
                        .contains(&Scope::Channel(channel))
                );
                assert!(app.shared_views.pending.is_empty());
                assert!(
                    !edits
                        .iter()
                        .any(|edit| matches!(edit, ModelEdit::ChangeView { .. }))
                );
                assert!(!edits.iter().any(|edit| matches!(edit,
                    ModelEdit::SeedView { panes, closed, .. }
                        if panes.iter().any(|pane| pane.id == rejected.id) || closed.contains(&rejected.id)
                )));
                assert_eq!(app.shared_views.quarantined, HashSet::from([rejected.id]));
                assert!(daemon.requests().is_empty());
                // Acknowledge only the seeds actually emitted above. In
                // particular, no early None ACK can manufacture an Insert.
                for (scope, panes, closed) in seeds {
                    assert!(closed.is_empty());
                    if *scope != Scope::Channel(channel) {
                        assert_eq!(*scope, Scope::Channel(active));
                        assert!(panes.is_empty());
                    }
                    let ack = app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                        scope: scope.clone(),
                        operation: None,
                        model: full.clone(),
                    });
                    settle(&mut app, ack);
                }
                assert!(
                    app.shared_views
                        .initialized
                        .contains(&Scope::Channel(channel))
                );
                assert!(!app.shared_views.seeding.contains(&Scope::Channel(channel)));
                assert!(app.shared_views.pending.is_empty());
                assert_eq!(daemon.requests(), vec![healthy_source.clone()]);
                assert!(!app.chat_start_pending(healthy.id));
                assert!(!app.chat_awaiting_admission(healthy.id));
                let witness = app.shared_views.validated_managed.get(&healthy.id).unwrap();
                assert_eq!(witness.registry, identity);
                assert_eq!(witness.scope, Scope::Channel(channel));
                assert_eq!(witness.source, healthy.kind);
                assert_eq!(Some(&witness.issuer), full.agents[0].session.as_ref());
                assert_eq!(witness.snapshot_sequence, 10);
                assert_eq!(witness.revision, 7);
                assert!(
                    !app.shared_views
                        .validated_managed
                        .contains_key(&rejected.id)
                );
                assert!(matches!(
                    app.cards.get(&rejected.id),
                    Some(CardRuntime::Failed(_))
                ));

                // The synthetic daemon's actual ChatAttached response owns
                // readiness and image capability; no Ready/snapshot is injected.
                let Some(CardRuntime::Chat(runtime)) = app.cards.get(&healthy.id) else {
                    panic!("admitted healthy runtime");
                };
                assert_eq!(runtime.restored_pane_ready(), Ok(true));
                assert_eq!(runtime.session, healthy_source);
                assert_eq!(runtime.unsent_text(), "");
                assert_eq!(runtime.transition_blocker(), None);
                custody::insert_image_only(&mut app, healthy.id, &image);
                let allocation =
                    custody::assert_runtime_only_image(&app, healthy.id, &image, false, false);
                if parked {
                    custody::park(&mut app, healthy.id);
                }
                assert_eq!(
                    custody::assert_runtime_only_image(&app, healthy.id, &image, parked, false),
                    allocation
                );
                assert!(app.shared_chat_has_local_work(healthy.id));
                assert_eq!(app.device_switch_chat_blocker(), None);
                assert!(!app.durable.local_only_cards.contains(&healthy.id));
                let project = app
                    .durable
                    .channels
                    .iter()
                    .find(|entry| entry.id == channel)
                    .unwrap()
                    .project_id;
                let directory = tempfile::tempdir().unwrap();
                let database = directory.path().join("mixed-image-custody.sqlite3");
                let boot = crate::model::BootId::new("boot");
                let (writer, _) = SessionRuntime::open(&database, boot.clone()).unwrap();
                writer.save_project(ProjectState {
                    id: project,
                    generation: 0,
                    name: "Mixed image custody".into(),
                    repository: "/fixture-project".into(),
                    kind: ProjectKind::Folder,
                    existence: Existence::Open,
                });
                app.session = Some(writer);
                app.persist();
                app.session
                    .as_ref()
                    .unwrap()
                    .flush(Duration::from_secs(5))
                    .unwrap();
                assert_eq!(
                    custody::assert_runtime_only_image(&app, healthy.id, &image, parked, false),
                    allocation
                );
                assert!(app.shared_views.pending.is_empty());
                assert!(
                    app.shared_views
                        .initialized
                        .contains(&Scope::Channel(channel))
                );
                assert!(!app.shared_views.seeding.contains(&Scope::Channel(channel)));
                assert_eq!(app.shared_views.quarantined, HashSet::from([rejected.id]));
                let witness = app.shared_views.validated_managed.get(&healthy.id).unwrap();
                assert_eq!(witness.source, healthy.kind);
                assert_eq!(Some(&witness.issuer), full.agents[0].session.as_ref());
                assert_eq!(witness.snapshot_sequence, 10);
                assert_eq!(witness.revision, 7);

                let mut retired = full.clone();
                retired.sequence = 11;
                retired.agents[0].closed = closed_owner;
                assert!(
                    retired.views[1]
                        .apply(&Change::Remove { id: healthy.id })
                        .unwrap()
                );
                retired.views[1].revision = 8;
                assert_eq!(retired.views[1].panes, vec![rejected.clone()]);
                assert_eq!(retired.views[1].retired, vec![healthy.id]);
                for update in [
                    ModelUpdate::Read(retired.clone()),
                    ModelUpdate::Ranked {
                        channel,
                        model: retired.clone(),
                    },
                    ModelUpdate::Read(retired.clone()),
                ] {
                    let task = app.update(Message::WorkspaceModelObserved(update));
                    settle(&mut app, task);
                    assert_eq!(app.workspace_model.views, retired.views);
                    assert_eq!(app.workspace_model.agents, retired.agents);
                    assert_eq!(app.shared_views.snapshot_sequence, 11);
                    assert_eq!(app.shared_views.quarantined, HashSet::from([rejected.id]));
                    assert!(
                        !app.shared_views
                            .validated_managed
                            .contains_key(&rejected.id)
                    );
                    assert_eq!(
                        custody::assert_runtime_only_image(&app, healthy.id, &image, parked, true),
                        allocation
                    );
                    assert_saved(
                        &app.persistent_workspace(),
                        &healthy,
                        &rejected,
                        &image,
                        channel,
                    );
                    assert!(!app.chat_start_pending(healthy.id));
                    assert!(!app.chat_start_pending(rejected.id));
                    assert!(app.shared_views.pending.is_empty());
                    app.persist();
                    app.session
                        .as_ref()
                        .unwrap()
                        .flush(Duration::from_secs(5))
                        .unwrap();
                    assert_eq!(app.session.as_ref().unwrap().last_error(), None);
                }
                assert!(
                    rx.try_recv().is_err(),
                    "retained work must not republish either source"
                );
                assert_eq!(daemon.requests(), vec![healthy_source]);
                assert!(daemon.resolutions().is_empty());
                assert!(
                    !daemon
                        .controls
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|request| matches!(
                            request,
                            ClientRequest::ChatStart { .. }
                                | ClientRequest::ChatPrepareResume { .. }
                                | ClientRequest::ChatClose { .. }
                                | ClientRequest::ChatCloseRetained { .. }
                                | ClientRequest::CloseTerminalChecked { .. }
                                | ClientRequest::ChatRecoverAccount { .. }
                                | ClientRequest::ChatStartSelected { .. }
                                | ClientRequest::ChatReturnTerminal { .. }
                        ))
                );
                let mut writer = app.session.take().unwrap();
                writer.shutdown();
                assert_eq!(writer.last_error(), None);
                drop(writer);
                // These are real private SQLite writer/load cycles, not a
                // claim that a cold native UI has reattached the retired chat.
                for _ in 0..2 {
                    let (mut reopened, loaded) =
                        SessionRuntime::open(&database, boot.clone()).unwrap();
                    let loaded = loaded.expect("saved mixed workspace");
                    assert_saved(&loaded, &healthy, &rejected, &image, channel);
                    reopened.snapshot(Arc::new(loaded));
                    reopened.flush(Duration::from_secs(5)).unwrap();
                    assert_eq!(reopened.last_error(), None);
                    reopened.shutdown();
                }
                drop(app);
                drop(daemon);
                std::fs::remove_file(crate::terminal_ipc::daemon_socket_path().unwrap()).unwrap();
            }
        }
    });
}

#[test]
fn ranked_snapshot_retires_verified_pane_behind_newer_metadata() {
    use crate::app::workspace_model_sync::ModelUpdate;
    let (mut app, mut retired, pane) = validated_retirement_fixture();
    let Scope::Channel(channel) = retired.views[1].scope else {
        unreachable!()
    };
    let mut previous = app.workspace_model.clone();
    // A provider-learned thread belongs to canonical pane A, while the
    // issuer's original B still has no thread. Rank admission must bind both.
    let Some(crate::agent_execution::HostedSession::Chat(issuer)) = &mut previous.agents[0].session
    else {
        unreachable!()
    };
    issuer.launch.session_id = None;
    retired.agents[0] = previous.agents[0].clone();
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    drop(app.apply_workspace_model_update(ModelUpdate::Ranked {
        channel,
        model: previous.clone(),
    }));
    let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    let seeds = edits
        .iter()
        .filter_map(|edit| match edit {
            ModelEdit::SeedView {
                scope,
                panes,
                closed,
            } if *scope == Scope::Channel(channel) => Some((panes, closed)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(seeds.len(), 1);
    assert_eq!(seeds[0].0, &vec![pane.clone()]);
    assert!(seeds[0].1.is_empty());
    assert!(!edits.iter().any(|edit| matches!(edit,
        ModelEdit::ChangeView { change, .. } if change_card(change) == Some(pane.id)
    )));
    assert!(app.shared_views.seeding.contains(&Scope::Channel(channel)));
    assert!(
        !app.shared_views
            .initialized
            .contains(&Scope::Channel(channel))
    );
    assert!(
        !app.shared_views
            .pending
            .iter()
            .any(|(_, _, change)| change_card(change) == Some(pane.id))
    );
    drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
        scope: Scope::Channel(channel),
        operation: None,
        model: previous.clone(),
    }));
    assert!(
        app.shared_views
            .initialized
            .contains(&Scope::Channel(channel))
    );
    assert!(!app.shared_views.seeding.contains(&Scope::Channel(channel)));
    assert!(
        !app.shared_views
            .pending
            .iter()
            .any(|(_, _, change)| change_card(change) == Some(pane.id))
    );
    assert!(!app.pane_is_quarantined(pane.id));
    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence: previous.sequence,
        edit: ModelEdit::CreateChannel {
            id: channel,
            repository: "/fixture-project".into(),
            title: "Late creation echo".into(),
            title_source: Default::default(),
        },
    }));
    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence: 100,
        edit: ModelEdit::RetitleChannel {
            id: channel,
            title: "Newer manual title".into(),
        },
    }));
    assert!(!app.shared_chat_has_local_work(pane.id));
    assert!(
        app.shared_views
            .initialized
            .contains(&Scope::Channel(channel))
    );
    assert!(!app.shared_views.seeding.contains(&Scope::Channel(channel)));
    assert!(
        !app.shared_views
            .pending
            .iter()
            .any(|(_, _, change)| change_card(change) == Some(pane.id))
    );
    let witness = app.shared_views.validated_managed.get(&pane.id).unwrap();
    assert_eq!(witness.registry, previous.identity);
    assert_eq!(witness.scope, Scope::Channel(channel));
    assert_eq!(witness.source, pane.kind);
    assert_eq!(Some(&witness.issuer), previous.agents[0].session.as_ref());
    assert_eq!(witness.snapshot_sequence, 10);
    assert_eq!(witness.revision, 7);
    assert!(!app.pane_is_quarantined(pane.id));
    retired.agents[0].closed = true;
    for _ in 0..2 {
        drop(app.apply_workspace_model_update(ModelUpdate::Ranked {
            channel,
            model: retired.clone(),
        }));
        assert_eq!(app.workspace_model.sequence, 100);
        assert_eq!(
            app.workspace_model.channel(channel).unwrap().title,
            "Newer manual title"
        );
        assert_eq!(app.shared_views.snapshot_sequence, 11);
        assert_eq!(app.workspace_model.views, retired.views);
        assert!(!app.card_state(pane.id).unwrap().existence.is_open());
        assert!(!app.pane_is_quarantined(pane.id));
    }
    drop(app.apply_workspace_model_update(ModelUpdate::Read(previous.clone())));
    drop(app.apply_workspace_model_update(ModelUpdate::Ranked {
        channel,
        model: previous,
    }));
    assert_eq!(app.workspace_model.views, retired.views);
    assert!(!app.card_state(pane.id).unwrap().existence.is_open());
    assert!(!app.pane_is_quarantined(pane.id));
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(edit,
            ModelEdit::ChangeView { change: Change::Insert { pane: inserted, .. }, .. }
                if inserted.id == pane.id
        ))
    );
}

#[test]
fn ranked_snapshot_and_late_create_echo_preserve_closed_durable_denial() {
    use crate::{
        app::workspace_model_sync::ModelUpdate,
        model::BootId,
        persistence::{SnapshotBase, StateStore},
    };
    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    current_owner(&mut pane);
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    let attachment = crate::chat::Attachment {
        path: "/retained/ranked-image.png".into(),
        reference: Some(crate::chat::ImageReference {
            id: uuid::Uuid::new_v4(),
            number: 1,
            device: uuid::Uuid::new_v4(),
            name: "ranked-image.png".into(),
            sha256: "0".repeat(64),
            extension: "png".into(),
        }),
    };
    app.chat_ui = chat_ui::State::with_draft_attachments(
        HashMap::from([(pane.id, "Closed ranked draft café 🌿".into())]),
        HashMap::from([(pane.id, vec![attachment.clone()])]),
    );
    let mut view = View::empty(hidden);
    view.panes.push(pane.clone());
    let mut full = model(identity, active, hidden, view);
    let mut owner = managed_owner(&pane, hidden);
    owner.closed = true;
    full.agents.push(owner);
    app.workspace_model = full.clone();
    app.refresh_managed_pane_quarantine(&full);
    drop(app.begin_chat_close(pane.id));
    let saved = app.persistent_workspace();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ranked-denial.sqlite3");
    let store = StateStore::open(&path, BootId::new("ranked-denial")).unwrap();
    store
        .save_workspace(
            &saved,
            SnapshotBase {
                workspace: 0,
                cards: &Default::default(),
            },
        )
        .unwrap();
    drop(store);
    let store = StateStore::open(&path, BootId::new("ranked-denial-reload")).unwrap();
    app.durable = Arc::new(store.load_workspace(saved.id, None).unwrap().unwrap());
    app.shared_views = State::for_workspace(&app.durable);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.chat_ui = chat_ui::State::with_draft_attachments(
        app.durable.chat_drafts.clone(),
        app.durable.chat_draft_attachments.clone(),
    );
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    full.sequence += 1;
    full.agents[0].closed = false;
    full.channels[1].ordinal = 17;
    drop(app.apply_workspace_model_update(ModelUpdate::Ranked {
        channel: hidden,
        model: full.clone(),
    }));
    assert_eq!(app.shared_views.snapshot_sequence, full.sequence);
    assert_eq!(app.workspace_model.agents, full.agents);
    assert_eq!(app.workspace_model.views, full.views);
    let ranked = app
        .durable
        .channels
        .iter()
        .find(|entry| entry.id == hidden)
        .unwrap();
    assert_eq!(ranked.ordinal, 17);
    assert!(ranked.shared);
    let title = app.workspace_model.channel(hidden).unwrap().title.clone();
    drop(app.apply_workspace_model_update(ModelUpdate::Edited {
        sequence: full.sequence,
        edit: ModelEdit::CreateChannel {
            id: hidden,
            repository: "/fixture-project".into(),
            title: "Late create echo".into(),
            title_source: Default::default(),
        },
    }));
    assert_eq!(app.workspace_model.channel(hidden).unwrap().title, title);
    let restored = app.persistent_workspace();
    assert!(!app.card_state(pane.id).unwrap().existence.is_open());
    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
    assert!(restored.local_only_cards.contains(&pane.id));
    assert_eq!(restored.quarantined_panes[&pane.id], pane.kind);
    assert_eq!(
        restored.chat_drafts[&pane.id],
        "Closed ranked draft café 🌿"
    );
    assert_eq!(restored.chat_draft_attachments[&pane.id], vec![attachment]);
    assert!(!app.chat_start_pending(pane.id));
    assert!(!app.workspace.order().any(|id| id == pane.id));
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| match edit {
            ModelEdit::SeedView { panes, closed, .. } =>
                closed.contains(&pane.id) || panes.iter().any(|source| source.id == pane.id),
            ModelEdit::ChangeView { change, .. } => change_card(&change) == Some(pane.id),
            _ => false,
        })
    );
}

#[test]
fn managed_quarantine_uses_full_ownership_proof_and_pending_sources() {
    for invalid in [
        "closed",
        "wrong-proof",
        "retired",
        "ambiguous",
        "duplicate-issuer",
        "pending-source",
    ] {
        let (mut app, active, hidden, mut pane, identity) = cached_chat();
        current_owner(&mut pane);
        app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
        let mut owner = managed_owner(&pane, hidden);
        let mut view = View::empty(hidden);
        view.panes.push(pane.clone());
        let mut authoritative = model(identity, active, hidden, view);
        let mut pending = Vec::new();
        match invalid {
            "closed" => owner.closed = true,
            "wrong-proof" => {
                let crate::agent_execution::HostedSession::Chat(source) =
                    owner.session.as_mut().unwrap()
                else {
                    unreachable!()
                };
                source.proof.as_mut().unwrap().host_incarnation = "replacement-owner".into();
            }
            "retired" => {
                authoritative.views[1].panes.clear();
                authoritative.views[1].retired.push(pane.id);
            }
            "ambiguous" => {
                let mut duplicate = authoritative.views[1].clone();
                duplicate.scope = Scope::Channel(active);
                authoritative.views.push(duplicate);
            }
            "duplicate-issuer" => {
                let mut duplicate = owner.clone();
                duplicate.closed = true;
                authoritative.agents.push(duplicate);
            }
            "pending-source" => {
                let mut foreign = pane.clone();
                let CardKind::Chat(session) = &mut foreign.kind else {
                    unreachable!()
                };
                session.proof.as_mut().unwrap().host_incarnation = "foreign-pending-owner".into();
                pending.push((
                    uuid::Uuid::new_v4(),
                    Scope::Channel(hidden),
                    Change::Update {
                        before: pane.clone(),
                        pane: foreign,
                    },
                ));
            }
            _ => unreachable!(),
        }
        authoritative.agents.push(owner);
        assert_eq!(
            quarantined_managed_panes(
                &authoritative,
                &app.durable,
                &HashSet::new(),
                &pending,
                0,
                &mut HashMap::new()
            ),
            HashSet::from([pane.id]),
            "{invalid}"
        );
        app.shared_views.pending = pending;
        app.workspace_model = authoritative;
        drop(app.reconcile_shared_views());
        assert!(
            app.shared_views.blocked.is_empty(),
            "{invalid} is pane-local"
        );
        assert!(app.shared_views.quarantined.contains(&pane.id));
        assert!(
            !app.chat_start_pending(pane.id),
            "{invalid} must not dispatch attachment"
        );
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        assert!(
            app.shared_views.pending.is_empty(),
            "a rejected pending source cannot survive as an overlay"
        );
    }
}

#[test]
fn managed_alias_duplicates_and_pending_resource_swaps_remain_quarantined() {
    for mode in [
        "duplicate",
        "duplicate-update",
        "duplicate-insert",
        "resource-swap",
        "two-issuers",
    ] {
        let (mut app, active, hidden, mut canonical, identity) = cached_chat();
        current_owner(&mut canonical);
        let alias = CardId::new();
        let old = canonical.id;
        app.card_state_mut(old).unwrap().id = alias;
        app.card_state_mut(alias).unwrap().kind = canonical.kind.clone();
        Arc::make_mut(&mut app.durable)
            .channels
            .iter_mut()
            .find(|row| row.id == hidden)
            .unwrap()
            .cards = vec![alias];
        let mut local = canonical.clone();
        local.id = alias;
        let mut view = View::empty(hidden);
        view.panes.push(local.clone());
        let mut full = model(identity, active, hidden, view);
        let owner = managed_owner(&canonical, hidden);
        full.agents.push(owner.clone());
        if mode.starts_with("duplicate") {
            full.views[0].panes.push(local.clone());
        }
        let mut pending = Vec::new();
        if mode == "duplicate-update" || mode == "resource-swap" {
            let mut updated = local.clone();
            updated.title = "Pending alias title".into();
            if mode == "resource-swap" {
                let CardKind::Chat(source) = &mut updated.kind else {
                    unreachable!()
                };
                source.id = TerminalId::new();
                Arc::make_mut(&mut app.durable)
                    .quarantined_panes
                    .insert(alias, local.kind.clone());
            }
            pending.push((
                uuid::Uuid::new_v4(),
                Scope::Channel(active),
                Change::Update {
                    before: local.clone(),
                    pane: updated,
                },
            ));
        }
        if mode == "duplicate-insert" {
            pending.push((
                uuid::Uuid::new_v4(),
                Scope::Channel(hidden),
                Change::Insert {
                    pane: local.clone(),
                    before: None,
                },
            ));
        }
        if mode == "two-issuers" {
            let mut other = owner;
            other.id = crate::model::AgentId::new();
            full.agents.push(other);
            full.views[1].panes.push(canonical.clone());
        }
        let expected = if mode == "two-issuers" {
            HashSet::from([alias, canonical.id])
        } else {
            HashSet::from([alias])
        };
        assert_eq!(
            quarantined_managed_panes(
                &full,
                &app.durable,
                &HashSet::new(),
                &pending,
                0,
                &mut HashMap::new()
            ),
            expected,
            "{mode}"
        );
        app.shared_views = State::for_workspace(&app.durable);
        app.shared_views.pending = pending;
        app.shared_views
            .initialized
            .extend([Scope::Channel(active), Scope::Channel(hidden)]);
        app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(
            alias,
            "Keep duplicate-alias evidence".into(),
        )]));
        app.workspace_model = full;
        drop(app.reconcile_shared_views());
        assert!(app.shared_views.quarantined.contains(&alias));
        assert!(!app.chat_start_pending(alias));
        assert_eq!(app.card_state(alias).unwrap().kind, local.kind);
        assert_eq!(
            app.persistent_workspace().chat_drafts[&alias],
            "Keep duplicate-alias evidence"
        );
        assert!(app.shared_views.pending.is_empty());
        assert!(app.shared_views.adoptions.is_empty());
    }
}

#[test]
fn managed_terminal_alias_retains_its_observed_end_before_the_update_acknowledgement() {
    use crate::agent_execution::Surface;
    use crate::app::workspace_model_sync::ModelUpdate;
    let (mut app, active, hidden, mut canonical, identity) = cached_chat();
    let mut owner = managed_owner(&canonical, hidden);
    canonical.kind = moving_terminal_kind();
    let CardKind::Terminal(issued) = &mut canonical.kind else {
        unreachable!()
    };
    issued.launch.working_directory = "/fixture-project".into();
    owner.spec.target.directory = issued.launch.working_directory.clone();
    owner.session = None;
    owner.console = Some(issued.clone());
    owner.spec.surface = Surface::Terminal;
    let old = canonical.id;
    let mut alias = canonical.clone();
    alias.id = CardId::new();
    app.card_state_mut(old).unwrap().id = alias.id;
    app.card_state_mut(alias.id).unwrap().kind = alias.kind.clone();
    let durable = Arc::make_mut(&mut app.durable);
    durable
        .channels
        .iter_mut()
        .find(|row| row.id == hidden)
        .unwrap()
        .cards = vec![alias.id];
    let mut view = View::empty(hidden);
    view.panes.push(alias.clone());
    let mut full = model(identity, active, hidden, view);
    full.agents.push(owner);
    app.workspace_model = full.clone();
    app.shared_views = State::for_workspace(&app.durable);
    app.shared_views
        .initialized
        .extend([Scope::Channel(active), Scope::Channel(hidden)]);
    app.shared_views
        .baseline
        .insert(Scope::Channel(active), Vec::new());
    app.shared_views
        .baseline
        .insert(Scope::Channel(hidden), vec![alias.clone()]);
    // Completed/Exited use this production reducer and persist the resulting
    // descriptor. A Completed placeholder keeps this fixture free of actors.
    app.cards.insert(alias.id, CardRuntime::Completed);
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    app.retire_ended_terminal(alias.id, 17, None, false);
    app.persist();
    let observed = app.card_state(alias.id).unwrap().kind.clone();
    assert!(exact_issued_terminal_end(&alias.kind, &observed));
    let published = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    let update = published
        .iter()
        .find_map(|edit| match edit {
            ModelEdit::ChangeView {
                operation,
                change: Change::Update { before, pane },
                ..
            } if before.kind == alias.kind && pane.kind == observed => Some(*operation),
            _ => None,
        })
        .expect("the production end reducer publishes its exact source update");
    assert!(app.shared_views.pending.iter().any(
        |(_, _, change)| matches!(change, Change::Update { pane, .. } if pane.kind == observed)
    ));
    let mut changed_proof = app.shared_views.pending.clone();
    for (_, _, change) in &mut changed_proof {
        if let Change::Update { before, .. } = change
            && let CardKind::Terminal(source) = &mut before.kind
            && let TerminalContinuity::Live {
                host_incarnation, ..
            } = &mut source.continuity
        {
            *host_incarnation = "foreign-observer".into();
        }
    }
    assert!(
        quarantined_managed_panes(
            &full,
            &app.durable,
            &HashSet::new(),
            &changed_proof,
            0,
            &mut HashMap::new()
        )
        .contains(&alias.id),
        "an end cannot launder a changed captured host proof"
    );
    drop(app.apply_workspace_model_update(ModelUpdate::Read(full.clone())));
    assert!(!app.pane_is_quarantined(alias.id));
    assert_eq!(app.card_state(alias.id).unwrap().kind, observed);
    assert!(!matches!(
        app.cards.get(&alias.id),
        Some(CardRuntime::Failed(_))
    ));
    assert!(app.terminal_starts.lock().unwrap().is_empty());
    let mut ended_model = full;
    ended_model.sequence += 1;
    ended_model
        .views
        .iter_mut()
        .flat_map(|view| &mut view.panes)
        .find(|pane| pane.id == alias.id)
        .unwrap()
        .kind = observed.clone();
    assert!(
        quarantined_managed_panes(
            &ended_model,
            &app.durable,
            &HashSet::new(),
            &app.shared_views.pending,
            0,
            &mut HashMap::new(),
        )
        .is_empty(),
        "installing the ended snapshot before ACK cannot briefly quarantine the retained surface"
    );
    assert!(
        quarantined_managed_panes(
            &ended_model,
            &app.durable,
            &HashSet::new(),
            &changed_proof,
            0,
            &mut HashMap::new()
        )
        .contains(&alias.id),
        "an ended raw descriptor cannot hide a foreign captured proof"
    );
    drop(app.apply_workspace_model_update(ModelUpdate::Read(ended_model.clone())));
    assert!(
        app.shared_views
            .pending
            .iter()
            .any(|(operation, _, _)| *operation == update)
    );
    assert!(!app.pane_is_quarantined(alias.id));
    assert!(matches!(
        app.cards.get(&alias.id),
        Some(CardRuntime::Completed)
    ));
    drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
        scope: Scope::Channel(hidden),
        operation: Some(update),
        model: ended_model.clone(),
    }));
    assert!(
        !app.shared_views
            .pending
            .iter()
            .any(|(operation, _, _)| *operation == update)
    );
    assert!(!app.pane_is_quarantined(alias.id));
    assert!(!app.shared_views.quarantine_actions.contains_key(&alias.id));
    assert!(matches!(
        app.cards.get(&alias.id),
        Some(CardRuntime::Completed)
    ));
    assert_eq!(app.card_state(alias.id).unwrap().kind, observed);
    // A different window may still have the exact live source cached when
    // this valid end arrives, including after its managed owner closes.
    for closed_owner in [false, true] {
        app.card_state_mut(alias.id).unwrap().kind = alias.kind.clone();
        app.cards.insert(alias.id, CardRuntime::Loading);
        ended_model.sequence += 1;
        ended_model.agents[0].closed = closed_owner;
        assert!(
            quarantined_managed_panes(
                &ended_model,
                &app.durable,
                &HashSet::new(),
                &[],
                0,
                &mut HashMap::new()
            )
            .is_empty()
        );
        drop(app.apply_workspace_model_update(ModelUpdate::Read(ended_model.clone())));
        assert!(!app.pane_is_quarantined(alias.id));
        assert!(matches!(
            app.cards.get(&alias.id),
            Some(CardRuntime::Completed)
        ));
        assert_eq!(app.card_state(alias.id).unwrap().kind, observed);
        assert!(app.terminal_starts.lock().unwrap().is_empty());
    }
    assert!(!std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(edit, ModelEdit::ChangeView { change: Change::Update { pane, .. }, .. } if pane.id == alias.id && pane.kind == alias.kind)), "an older live cache cannot publish over the observed end");
}

#[test]
fn stale_closed_managed_source_cannot_seed_close_its_current_canonical_successor() {
    use crate::{
        app::workspace_model_sync::ModelUpdate,
        chat::attachment_assets,
        model::BootId,
        persistence::{SnapshotBase, StateStore},
    };
    if isolated("stale_closed_managed_source_cannot_seed_close_its_current_canonical_successor") {
        return;
    }

    attachment_assets::with_test_image(|image| {
        let (mut app, active, hidden, stale, identity) = cached_chat();
        let original_kind = stale.kind.clone();
        let closed = super::super::super::project_ui::removed_existence("saved explicit close");
        app.card_state_mut(stale.id).unwrap().existence = closed.clone();
        let mut canonical = stale.clone();
        current_owner(&mut canonical);
        let healthy = sibling_chat(&canonical, "Healthy current sibling");
        install_held_pane(&mut app, hidden, &healthy);
        let CardKind::Chat(stale_source) = &stale.kind else {
            unreachable!()
        };
        let CardKind::Chat(canonical_source) = &canonical.kind else {
            unreachable!()
        };
        let CardKind::Chat(healthy_source) = &healthy.kind else {
            unreachable!()
        };
        assert_eq!(stale.id, canonical.id);
        assert_eq!(stale_source.id, canonical_source.id);
        assert_eq!(stale_source.launch, canonical_source.launch);
        assert_ne!(stale_source.proof, canonical_source.proof);
        assert!(stale_source.proof.as_ref().unwrap().is_complete());
        assert!(canonical_source.proof.as_ref().unwrap().is_complete());

        let draft = "Keep the closed source's complete draft café 🌿";
        let durable = Arc::make_mut(&mut app.durable);
        durable.chat_drafts.insert(stale.id, draft.into());
        durable
            .chat_draft_attachments
            .insert(stale.id, vec![image.clone()]);
        let assert_retained = |saved: &WorkspaceState, quarantined: bool| {
            let card = saved.cards.iter().find(|card| card.id == stale.id).unwrap();
            assert_eq!(card.kind, original_kind);
            assert_eq!(card.existence, closed);
            assert_eq!(card.title, stale.title);
            assert_eq!(card.width_fraction, stale.width_fraction);
            assert!(!saved.local_only_cards.contains(&stale.id));
            assert_eq!(saved.chat_drafts[&stale.id], draft);
            assert_eq!(saved.chat_draft_attachments[&stale.id], vec![image.clone()]);
            attachment_assets::validate(&saved.chat_draft_attachments[&stale.id][0]).unwrap();
            assert_eq!(
                saved.quarantined_panes,
                if quarantined {
                    HashMap::from([(stale.id, original_kind.clone())])
                } else {
                    HashMap::new()
                }
            );
            let sibling = saved
                .cards
                .iter()
                .find(|card| card.id == healthy.id)
                .unwrap();
            assert!(sibling.existence.is_open());
            assert_eq!(sibling.kind, healthy.kind);
        };
        assert_retained(&app.durable, false);
        assert!(app.durable.local_only_cards.is_empty());

        // Load actual saved CLOSED A and OPEN H before connecting. This is
        // stored payload, not a preinstalled quarantine or a local-only waiver.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stale-closed-source.sqlite3");
        let workspace = app.durable.id;
        let store = StateStore::open(&path, BootId::new("closed-source-save")).unwrap();
        let first_commit = store
            .save_workspace(
                &app.durable,
                SnapshotBase {
                    workspace: 0,
                    cards: &Default::default(),
                },
            )
            .unwrap();
        assert!(first_commit.refused.is_empty());
        drop(store);
        let store = StateStore::open(&path, BootId::new("closed-source-reload")).unwrap();
        app.durable = Arc::new(store.load_workspace(workspace, None).unwrap().unwrap());
        assert_retained(&app.durable, false);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.shared_views = State::for_workspace(&app.durable);
        app.chat_ui = chat_ui::State::with_draft_attachments(
            app.durable.chat_drafts.clone(),
            app.durable.chat_draft_attachments.clone(),
        );
        assert_eq!(
            app.workspace_model.identity,
            crate::workspace_model::NO_REGISTRY
        );
        assert_eq!(app.durable.workspace_model_identity, Some(identity));
        assert!(app.shared_views.quarantined.is_empty());
        assert!(app.durable.quarantined_panes.is_empty());
        assert!(app.durable.local_only_cards.is_empty());

        let mut view = View::empty(hidden);
        view.revision = 7;
        view.panes = vec![canonical.clone(), healthy.clone()];
        let mut full = model(identity, active, hidden, view);
        full.sequence = 10;
        full.agents = vec![
            managed_owner(&canonical, hidden),
            managed_owner(&healthy, hidden),
        ];
        assert!(full.agents.iter().all(|owner| !owner.closed));
        let canonical_issuer = full.agents[0].session.as_ref().unwrap();
        assert!(managed_issued_source_matches(
            canonical_issuer,
            &canonical.kind
        ));
        assert!(!managed_issued_source_matches(
            canonical_issuer,
            &stale.kind
        ));
        assert!(managed_issued_source_matches(
            full.agents[1].session.as_ref().unwrap(),
            &healthy.kind
        ));

        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        let daemon = ScriptedDaemon::with_owner(healthy_source.clone());
        let startup = app.initialize_runtimes();
        settle(&mut app, startup);
        assert!(app.chat_awaiting_admission(healthy.id));
        assert!(!app.cards.contains_key(&stale.id));
        assert!(app.shared_views.quarantined.is_empty());
        assert!(daemon.requests().is_empty());
        assert!(daemon.resolutions().is_empty());
        assert!(
            rx.try_recv().is_err(),
            "NO_REGISTRY cannot seed saved panes"
        );

        // B has the same CardId as stale CLOSED A and a valid OPEN issuer.
        // Connected must classify the raw saved source before its first seed.
        let connected = app.apply_workspace_model_update(ModelUpdate::Connected(full.clone()));
        settle(&mut app, connected);
        assert_eq!(app.workspace_model.views, full.views);
        assert_eq!(app.workspace_model.agents, full.agents);
        assert_eq!(app.shared_views.snapshot_sequence, 10);
        assert_eq!(app.shared_views.quarantined, HashSet::from([stale.id]));
        assert_eq!(
            app.durable.quarantined_panes,
            HashMap::from([(stale.id, original_kind.clone())])
        );
        let denial = app.shared_views.quarantine_actions.get(&stale.id).unwrap();
        assert_eq!(denial.source, original_kind);
        let denial_nonce = denial.nonce;
        assert_eq!(app.card_state(stale.id).unwrap().kind, original_kind);
        assert_eq!(app.card_state(stale.id).unwrap().existence, closed);

        let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        let assert_no_stale_edit = |edits: &[ModelEdit]| {
            assert!(
                !edits.iter().any(|edit| match edit {
                    ModelEdit::SeedView { panes, closed, .. } => {
                        panes.iter().any(|pane| pane.id == stale.id) || closed.contains(&stale.id)
                    }
                    ModelEdit::ChangeView { change, .. } => change_card(change) == Some(stale.id),
                    _ => false,
                }),
                "stale A must not publish B's close or any other view intent: {edits:?}"
            );
        };
        assert_no_stale_edit(&edits);
        let seeds = edits
            .iter()
            .filter_map(|edit| match edit {
                ModelEdit::SeedView {
                    scope,
                    panes,
                    closed,
                } => Some((scope.clone(), panes.clone(), closed.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        let hidden_seeds = seeds
            .iter()
            .filter(|(scope, _, _)| *scope == Scope::Channel(hidden))
            .collect::<Vec<_>>();
        assert_eq!(hidden_seeds.len(), 1, "one actual healthy seed: {edits:?}");
        assert_eq!(hidden_seeds[0].1, vec![healthy.clone()]);
        assert!(hidden_seeds[0].2.is_empty());
        assert!(app.shared_views.seeding.contains(&Scope::Channel(hidden)));
        assert!(
            !app.shared_views
                .initialized
                .contains(&Scope::Channel(hidden))
        );
        assert!(app.chat_awaiting_admission(healthy.id));
        assert!(daemon.requests().is_empty());

        // Only ACK scopes actually emitted by Connected; an invented None
        // ACK would bypass the exact admission boundary this parent owns.
        for (scope, panes, closed_ids) in seeds {
            let admits_healthy = scope == Scope::Channel(hidden);
            let still_held = !app
                .shared_views
                .initialized
                .contains(&Scope::Channel(hidden));
            if still_held {
                assert!(app.chat_awaiting_admission(healthy.id));
                assert!(daemon.requests().is_empty());
            }
            assert!(closed_ids.is_empty());
            if !admits_healthy {
                assert_eq!(scope, Scope::Channel(active));
                assert!(panes.is_empty());
            }
            let ack = app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope,
                operation: None,
                model: full.clone(),
            });
            settle(&mut app, ack);
            if !admits_healthy && still_held {
                assert!(app.chat_awaiting_admission(healthy.id));
                assert!(daemon.requests().is_empty());
            }
        }
        assert!(
            app.shared_views
                .initialized
                .contains(&Scope::Channel(hidden))
        );
        assert!(!app.shared_views.seeding.contains(&Scope::Channel(hidden)));
        assert!(!app.chat_awaiting_admission(healthy.id));
        assert!(!app.chat_start_pending(healthy.id));
        assert_eq!(daemon.requests(), vec![healthy_source.clone()]);
        assert!(daemon.resolutions().is_empty());
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&healthy.id) else {
            panic!("the scripted ChatAttached response must own H's actual runtime");
        };
        assert_eq!(runtime.session, *healthy_source);
        assert_eq!(runtime.restored_pane_ready(), Ok(true));
        let witness = app.shared_views.validated_managed.get(&healthy.id).unwrap();
        assert_eq!(witness.source, healthy.kind);
        assert_eq!(witness.registry, identity);
        assert_eq!(witness.scope, Scope::Channel(hidden));
        assert!(!app.shared_views.validated_managed.contains_key(&stale.id));
        assert!(!app.cards.contains_key(&stale.id));
        assert!(!app.chat_start_pending(stale.id));
        assert_eq!(app.shared_views.quarantined, HashSet::from([stale.id]));
        assert_eq!(
            app.shared_views.quarantine_actions[&stale.id].source,
            original_kind
        );
        assert_eq!(
            app.shared_views.quarantine_actions[&stale.id].nonce,
            denial_nonce
        );
        // Closed quarantine alone is not an OPEN pane recovery failure.
        assert_eq!(app.startup_pane_check(), Ok(true));
        assert_no_stale_edit(&std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>());
        assert!(
            !daemon
                .controls
                .lock()
                .unwrap()
                .iter()
                .any(|request| matches!(
                    request,
                    ClientRequest::ChatStart { .. }
                        | ClientRequest::ChatPrepareResume { .. }
                        | ClientRequest::ChatClose { .. }
                        | ClientRequest::ChatCloseRetained { .. }
                        | ClientRequest::CloseTerminalChecked { .. }
                        | ClientRequest::ChatRecoverAccount { .. }
                        | ClientRequest::ChatStartSelected { .. }
                        | ClientRequest::ChatReturnTerminal { .. }
                ))
        );

        // Captured Seed edits establish publication intent, not a Host close
        // effect. The separate daemon record proves only scripted IPC effects.
        let saved = app.persistent_workspace();
        assert_retained(&saved, true);
        let committed = store
            .save_workspace(
                &saved,
                SnapshotBase {
                    workspace: first_commit.generation,
                    cards: &first_commit.card_generations,
                },
            )
            .unwrap();
        assert!(committed.refused.is_empty());
        drop(store);
        let store = StateStore::open(&path, BootId::new("closed-source-evidence-reload")).unwrap();
        let restored = store.load_workspace(workspace, None).unwrap().unwrap();
        assert_retained(&restored, true);
        let history = store
            .saved_conversations()
            .unwrap()
            .into_iter()
            .find(|entry| entry.card == stale.id)
            .expect("closed stale source remains in genuine History storage");
        assert!(!history.open);
        assert!(history.quarantined);
        assert_eq!(history.session, *stale_source);
        assert_eq!(daemon.requests(), vec![healthy_source.clone()]);
    });
}

#[test]
fn exact_canonical_replacement_and_valid_closed_history_keep_their_admission_contract() {
    let (mut app, active, hidden, mut canonical, identity) = cached_chat();
    let saved = canonical.clone();
    current_owner(&mut canonical);
    let mut view = View::empty(hidden);
    view.panes.push(canonical.clone());
    let mut authoritative = model(identity, active, hidden, view);
    authoritative.agents.push(managed_owner(&canonical, hidden));
    app.workspace_model = authoritative;
    app.shared_views = State::for_workspace(&app.durable);
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    drop(app.reconcile_shared_views());
    assert!(
        app.shared_views.quarantined.is_empty(),
        "exact canonical proof may replace an old cached open source"
    );
    assert_eq!(
        app.card_state(saved.id).unwrap().kind,
        saved.kind,
        "seed preparation cannot overwrite recovery evidence"
    );
    let seeds = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(seeds.iter().any(
        |edit| matches!(edit, ModelEdit::SeedView { panes, .. } if panes.contains(&canonical))
    ));

    app.card_state_mut(saved.id).unwrap().kind = canonical.kind.clone();
    app.card_state_mut(saved.id).unwrap().existence =
        super::super::super::project_ui::removed_existence("explicit close");
    app.shared_views = State::for_workspace(&app.durable);
    drop(app.reconcile_shared_views());
    assert!(app.shared_views.quarantined.is_empty());
    let seeds = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        seeds.iter().any(
            |edit| matches!(edit, ModelEdit::SeedView { closed, .. } if closed.contains(&saved.id))
        ),
        "a valid explicit close still reaches the strict host"
    );
    app.acknowledge_view_edit(Scope::Channel(hidden), None);
    drop(app.reconcile_shared_views());
    assert!(
        !app.card_state(saved.id).unwrap().existence.is_open(),
        "old canonical membership cannot resurrect closed local history"
    );
}

fn focus_active_settings_pane(app: &mut Cagents, active: ChannelId) -> CardId {
    let pane = Pane {
        id: CardId::new(),
        title: "Active settings".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Settings,
    };
    install_held_pane(app, active, &pane);
    let durable = Arc::make_mut(&mut app.durable);
    durable.active_channel = Some(active);
    durable.focused = Some(pane.id);
    durable
        .channels
        .iter_mut()
        .find(|channel| channel.id == active)
        .unwrap()
        .focused = Some(pane.id);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    // rebuild_workspace is startup staging for every open durable pane. The
    // real startup scopes its strip before initializing runtimes; exercise the
    // production strip projection here before recording the active focus.
    drop(app.project_shared_strip(&[], &[pane.id]));
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![pane.id]);
    assert_eq!(app.workspace.focused(), Some(pane.id));
    pane.id
}

#[test]
fn hidden_quarantine_projection_preserves_the_active_scope_pane_and_focus() {
    use crate::app::workspace_model_sync::ModelUpdate;
    let (mut app, active, hidden, mut rejected, identity) = cached_chat();
    current_owner(&mut rejected);
    app.card_state_mut(rejected.id).unwrap().kind = rejected.kind.clone();
    let healthy = sibling_chat(&rejected, "Healthy hidden conversation");
    install_held_pane(&mut app, hidden, &healthy);
    let focused = focus_active_settings_pane(&mut app, active);
    let mut view = View::empty(hidden);
    view.panes = vec![rejected.clone(), healthy.clone()];
    let mut full = model(identity, active, hidden, view);
    let mut closed = managed_owner(&rejected, hidden);
    closed.closed = true;
    full.agents = vec![closed, managed_owner(&healthy, hidden)];
    app.workspace_model = full.clone();
    app.shared_views = State::for_workspace(&app.durable);
    app.shared_views
        .initialized
        .extend([Scope::Channel(active), Scope::Channel(hidden)]);
    // These tasks remain unexecuted in this source-only reducer fixture.
    // Actual healthy attachment/readiness is qualified by the User IPC tests.
    drop(app.reconcile_workspace_model());
    for _ in 0..2 {
        drop(app.apply_workspace_model_update(ModelUpdate::Read(full.clone())));
        assert_eq!(app.durable.active_channel, Some(active));
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![focused]);
        assert_eq!(app.workspace.focused(), Some(focused));
        assert_eq!(app.card_state(focused).unwrap().kind, CardKind::Settings);
        assert!(app.card_state(focused).unwrap().existence.is_open());
        assert_eq!(app.card_state(healthy.id).unwrap().kind, healthy.kind);
        assert!(matches!(
            app.cards.get(&rejected.id),
            Some(CardRuntime::Failed(_))
        ));
        assert!(app.terminal_starts.lock().unwrap().is_empty());
    }
}

fn mixed_managed_saved_scope(visible: bool) {
    use crate::app::workspace_model_sync::ModelUpdate;
    let (mut app, active, hidden, mut closed_owner, identity) = cached_chat();
    current_owner(&mut closed_owner);
    closed_owner.title = "Unavailable saved agent".into();
    closed_owner.width_fraction = 0.37;
    app.card_state_mut(closed_owner.id).unwrap().kind = closed_owner.kind.clone();
    app.card_state_mut(closed_owner.id).unwrap().title = closed_owner.title.clone();
    app.card_state_mut(closed_owner.id).unwrap().width_fraction = closed_owner.width_fraction;
    let stale = sibling_chat(&closed_owner, "Stale saved proof");
    let closed_history = sibling_chat(&closed_owner, "Closed saved history");
    let healthy = sibling_chat(&closed_owner, "Healthy saved conversation");
    for pane in [&stale, &closed_history, &healthy] {
        install_held_pane(&mut app, hidden, pane);
    }
    app.card_state_mut(closed_history.id).unwrap().existence =
        super::super::super::project_ui::removed_existence("previously closed");
    let CardKind::Chat(expected) = &healthy.kind else {
        unreachable!()
    };
    let daemon = ScriptedDaemon::with_owner(expected.clone());
    let mut closed_agent = managed_owner(&closed_owner, hidden);
    closed_agent.closed = true;
    let mut stale_agent = managed_owner(&stale, hidden);
    let crate::agent_execution::HostedSession::Chat(source) = stale_agent.session.as_mut().unwrap()
    else {
        unreachable!()
    };
    source.proof.as_mut().unwrap().host_incarnation = "replacement-proof".into();
    let mut history_agent = managed_owner(&closed_history, hidden);
    history_agent.closed = true;
    let mut view = View::empty(hidden);
    view.panes = vec![
        closed_owner.clone(),
        stale.clone(),
        closed_history.clone(),
        healthy.clone(),
    ];
    let mut authoritative = model(identity, active, hidden, view);
    authoritative.agents = vec![
        closed_agent,
        stale_agent,
        history_agent,
        managed_owner(&healthy, hidden),
    ];
    if visible {
        Arc::make_mut(&mut app.durable).active_channel = Some(hidden);
    }
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    let active_focus = (!visible).then(|| focus_active_settings_pane(&mut app, active));
    let original_focus = app.workspace.focused();
    if let Some(focused) = active_focus {
        assert_eq!(original_focus, Some(focused));
        assert!(![closed_owner.id, stale.id, healthy.id].contains(&focused));
    }
    let original_channel = app.durable.active_channel;
    app.shared_views = State::for_workspace(&app.durable);
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([
        (closed_owner.id, "Retained failed draft".into()),
        (stale.id, "Retained stale draft".into()),
    ]));
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    let startup = app.initialize_runtimes();
    settle(&mut app, startup);
    assert!(daemon.requests().is_empty());
    let connected = app.apply_workspace_model_update(ModelUpdate::Connected(authoritative.clone()));
    settle(&mut app, connected);
    let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    let Some(ModelEdit::SeedView { panes, closed, .. }) = edits.iter().find(|edit| matches!(edit, ModelEdit::SeedView { scope: Scope::Channel(id), .. } if *id == hidden)) else { panic!("healthy scope must reach SeedView") };
    assert_eq!(panes, &vec![healthy.clone()]);
    assert!(
        closed.is_empty(),
        "invalid closed history cannot become host removal authority"
    );
    assert_eq!(
        app.shared_views.quarantined,
        HashSet::from([closed_owner.id, stale.id, closed_history.id])
    );
    assert!(app.shared_views.blocked.is_empty());
    for channel in [active, hidden] {
        let ack = app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: Scope::Channel(channel),
            operation: None,
            model: authoritative.clone(),
        });
        settle(&mut app, ack);
    }
    assert_eq!(
        daemon.requests(),
        vec![expected.clone()],
        "only the healthy exact source attaches"
    );
    let Some(CardRuntime::Chat(healthy_runtime)) = app.cards.get(&healthy.id) else {
        panic!("healthy sibling must own a native chat runtime")
    };
    assert_eq!(healthy_runtime.session, *expected);
    assert_eq!(
        healthy_runtime.restored_pane_ready(),
        Ok(true),
        "the healthy sibling must actually recover, not merely dispatch an attach"
    );
    assert_eq!(app.durable.active_channel, original_channel);
    assert_eq!(
        app.workspace.focused(),
        original_focus,
        "recovery must not steal focus, including from another active scope"
    );
    assert_eq!(
        app.card_state(closed_owner.id).unwrap().kind,
        closed_owner.kind
    );
    assert_eq!(app.card_state(stale.id).unwrap().kind, stale.kind);
    assert!(
        !app.card_state(closed_history.id)
            .unwrap()
            .existence
            .is_open()
    );
    assert!(matches!(
        app.cards.get(&closed_owner.id),
        Some(CardRuntime::Failed(_))
    ));
    assert_eq!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == hidden)
            .unwrap()
            .cards,
        vec![closed_owner.id, stale.id, healthy.id]
    );
    if visible {
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            vec![closed_owner.id, stale.id, healthy.id]
        );
    }
    assert!(
        app.startup_pane_check().is_err(),
        "recovery ends with a pane failure instead of waiting forever"
    );
    let saved = app.persistent_workspace();
    assert_eq!(saved.chat_drafts[&closed_owner.id], "Retained failed draft");
    assert_eq!(saved.chat_drafts[&stale.id], "Retained stale draft");
    assert!(
        !saved.local_only_cards.contains(&closed_owner.id),
        "open quarantine never bypasses startup admission"
    );
    for message in [
        Message::RetryCard(closed_owner.id),
        Message::ChatUi(chat_ui::Action::StartFresh(stale.id)),
        Message::MovePaneToChannel {
            card: stale.id,
            channel: active,
        },
    ] {
        let task = app.update(message);
        settle(&mut app, task);
    }
    let delete = app.delete_channel(hidden);
    settle(&mut app, delete);
    assert!(!app.channel_ui.is_deleting(hidden));
    assert_eq!(daemon.requests(), vec![expected.clone()]);
    let edits = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        !edits.iter().any(|edit| match edit {
            ModelEdit::ChangeView { change, .. } => change_card(change).is_some_and(|card| [
                closed_owner.id,
                stale.id,
                closed_history.id
            ]
            .contains(&card)),
            ModelEdit::DeleteChannel { id } => *id == hidden,
            _ => false,
        }),
        "quarantine is absent from both baseline and desired publication"
    );
}

#[test]
fn active_mixed_managed_saved_scope_recovers_healthy_panes_only() {
    assert!(
        crate::agent_execution::Authority::from_environment()
            .token()
            .is_none(),
        "provider-effect qualification requires a genuine User invocation; inherited session authority must not be cleared"
    );
    if isolated("active_mixed_managed_saved_scope_recovers_healthy_panes_only") {
        return;
    }
    mixed_managed_saved_scope(true);
}

#[test]
fn hidden_mixed_managed_saved_scope_recovers_healthy_panes_only() {
    assert!(
        crate::agent_execution::Authority::from_environment()
            .token()
            .is_none(),
        "provider-effect qualification requires a genuine User invocation; inherited session authority must not be cleared"
    );
    if isolated("hidden_mixed_managed_saved_scope_recovers_healthy_panes_only") {
        return;
    }
    mixed_managed_saved_scope(false);
}

#[test]
fn saved_pane_check_results_survive_transport_recovery_and_follow_the_current_source() {
    assert!(
        std::env::var_os("CAGENTS_AGENT_AUTHORITY").is_none(),
        "This process-local health fixture requires a genuine User invocation before isolation"
    );
    if isolated("saved_pane_check_results_survive_transport_recovery_and_follow_the_current_source")
    {
        return;
    }
    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    current_owner(&mut pane);
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    let mut view = View::empty(hidden);
    view.panes.push(pane.clone());
    let mut owner = managed_owner(&pane, hidden);
    owner.closed = true;
    let mut invalid = model(identity, active, hidden, view);
    invalid.agents.push(owner);
    app.workspace_model = invalid.clone();
    drop(app.reconcile_shared_views());
    let key = NoticeKey::SavedPaneCheck(pane.id);
    let result = |app: &Cagents| {
        app.notices
            .recent()
            .find(|entry| entry.notice.key == Some(key))
            .map(|entry| entry.notice.clone())
    };

    // This is the only test in its private User child. Exercise the actual
    // selected-socket health monitor without a socket, provider or authority
    // replacement; an ordinary failure must really be suppressed first.
    let socket = crate::terminal::ipc::daemon_socket_path().unwrap();
    let failure = crate::control::ControlError::ConnectionClosed;
    let details = failure.to_string();
    crate::daemon_health::connection::<()>(&socket, &Err(failure), crate::daemon_health::begin());
    assert!(crate::daemon_health::current().unavailable());
    assert!(crate::daemon_health::replaces_notice(&details));
    app.status =
        Some(TransientNotice::warning("Ordinary transport failure").with_details(&details));
    drop(app.update(Message::ExpireNotices));
    assert!(
        !app.notices
            .recent()
            .any(|entry| entry.notice.message == "Ordinary transport failure")
    );
    drop(app.recheck_quarantined_pane(pane.id));
    let first = app.shared_views.rechecking[&pane.id].clone();
    drop(app.update(Message::QuarantinedPaneChecked {
        card: pane.id,
        check: first,
        result: Err(details.clone()),
    }));
    let notice = result(&app).expect("an explicit check must retain its own failed result");
    assert_eq!(notice.details.as_deref(), Some(details.as_str()));
    assert_eq!(
        notice.message,
        "Could not check this saved session. Its pane and drafts are retained."
    );
    app.notices
        .record(TransientNotice::success("Copied saved draft"));
    crate::daemon_health::connection(&socket, &Ok(()), crate::daemon_health::begin());
    drop(app.update(Message::DaemonHealth(
        crate::daemon_health::State::Responsive,
    )));
    assert_eq!(result(&app), Some(notice.clone()));
    assert!(app.pane_is_quarantined(pane.id));
    assert!(!app.chat_start_pending(pane.id));

    app.update_prepared = true;
    drop(app.recheck_quarantined_pane(pane.id));
    assert_eq!(result(&app), Some(notice));
    app.update_prepared = false;
    drop(app.recheck_quarantined_pane(pane.id));
    assert!(
        result(&app).is_none(),
        "only an accepted retry clears its previous result"
    );
    let second = app.shared_views.rechecking[&pane.id].clone();
    drop(app.update(Message::QuarantinedPaneChecked {
        card: pane.id,
        check: second.clone(),
        result: Ok(invalid.clone()),
    }));
    assert!(
        result(&app)
            .unwrap()
            .message
            .starts_with("This saved pane is still unverified.")
    );
    let mut fresh = invalid.clone();
    fresh.sequence += 1;
    fresh.agents[0].closed = false;
    app.refresh_managed_pane_quarantine(&fresh);
    assert!(!app.pane_is_quarantined(pane.id));
    assert!(
        result(&app).is_none(),
        "passive authoritative repair resolves the old result"
    );

    app.refresh_managed_pane_quarantine(&invalid);
    drop(app.recheck_quarantined_pane(pane.id));
    let replaced = app.shared_views.rechecking[&pane.id].clone();
    drop(app.update(Message::QuarantinedPaneChecked {
        card: pane.id,
        check: replaced.clone(),
        result: Err(details.clone()),
    }));
    assert!(result(&app).is_some());
    let CardKind::Chat(source) = &mut app.card_state_mut(pane.id).unwrap().kind else {
        unreachable!()
    };
    source.id = TerminalId::new();
    app.present_quarantined_pane(pane.id);
    assert!(
        result(&app).is_none(),
        "a replacement source cannot inherit the old result"
    );
    drop(app.update(Message::QuarantinedPaneChecked {
        card: pane.id,
        check: replaced,
        result: Err(details.clone()),
    }));
    assert!(
        result(&app).is_none(),
        "a duplicate prior completion cannot restore it"
    );

    drop(app.recheck_quarantined_pane(pane.id));
    let removed = app.shared_views.rechecking[&pane.id].clone();
    drop(app.update(Message::QuarantinedPaneChecked {
        card: pane.id,
        check: removed,
        result: Err(details),
    }));
    assert!(result(&app).is_some());
    let nonce = app.shared_views.quarantine_actions[&pane.id].nonce;
    drop(app.update(Message::RemoveQuarantinedPane(pane.id, nonce)));
    assert!(!app.card_state(pane.id).unwrap().existence.is_open());
    assert!(result(&app).is_none());
    assert!(app.notices.recent().any(|entry| entry.notice.key.is_none()
        && entry.notice.message
            == "Saved pane removed from this window. Its saved session and drafts remain stored."));
    assert!(
        app.notices
            .recent()
            .any(|entry| entry.notice.message == "Copied saved draft")
    );
}

#[test]
fn stale_pane_check_cannot_settle_a_successor_or_revive_changed_evidence() {
    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    current_owner(&mut pane);
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    let mut view = View::empty(hidden);
    view.panes.push(pane.clone());
    let mut owner = managed_owner(&pane, hidden);
    owner.closed = true;
    let mut invalid = model(identity, active, hidden, view);
    invalid.agents.push(owner);
    app.workspace_model = invalid.clone();
    drop(app.reconcile_shared_views());
    drop(app.recheck_quarantined_pane(pane.id));
    let first = app.shared_views.rechecking[&pane.id].clone();
    drop(app.recheck_quarantined_pane(pane.id));
    assert_eq!(app.shared_views.rechecking[&pane.id], first);
    let mut fresh = invalid.clone();
    fresh.sequence += 1;
    fresh.agents[0].closed = false;
    app.refresh_managed_pane_quarantine(&fresh);
    assert!(!app.shared_views.quarantined.contains(&pane.id));
    app.refresh_managed_pane_quarantine(&invalid);
    drop(app.recheck_quarantined_pane(pane.id));
    let second = app.shared_views.rechecking[&pane.id].clone();
    assert_ne!(first.request, second.request);
    drop(app.update(Message::QuarantinedPaneChecked {
        card: pane.id,
        check: first,
        result: Ok(fresh.clone()),
    }));
    assert_eq!(app.shared_views.rechecking[&pane.id], second);
    assert!(app.shared_views.quarantined.contains(&pane.id));
    assert_eq!(app.workspace_model.sequence, invalid.sequence);
    let mut wrong_registry = fresh.clone();
    wrong_registry.identity = uuid::Uuid::new_v4();
    drop(app.update(Message::QuarantinedPaneChecked {
        card: pane.id,
        check: second,
        result: Ok(wrong_registry),
    }));
    assert!(app.shared_views.quarantined.contains(&pane.id));

    drop(app.recheck_quarantined_pane(pane.id));
    let old_source = app.shared_views.rechecking[&pane.id].clone();
    let CardKind::Chat(changed) = &mut app.card_state_mut(pane.id).unwrap().kind else {
        unreachable!()
    };
    changed.id = TerminalId::new();
    drop(app.update(Message::QuarantinedPaneChecked {
        card: pane.id,
        check: old_source,
        result: Ok(fresh.clone()),
    }));
    assert!(app.shared_views.quarantined.contains(&pane.id));
    assert_eq!(
        app.workspace_model.sequence, invalid.sequence,
        "a replaced source cannot borrow the previous check's result"
    );
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();

    drop(app.recheck_quarantined_pane(pane.id));
    let mut different_endpoint = app.shared_views.rechecking[&pane.id].clone();
    different_endpoint.endpoint.0 = Some(("different-device".into(), "cagents".into()));
    app.shared_views
        .rechecking
        .insert(pane.id, different_endpoint.clone());
    drop(app.update(Message::QuarantinedPaneChecked {
        card: pane.id,
        check: different_endpoint,
        result: Ok(fresh.clone()),
    }));
    assert!(app.shared_views.quarantined.contains(&pane.id));
    assert_eq!(
        app.workspace_model.sequence, invalid.sequence,
        "another endpoint cannot install model evidence"
    );
    drop(app.recheck_quarantined_pane(pane.id));
    let exact = app.shared_views.rechecking[&pane.id].clone();
    drop(app.update(Message::QuarantinedPaneChecked {
        card: pane.id,
        check: exact,
        result: Ok(fresh),
    }));
    assert!(!app.shared_views.quarantined.contains(&pane.id));
    assert!(
        !matches!(app.cards.get(&pane.id), Some(CardRuntime::Failed(_))),
        "fresh exact validation clears the fixed failure and re-enters normal admission"
    );
}

#[test]
fn quarantined_chat_remove_routes_never_contact_or_close_the_inventory_owner() {
    assert!(
        crate::agent_execution::Authority::from_environment()
            .token()
            .is_none(),
        "This scripted IPC fixture requires a genuine User invocation before subprocess isolation; inherited caller authority must not be cleared"
    );
    if isolated("quarantined_chat_remove_routes_never_contact_or_close_the_inventory_owner") {
        return;
    }
    let (_, _, _, mut initial_pane, _) = cached_chat();
    let source = current_owner(&mut initial_pane);
    let daemon = ScriptedDaemon::with_owner(source);
    for retained in [false, true] {
        for route in [
            "chrome",
            "failed-button",
            "unasked",
            "commit",
            "chat-entry",
            "stale-repaired-button",
        ] {
            let (mut app, active, hidden, mut pane, identity) = cached_chat();
            current_owner(&mut pane);
            let CardKind::Chat(session) = &mut pane.kind else {
                unreachable!()
            };
            if retained {
                session.native_continuity =
                    crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
            }
            let source = session.clone();
            *daemon.inventory.lock().unwrap() = vec![source.clone()];
            app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
            Arc::make_mut(&mut app.durable).active_channel = Some(hidden);
            app.workspace = Cagents::rebuild_workspace(&app.durable);
            app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(
                pane.id,
                "Keep local removal payload".into(),
            )]));
            let mut view = View::empty(hidden);
            view.panes.push(pane.clone());
            let mut full = model(identity, active, hidden, view);
            let mut owner = managed_owner(&pane, hidden);
            owner.closed = true;
            full.agents.push(owner);
            app.workspace_model = full.clone();
            app.refresh_managed_pane_quarantine(&full);
            assert!(matches!(
                app.cards.get(&pane.id),
                Some(CardRuntime::Failed(_))
            ));
            let stale_repaired = route == "stale-repaired-button";
            let queued = stale_repaired.then(|| click_quarantined_remove(&app, pane.id));
            if stale_repaired {
                full.sequence += 1;
                full.agents[0].closed = false;
                app.refresh_managed_pane_quarantine(&full);
                app.workspace_model = full;
                assert!(!app.pane_is_quarantined(pane.id));
            }
            let socket = crate::terminal_ipc::daemon_socket_path().unwrap();
            let inventory = crate::control::ControlClient::connect(&socket)
                .unwrap()
                .list_chats()
                .unwrap();
            assert_eq!(
                inventory,
                vec![source.clone()],
                "the rejected identity is still a real inventory target"
            );
            let before = daemon.controls.lock().unwrap().len();
            let task = match route {
                "chrome" => app.update(Message::CloseCard(pane.id)),
                "failed-button" => {
                    let message = click_quarantined_remove(&app, pane.id);
                    app.update(message)
                }
                "unasked" => app.update(Message::CloseCardUnasked(pane.id)),
                "commit" => app.update(Message::CommitClose(pane.id)),
                "chat-entry" => app.begin_chat_close(pane.id),
                "stale-repaired-button" => app.update(queued.unwrap()),
                _ => unreachable!(),
            };
            settle(&mut app, task);
            assert_eq!(
                daemon.controls.lock().unwrap().len(),
                before,
                "{route}/{retained}: local presentation removal does not even query or attach inventory"
            );
            assert!(
                daemon.requests().is_empty(),
                "no provider start or attachment"
            );
            assert!(!app.chat_close_owned(pane.id));
            let saved = app.persistent_workspace();
            let history = saved.cards.iter().find(|card| card.id == pane.id).unwrap();
            assert_eq!(history.kind, pane.kind);
            assert_eq!(saved.chat_drafts[&pane.id], "Keep local removal payload");
            if stale_repaired {
                assert!(history.existence.is_open(), "{route}/{retained}");
                assert!(!saved.quarantined_panes.contains_key(&pane.id));
                assert!(app.workspace.order().any(|id| id == pane.id));
            } else {
                assert!(!history.existence.is_open(), "{route}/{retained}");
                assert_eq!(saved.quarantined_panes.get(&pane.id), Some(&pane.kind));
                assert!(!app.workspace.order().any(|id| id == pane.id));
            }
            assert_eq!(
                crate::control::ControlClient::connect(&socket)
                    .unwrap()
                    .list_chats()
                    .unwrap(),
                vec![source]
            );
        }
    }
    assert!(
        !daemon
            .controls
            .lock()
            .unwrap()
            .iter()
            .any(|request| matches!(
                request,
                ClientRequest::ChatClose { .. }
                    | ClientRequest::ChatCloseRetained { .. }
                    | ClientRequest::CloseTerminalChecked { .. }
            ))
    );
}

fn click_quarantined_remove(app: &Cagents, card: CardId) -> Message {
    use cosmic::iced::advanced::{
        Layout, Shell, clipboard, layout,
        widget::{Operation, Tree, operation::Focusable},
    };
    use cosmic::iced::{Event, Rectangle, Size, mouse};
    struct Bounds {
        id: widget::Id,
        bounds: Option<Rectangle>,
    }
    impl Operation for Bounds {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(&mut self, id: Option<&widget::Id>, bounds: Rectangle, _: &mut dyn Focusable) {
            if id == Some(&self.id) {
                self.bounds = Some(bounds);
            }
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    let mut element = app.quarantined_pane_view(card);
    let mut tree = Tree::new(element.as_widget());
    let size = Size::new(1200.0, 900.0);
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, size),
    );
    let mut bounds = Bounds {
        id: card_control_id(card, "quarantine.remove"),
        bounds: None,
    };
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut bounds);
    let point = bounds
        .bounds
        .expect("the actual failed view exposes its Remove saved pane control")
        .center();
    let mut messages = Vec::new();
    for event in [
        mouse::Event::ButtonPressed(mouse::Button::Left),
        mouse::Event::ButtonReleased(mouse::Button::Left),
    ] {
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(event),
            Layout::new(&node),
            mouse::Cursor::Available(point),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(size),
        );
    }
    assert_eq!(messages.len(), 1);
    let message = messages.pop().unwrap();
    assert!(
        matches!(&message, Message::RemoveQuarantinedPane(id, nonce) if *id == card && *nonce == app.shared_views.quarantine_actions[&card].nonce)
    );
    message
}

#[test]
fn queued_failed_remove_cannot_close_an_exactly_repaired_chat() {
    for retained in [false, true] {
        let (mut app, active, hidden, mut pane, identity) = cached_chat();
        current_owner(&mut pane);
        if retained {
            let CardKind::Chat(source) = &mut pane.kind else {
                unreachable!()
            };
            source.native_continuity =
                crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        }
        app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
        let mut view = View::empty(hidden);
        view.panes.push(pane.clone());
        let mut rejected = model(identity, active, hidden, view);
        let mut owner = managed_owner(&pane, hidden);
        owner.closed = true;
        rejected.agents.push(owner);
        app.workspace_model = rejected.clone();
        app.refresh_managed_pane_quarantine(&rejected);
        let queued = click_quarantined_remove(&app, pane.id);
        let mut repaired = rejected;
        repaired.sequence += 1;
        repaired.agents[0].closed = false;
        app.refresh_managed_pane_quarantine(&repaired);
        app.workspace_model = repaired;
        assert!(!app.pane_is_quarantined(pane.id));
        let Message::RemoveQuarantinedPane(card, nonce) = &queued else {
            unreachable!()
        };
        assert!(!app.quarantine_presentation_action_allowed(*card, *nonce));
        // The full App always batches its visibility/presence postlude, even
        // when the action is refused. Check the synchronous close/source
        // contract here without running hypothetical host work in this pure
        // fixture. The genuine-User isolated inventory fixture above drains
        // this actual stale-button task and verifies that it sends zero IPC.
        drop(app.update(queued));
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        assert!(!app.chat_close_owned(pane.id));
        assert!(app.terminal_starts.lock().unwrap().is_empty());
    }
}

#[test]
fn quarantined_pane_removal_restores_the_surviving_native_control() {
    assert!(
        std::env::var_os("CAGENTS_AGENT_AUTHORITY").is_none(),
        "This isolated widget fixture requires a genuine User invocation; inherited caller authority must not be cleared"
    );
    if isolated("quarantined_pane_removal_restores_the_surviving_native_control") {
        return;
    }
    for route in ["chrome", "failed-button", "background", "last"] {
        let (mut app, _, hidden, pane, _) = cached_chat();
        let healthy = CardId::new();
        let mut session = match &pane.kind {
            CardKind::Chat(session) => session.clone(),
            _ => unreachable!(),
        };
        session.id = TerminalId::new();
        session.launch.session_id = None;
        session.proof = None;
        session.terminal = None;
        session.owned_codex_listener = None;
        session.native_continuity = Default::default();
        assert!(session.is_unstarted());
        let durable = Arc::make_mut(&mut app.durable);
        durable.active_channel = Some(hidden);
        durable.focused = Some(if route == "background" {
            healthy
        } else {
            pane.id
        });
        let generation = durable.generation;
        if route != "last" {
            durable.cards.push(CardState {
                id: healthy,
                generation,
                title: "Healthy draft".into(),
                width_fraction: 0.5,
                kind: CardKind::Chat(session.clone()),
                existence: Existence::Open,
                exposure: Exposure::Visible,
                residency: Residency::Hot,
            });
        }
        let channel = durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == hidden)
            .unwrap();
        if route != "last" {
            channel.cards.insert(0, healthy);
        }
        channel.focused = durable.focused;
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.workspace.resize_viewport(1600.0);
        app.context_inspector_open = false;
        if route != "last" {
            // An unstarted draft supplies the real composer; its returned
            // skill-loading Task is not executed and no provider is started.
            drop(app.open_chat(healthy, session, crate::chat::Intent::Start));
        }
        app.shared_views.quarantined.insert(pane.id);
        app.present_quarantined_pane(pane.id);
        drop(app.sync_chrome());
        // Do not let setup's deferred focus conceal a missing close handoff.
        let _ = app.workspace.take_widget_focus_request();
        let task = if route == "failed-button" {
            let message = click_quarantined_remove(&app, pane.id);
            app.update(message)
        } else {
            app.update(Message::CloseCard(pane.id))
        };
        assert!(app.workspace.card(pane.id).is_none());
        assert!(!app.card_state(pane.id).unwrap().existence.is_open());
        let toolbar = widget::Id::new(TOOLBAR_FOCUS_ID);
        let (expected, task) = if route == "background" {
            // Closing a different pane must preserve a deliberate toolbar
            // choice instead of pulling the keyboard back to the composer.
            let initial = iced::widget::operation::focus(toolbar.clone()).map(cosmic::Action::App);
            (toolbar, initial.chain(task))
        } else if route == "last" {
            (toolbar, task)
        } else {
            (card_control_id(healthy, "chat-composer"), task)
        };
        assert_eq!(
            crate::app::tests::focused(&app, task),
            Some(expected),
            "{route} must hand the keyboard to the actual surviving widget"
        );
    }
}

#[test]
fn keyboard_saved_draft_copy_preserves_the_first_attachment_locations_click() {
    assert!(
        std::env::var_os("CAGENTS_AGENT_AUTHORITY").is_none(),
        "This isolated widget fixture requires a genuine User invocation; inherited caller authority must not be cleared"
    );
    if isolated("keyboard_saved_draft_copy_preserves_the_first_attachment_locations_click") {
        return;
    }
    use cosmic::iced::advanced::{
        clipboard,
        widget::{Operation, operation::Focusable},
    };
    use cosmic::iced::futures::StreamExt;
    use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
    use cosmic::iced::{Event, Rectangle, Size, keyboard, mouse};

    struct Bounds {
        id: widget::Id,
        bounds: Option<Rectangle>,
    }
    impl Operation for Bounds {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(&mut self, id: Option<&widget::Id>, bounds: Rectangle, _: &mut dyn Focusable) {
            if id == Some(&self.id) {
                self.bounds = Some(bounds);
            }
        }
    }
    fn assert_clipboard_write(task: Task<Message>, expected: &str) {
        let copied = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let mut effects = cosmic::iced::runtime::task::into_stream(task)
                    .expect("the actual copy callback produces clipboard work");
                tokio::time::timeout(Duration::from_secs(1), async {
                    while let Some(effect) = effects.next().await {
                        if let cosmic::iced::runtime::Action::Clipboard(
                            cosmic::iced::runtime::clipboard::Action::Write { contents, .. },
                        ) = effect
                        {
                            return contents;
                        }
                    }
                    panic!("the actual copy callback must emit a clipboard write");
                })
                .await
                .expect("saved payload copy must complete promptly")
            });
        assert_eq!(copied, expected);
    }

    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    current_owner(&mut pane);
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    let draft = "Keep this saved draft 🦀\nwith its complete second line.";
    let attachments = vec![
        crate::chat::Attachment {
            path: "/saved/first image.png".into(),
            reference: None,
        },
        crate::chat::Attachment {
            path: "/saved/second-image.png".into(),
            reference: None,
        },
    ];
    app.chat_ui = chat_ui::State::with_draft_attachments(
        HashMap::from([(pane.id, draft.into())]),
        HashMap::from([(pane.id, attachments.clone())]),
    );
    let healthy = CardId::new();
    app.cards.insert(healthy, CardRuntime::Settings);
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.push(CardState {
        id: healthy,
        generation: durable.generation,
        title: "Settings".into(),
        width_fraction: 0.5,
        kind: CardKind::Settings,
        existence: Existence::Open,
        exposure: Exposure::Visible,
        residency: Residency::Hot,
    });
    durable.active_channel = Some(hidden);
    durable.focused = Some(healthy);
    let channel = durable
        .channels
        .iter_mut()
        .find(|channel| channel.id == hidden)
        .unwrap();
    channel.cards.push(healthy);
    channel.focused = Some(healthy);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.workspace.resize_viewport(1600.0);
    app.context_inspector_open = false;
    let mut view = View::empty(hidden);
    view.panes.push(pane.clone());
    let mut rejected = model(identity, active, hidden, view);
    let mut owner = managed_owner(&pane, hidden);
    owner.closed = true;
    rejected.agents.push(owner);
    app.workspace_model = rejected.clone();
    app.refresh_managed_pane_quarantine(&rejected);
    drop(app.sync_chrome());
    assert!(matches!(
        app.cards.get(&pane.id),
        Some(CardRuntime::Failed(_))
    ));
    let nonce = app.shared_views.quarantine_actions[&pane.id].nonce;
    let mut cache = Cache::new();
    let mut renderer = crate::app::tests::headless_renderer();
    // Retain the real workspace, CardSurface and button state through each
    // reducer/rerender. A fresh tree would hide an Enter-created pressed latch.
    let mut frame = |app: &Cagents, control: &str, focus: bool, event: Event| {
        let mut ui = UserInterface::build(
            cosmic::Application::view(app),
            Size::new(app.window_width, app.window_height),
            std::mem::replace(&mut cache, Cache::new()),
            &mut renderer,
        );
        let id = card_control_id(pane.id, control);
        if focus {
            ui.operate(
                &renderer,
                &mut cosmic::iced::advanced::widget::operation::focusable::focus(id.clone()),
            );
        }
        let mut bounds = Bounds { id, bounds: None };
        ui.operate(&renderer, &mut bounds);
        let point = bounds
            .bounds
            .expect("actual saved-payload control")
            .center();
        let mut messages = Vec::new();
        let _ = ui.update(
            &[event],
            mouse::Cursor::Available(point),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        cache = ui.into_cache();
        messages
    };
    let mut messages = frame(
        &app,
        "quarantine.copy",
        true,
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        }),
    );
    assert_eq!(messages.len(), 1, "Enter copies the saved draft once");
    assert!(matches!(
        &messages[0],
        Message::CopyQuarantinedDraft(card, action) if *card == pane.id && *action == nonce
    ));
    assert_clipboard_write(app.update(messages.pop().unwrap()), draft);
    assert!(
        frame(
            &app,
            "quarantine.copy",
            false,
            Event::Keyboard(keyboard::Event::KeyReleased {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
            }),
        )
        .is_empty(),
        "releasing Enter must not copy again"
    );
    rejected.sequence += 1;
    app.workspace_model = rejected.clone();
    app.refresh_managed_pane_quarantine(&rejected);
    assert_eq!(app.workspace.focused(), Some(healthy));
    let mut messages = frame(
        &app,
        "quarantine.attachments",
        false,
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
    );
    assert_eq!(
        messages.len(),
        1,
        "the first press activates its actual pane"
    );
    assert!(matches!(
        &messages[0],
        Message::Viewport { generation, event: WorkspaceViewportEvent::PointerFocused(card) }
            if *generation == app.workspace_view_generation && *card == pane.id
    ));
    // Exercise the real pane-activation reducer before release. Its runtime
    // scroll effects are outside this headless interaction regression.
    drop(app.update(messages.pop().unwrap()));
    assert_eq!(app.workspace.focused(), Some(pane.id));
    let mut messages = frame(
        &app,
        "quarantine.attachments",
        false,
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
    );
    assert_eq!(
        messages.len(),
        1,
        "the first click must copy the saved locations"
    );
    assert!(matches!(
        &messages[0],
        Message::CopyQuarantinedAttachmentPaths(card, action) if *card == pane.id && *action == nonce
    ));
    assert_clipboard_write(
        app.update(messages.pop().unwrap()),
        "/saved/first image.png\n/saved/second-image.png",
    );
    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
    assert_eq!(app.quarantined_pane_draft(pane.id), Some(draft));
    assert_eq!(
        app.persistent_workspace().chat_draft_attachments[&pane.id],
        attachments
    );
    assert_eq!(app.shared_views.quarantine_actions[&pane.id].nonce, nonce);
}

#[test]
fn held_failed_remove_press_cannot_migrate_to_a_successor_quarantine_presentation() {
    use cosmic::iced::advanced::{
        clipboard,
        widget::{Operation, operation::Focusable},
    };
    use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
    use cosmic::iced::{Event, Rectangle, Size, mouse};
    struct Bounds {
        id: widget::Id,
        bounds: Option<Rectangle>,
    }
    impl Operation for Bounds {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(&mut self, id: Option<&widget::Id>, bounds: Rectangle, _: &mut dyn Focusable) {
            if id == Some(&self.id) {
                self.bounds = Some(bounds);
            }
        }
    }
    for replacement in ["source", "presentation"] {
        let (mut app, _, _, pane, _) = cached_chat();
        app.shared_views.quarantined.insert(pane.id);
        app.present_quarantined_pane(pane.id);
        let original = app.shared_views.quarantine_actions[&pane.id].nonce;
        let mut cache = Cache::new();
        let mut renderer = crate::app::tests::headless_renderer();
        // The runtime cache extracts and restores the actual named button's
        // native state across frames, including a held mouse press.
        let mut frame = |app: &Cagents, event| {
            let mut ui = UserInterface::build(
                app.quarantined_pane_view(pane.id),
                Size::new(1200.0, 900.0),
                std::mem::replace(&mut cache, Cache::new()),
                &mut renderer,
            );
            let mut bounds = Bounds {
                id: card_control_id(pane.id, "quarantine.remove"),
                bounds: None,
            };
            ui.operate(&renderer, &mut bounds);
            let point = bounds.bounds.expect("actual failed Remove button").center();
            let mut messages = Vec::new();
            let _ = ui.update(
                &[Event::Mouse(event)],
                mouse::Cursor::Available(point),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            cache = ui.into_cache();
            messages
        };
        assert!(frame(&app, mouse::Event::ButtonPressed(mouse::Button::Left)).is_empty());
        if replacement == "source" {
            let CardKind::Chat(source) = &mut app.card_state_mut(pane.id).unwrap().kind else {
                unreachable!()
            };
            source.id = TerminalId::new();
        } else {
            app.shared_views.quarantine_actions.remove(&pane.id);
        }
        app.present_quarantined_pane(pane.id);
        let successor = app.shared_views.quarantine_actions[&pane.id].nonce;
        assert_ne!(successor, original);
        assert!(
            frame(&app, mouse::Event::ButtonReleased(mouse::Button::Left)).is_empty(),
            "a held {replacement} press must not remove the replacement saved pane"
        );
        assert!(app.card_state(pane.id).unwrap().existence.is_open());
        assert!(frame(&app, mouse::Event::ButtonPressed(mouse::Button::Left)).is_empty());
        let messages = frame(&app, mouse::Event::ButtonReleased(mouse::Button::Left));
        assert_eq!(messages.len(), 1, "a fresh press remains usable");
        assert!(matches!(
            &messages[0],
            Message::RemoveQuarantinedPane(card, nonce) if *card == pane.id && *nonce == successor
        ));
    }
}

#[test]
fn local_removal_preserves_closed_history_and_draft_receipts_across_reload() {
    use crate::persistence::{SnapshotBase, StateStore};
    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    current_owner(&mut pane);
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|channel| channel.id == hidden)
        .unwrap()
        .focused = Some(pane.id);
    Arc::make_mut(&mut app.durable)
        .card_scopes
        .insert(pane.id, "/fixture-project".into());
    let attachment = crate::chat::Attachment {
        path: "/retained/saved-image.png".into(),
        reference: Some(crate::chat::ImageReference {
            id: uuid::Uuid::new_v4(),
            number: 1,
            device: uuid::Uuid::new_v4(),
            name: "retained-image.png".into(),
            sha256: "0".repeat(64),
            extension: "png".into(),
        }),
    };
    app.chat_ui = chat_ui::State::with_draft_attachments(
        HashMap::from([(pane.id, "Saved payload before quarantine".into())]),
        HashMap::from([(pane.id, vec![attachment.clone()])]),
    );
    let mut view = View::empty(hidden);
    view.panes.push(pane.clone());
    let mut authoritative = model(identity, active, hidden, view);
    let mut owner = managed_owner(&pane, hidden);
    owner.closed = true;
    authoritative.agents.push(owner);
    app.workspace_model = authoritative;
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    drop(app.initialize_runtimes());
    drop(app.reconcile_shared_views());
    assert_eq!(
        app.quarantined_pane_draft(pane.id),
        Some("Saved payload before quarantine")
    );
    assert_eq!(app.quarantined_pane_attachment_count(pane.id), 1);
    while rx.try_recv().is_ok() {}
    drop(app.begin_card_close(pane.id));
    let saved = app.persistent_workspace();
    let closed = saved.cards.iter().find(|card| card.id == pane.id).unwrap();
    assert!(!closed.existence.is_open());
    assert_eq!(closed.kind, pane.kind);
    assert_eq!(
        saved.card_scopes[&pane.id],
        PathBuf::from("/fixture-project")
    );
    assert!(saved.local_only_cards.contains(&pane.id));
    assert_eq!(
        saved
            .channels
            .iter()
            .find(|channel| channel.id == hidden)
            .unwrap()
            .focused,
        None,
        "removing a hidden focused pane must leave a savable channel"
    );
    assert_eq!(
        saved.chat_drafts[&pane.id],
        "Saved payload before quarantine"
    );
    assert_eq!(
        saved.chat_draft_attachments[&pane.id],
        vec![attachment.clone()]
    );
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| match edit {
            ModelEdit::SeedView { closed, panes, .. } =>
                closed.contains(&pane.id) || panes.iter().any(|source| source.id == pane.id),
            ModelEdit::ChangeView { change, .. } => change_card(&change) == Some(pane.id),
            _ => false,
        })
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("quarantined-history.sqlite3");
    let store = StateStore::open(&path, crate::model::BootId::new("saved-history")).unwrap();
    store
        .save_workspace(
            &saved,
            SnapshotBase {
                workspace: 0,
                cards: &Default::default(),
            },
        )
        .unwrap();
    drop(store);
    let store = StateStore::open(&path, crate::model::BootId::new("saved-history-reload")).unwrap();
    let restored = store.load_workspace(saved.id, None).unwrap().unwrap();
    assert!(restored.local_only_cards.contains(&pane.id));
    assert_eq!(
        restored.chat_drafts[&pane.id],
        "Saved payload before quarantine"
    );
    assert_eq!(restored.chat_draft_attachments[&pane.id], vec![attachment]);
    let history = store
        .saved_conversations()
        .unwrap()
        .into_iter()
        .find(|entry| entry.card == pane.id)
        .expect("removed local pane remains selectable in real History storage");
    assert!(!history.open);
    assert_eq!(
        history.session,
        match &pane.kind {
            CardKind::Chat(session) => session.clone(),
            _ => unreachable!(),
        }
    );
    app.durable = Arc::new(restored);
    app.shared_views = State::for_workspace(&app.durable);
    app.chat_ui = chat_ui::State::with_draft_attachments(
        app.durable.chat_drafts.clone(),
        app.durable.chat_draft_attachments.clone(),
    );
    let snapshot = app.persistent_workspace();
    assert_eq!(
        snapshot.chat_draft_attachments[&pane.id].len(),
        1,
        "closed receipt survives the first save after restart"
    );
    drop(app.reconcile_shared_views());
    assert!(!app.card_state(pane.id).unwrap().existence.is_open());

    let mut replacement = app.workspace_model.clone();
    replacement.identity = uuid::Uuid::new_v4();
    replacement.sequence = 0;
    drop(app.apply_workspace_model_update(
        crate::app::workspace_model_sync::ModelUpdate::Connected(replacement),
    ));
    let replacement_saved = app.persistent_workspace();
    assert!(!app.card_state(pane.id).unwrap().existence.is_open());
    assert!(replacement_saved.local_only_cards.contains(&pane.id));
    assert_eq!(
        replacement_saved.chat_drafts[&pane.id],
        "Saved payload before quarantine"
    );
    assert_eq!(
        replacement_saved.chat_draft_attachments[&pane.id].len(),
        1,
        "registry replacement cannot strip dismissed history payload"
    );
}

#[test]
fn a_missing_shared_channel_keeps_quarantined_evidence_without_open_local_only_admission() {
    use crate::app::workspace_model_sync::ModelUpdate;
    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    current_owner(&mut pane);
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(
        pane.id,
        "Draft survives missing channel".into(),
    )]));
    let mut incoming = model(identity, active, hidden, View::empty(hidden));
    incoming.channels.retain(|channel| channel.id != hidden);
    incoming
        .views
        .retain(|view| view.scope != Scope::Channel(hidden));
    let mut owner = managed_owner(&pane, hidden);
    owner.closed = true;
    incoming.agents.push(owner);
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(incoming)));
    assert!(
        !app.durable
            .channels
            .iter()
            .any(|channel| channel.id == hidden)
    );
    assert!(app.shared_views.quarantined.contains(&pane.id));
    assert!(matches!(
        app.cards.get(&pane.id),
        Some(CardRuntime::Failed(_))
    ));
    assert!(app.card_state(pane.id).unwrap().existence.is_open());
    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
    let saved = app.persistent_workspace();
    assert_eq!(
        saved.chat_drafts[&pane.id],
        "Draft survives missing channel"
    );
    assert_eq!(
        saved.card_scopes[&pane.id],
        PathBuf::from("/fixture-project")
    );
    assert!(
        !saved.local_only_cards.contains(&pane.id),
        "inert open evidence cannot acquire the local-only startup exemption"
    );
    let restored_admission = State::for_workspace(&saved);
    assert!(
        restored_admission.attached.contains(&pane.id),
        "the next window must still wait for exact authority"
    );
}

#[test]
fn a_wrong_proof_alias_is_preserved_instead_of_retired_by_uuid_equality() {
    let (mut app, active, hidden, mut canonical, identity) = cached_chat();
    let alias = CardId::new();
    let original = app.card_state(canonical.id).unwrap().kind.clone();
    app.card_state_mut(canonical.id).unwrap().id = alias;
    let durable = Arc::make_mut(&mut app.durable);
    durable
        .channels
        .iter_mut()
        .find(|channel| channel.id == hidden)
        .unwrap()
        .cards = vec![alias];
    current_owner(&mut canonical);
    let mut view = View::empty(hidden);
    view.panes.push(canonical.clone());
    let mut authoritative = model(identity, active, hidden, view);
    authoritative.agents.push(managed_owner(&canonical, hidden));
    app.workspace_model = authoritative;
    app.shared_views = State::for_workspace(&app.durable);
    app.shared_views
        .initialized
        .extend([Scope::Channel(active), Scope::Channel(hidden)]);
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(
        alias,
        "Alias draft must stay with its proof".into(),
    )]));
    drop(app.reconcile_shared_views());
    assert!(app.shared_views.quarantined.contains(&alias));
    assert!(!app.shared_views.quarantined.contains(&canonical.id));
    assert!(app.shared_views.blocked.is_empty());
    assert!(app.card_state(alias).unwrap().existence.is_open());
    assert_eq!(app.card_state(alias).unwrap().kind, original);
    assert_eq!(
        app.persistent_workspace().chat_drafts[&alias],
        "Alias draft must stay with its proof"
    );
    assert!(!app.chat_start_pending(alias));
    assert!(app.card_state(canonical.id).unwrap().existence.is_open());
}

#[test]
fn unchanged_quarantine_refresh_preserves_the_durable_evidence_allocation() {
    let (mut app, active, hidden, pane, identity) = cached_chat();
    Arc::make_mut(&mut app.durable)
        .quarantined_panes
        .insert(pane.id, pane.kind.clone());
    app.shared_views = State::for_workspace(&app.durable);
    let snapshot = model(identity, active, hidden, View::empty(hidden));
    let original = Arc::clone(&app.durable);
    app.refresh_managed_pane_quarantine(&snapshot);
    app.refresh_managed_pane_quarantine(&snapshot);
    assert!(Arc::ptr_eq(&original, &app.durable));
    assert_eq!(
        app.durable.quarantined_panes.get(&pane.id),
        Some(&pane.kind)
    );
    assert!(app.shared_views.quarantined.contains(&pane.id));
    assert!(matches!(
        app.cards.get(&pane.id),
        Some(CardRuntime::Failed(_))
    ));
}

#[test]
fn retained_chat_tombstone_still_records_its_exact_closed_source() {
    let (mut app, active, hidden, pane, identity) = cached_chat();
    let CardKind::Chat(source) = &pane.kind else {
        unreachable!()
    };
    Arc::make_mut(&mut app.durable)
        .local_only_cards
        .insert(pane.id);
    app.shared_views = State::for_workspace(&app.durable);
    app.shared_views
        .initialized
        .extend([Scope::Channel(active), Scope::Channel(hidden)]);
    app.shared_views
        .observed_chats
        .insert(pane.id, source.clone());
    app.chat_ui =
        chat_ui::State::with_drafts(HashMap::from([(pane.id, "Retained closed draft".into())]));
    app.cards.insert(
        pane.id,
        CardRuntime::Failed("Fixture detached saved reader".into()),
    );
    let mut retired = View::empty(hidden);
    retired.retired.push(pane.id);
    app.workspace_model = model(identity, active, hidden, retired);
    drop(app.reconcile_shared_views());
    assert_eq!(app.shared_views.closed_chats.get(&pane.id), Some(source));
    assert!(
        matches!(app.card_state(pane.id).map(|card| &card.kind), Some(CardKind::Chat(current))
            if chat_ui::closed_draft_source_matches(source, current))
            && !app
                .workspace_model
                .views
                .iter()
                .any(|view| view.panes.iter().any(|shared| shared.id == pane.id)),
        "retained visibility cannot suppress authoritative chat retirement bookkeeping"
    );
    assert!(app.card_state(pane.id).unwrap().existence.is_open());
    assert_eq!(
        app.persistent_workspace().chat_drafts[&pane.id],
        "Retained closed draft"
    );
    assert!(!app.chat_start_pending(pane.id));
}

#[test]
fn rejected_identity_cannot_borrow_another_issuers_exact_source_but_accepts_a_canonical_upgrade() {
    let (mut app, active, hidden, mut rejected, identity) = cached_chat();
    current_owner(&mut rejected);
    app.card_state_mut(rejected.id).unwrap().kind = rejected.kind.clone();
    Arc::make_mut(&mut app.durable)
        .quarantined_panes
        .insert(rejected.id, rejected.kind.clone());
    let healthy = sibling_chat(&rejected, "Another issuer's valid source");
    let mut view = View::empty(hidden);
    view.panes.push(healthy.clone());
    let mut incoming = model(identity, active, hidden, view);
    incoming.agents.push(managed_owner(&healthy, hidden));
    // Simulate a same-ID local descriptor replacement. Its capability really
    // belongs to B, while the complete model admits no descriptor for A.
    app.card_state_mut(rejected.id).unwrap().kind = healthy.kind.clone();
    app.shared_views = State::for_workspace(&app.durable);
    app.shared_views
        .initialized
        .extend([Scope::Channel(active), Scope::Channel(hidden)]);
    app.workspace_model = incoming.clone();
    drop(app.reconcile_shared_views());
    assert!(app.shared_views.quarantined.contains(&rejected.id));
    assert_eq!(
        app.durable.quarantined_panes.get(&rejected.id),
        Some(&rejected.kind)
    );
    assert!(matches!(
        app.cards.get(&rejected.id),
        Some(CardRuntime::Failed(_))
    ));
    assert!(
        !app.chat_start_pending(rejected.id),
        "A cannot attach or resume by borrowing B's otherwise valid proof"
    );

    let mut upgraded = rejected.clone();
    let CardKind::Chat(session) = &mut upgraded.kind else {
        unreachable!()
    };
    session.proof.as_mut().unwrap().host_incarnation = "verified-canonical-upgrade".into();
    incoming.sequence += 1;
    incoming.views[1].panes.push(upgraded.clone());
    incoming.agents.push(managed_owner(&upgraded, hidden));
    app.refresh_managed_pane_quarantine(&incoming);
    assert!(
        !app.shared_views.quarantined.contains(&rejected.id),
        "the unique exact canonical update can re-enter ordinary source admission"
    );
    assert!(!app.durable.quarantined_panes.contains_key(&rejected.id));
    assert_eq!(
        app.card_state(rejected.id).unwrap().kind,
        healthy.kind,
        "classification leaves source replacement and draft preservation to ordinary admission"
    );
}

#[test]
fn persisted_open_quarantine_survives_restart_initialization_and_a_registry_without_its_issuer() {
    use crate::app::workspace_model_sync::ModelUpdate;
    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    current_owner(&mut pane);
    let CardKind::Chat(session) = &mut pane.kind else {
        unreachable!()
    };
    session.launch.provider = crate::agent::Provider::Codex;
    session.launch.recipe.program = "codex".into();
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(
        pane.id,
        "Keep rejected Codex draft".into(),
    )]));
    let mut view = View::empty(hidden);
    view.panes.push(pane.clone());
    let mut rejected = model(identity, active, hidden, view.clone());
    let mut issuer = managed_owner(&pane, hidden);
    issuer.closed = true;
    rejected.agents.push(issuer);
    app.workspace_model = rejected;
    drop(app.reconcile_shared_views());
    let saved = app.persistent_workspace();
    assert_eq!(saved.quarantined_panes.get(&pane.id), Some(&pane.kind));
    assert!(!saved.local_only_cards.contains(&pane.id));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("open-quarantine.sqlite3");
    let store =
        crate::persistence::StateStore::open(&path, crate::model::BootId::new("before-reload"))
            .unwrap();
    store
        .save_workspace(
            &saved,
            crate::persistence::SnapshotBase {
                workspace: 0,
                cards: &Default::default(),
            },
        )
        .unwrap();
    drop(store);
    let store =
        crate::persistence::StateStore::open(&path, crate::model::BootId::new("after-reload"))
            .unwrap();
    let restored = store.load_workspace(saved.id, None).unwrap().unwrap();
    assert_eq!(restored.quarantined_panes.get(&pane.id), Some(&pane.kind));
    app.durable = Arc::new(restored);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.shared_views = State::for_workspace(&app.durable);
    app.chat_ui = chat_ui::State::with_draft_attachments(
        app.durable.chat_drafts.clone(),
        app.durable.chat_draft_attachments.clone(),
    );
    app.workspace_model = Default::default();
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    drop(app.initialize_runtimes());
    assert!(
        matches!(app.cards.get(&pane.id), Some(CardRuntime::Failed(_))),
        "persisted markers must paint an inert failure during initialization"
    );
    assert!(!app.chat_start_pending(pane.id));
    let replacement = model(uuid::Uuid::new_v4(), active, hidden, view);
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(replacement.clone())));
    for channel in [active, hidden] {
        drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: Scope::Channel(channel),
            operation: None,
            model: replacement.clone(),
        }));
    }
    assert!(app.shared_views.quarantined.contains(&pane.id));
    assert!(matches!(
        app.cards.get(&pane.id),
        Some(CardRuntime::Failed(_))
    ));
    assert!(
        !app.chat_start_pending(pane.id),
        "issuer absence cannot authorize a resumable provider start after SeedView admission"
    );
    assert_eq!(
        app.persistent_workspace().chat_drafts[&pane.id],
        "Keep rejected Codex draft"
    );
    assert!(!std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(edit,
        ModelEdit::SeedView { panes, closed, .. } if panes.iter().any(|source| source.id == pane.id) || closed.contains(&pane.id))));

    // Changing the same-ID local source is also not proof that the original
    // rejection has gone away. Even the local-only startup exception must wait.
    let original = pane.kind.clone();
    let CardKind::Chat(changed) = &mut app.card_state_mut(pane.id).unwrap().kind else {
        unreachable!()
    };
    changed.id = TerminalId::new();
    Arc::make_mut(&mut app.durable)
        .local_only_cards
        .insert(pane.id);
    app.shared_views = State::for_workspace(&app.durable);
    drop(app.initialize_runtimes());
    drop(app.reconcile_shared_views());
    assert!(app.shared_views.quarantined.contains(&pane.id));
    assert!(matches!(
        app.cards.get(&pane.id),
        Some(CardRuntime::Failed(_))
    ));
    assert!(!app.chat_start_pending(pane.id));
    assert_eq!(
        app.durable.quarantined_panes.get(&pane.id),
        Some(&original),
        "source mismatch preserves the original rejected evidence"
    );

    Arc::make_mut(&mut app.durable)
        .local_only_cards
        .remove(&pane.id);
    app.card_state_mut(pane.id).unwrap().kind = original;
    let mut verified = replacement;
    verified.sequence += 1;
    verified.agents.push(managed_owner(&pane, hidden));
    drop(app.apply_workspace_model_update(ModelUpdate::Read(verified)));
    assert!(!app.shared_views.quarantined.contains(&pane.id));
    assert!(
        !app.durable.quarantined_panes.contains_key(&pane.id),
        "only exact authoritative revalidation clears the durable rejection"
    );
}

#[test]
fn removing_an_earlier_clean_pane_keeps_quarantine_before_its_healthy_anchor() {
    for removed_first in [false, true] {
        let (mut app, active, hidden, mut rejected, identity) = cached_chat();
        current_owner(&mut rejected);
        app.card_state_mut(rejected.id).unwrap().kind = rejected.kind.clone();
        let (_, _, _, browser) = fixture();
        let mut healthy = browser.clone();
        healthy.id = CardId::new();
        let mut removed = browser;
        removed.id = CardId::new();
        if let CardKind::Browser(session) = &mut removed.kind {
            session.id = crate::model::BrowserId::new();
        }
        install_held_pane(&mut app, hidden, &healthy);
        install_held_pane(&mut app, hidden, &removed);
        let durable = Arc::make_mut(&mut app.durable);
        durable.active_channel = Some(hidden);
        durable.local_only_cards.insert(removed.id);
        durable
            .channels
            .iter_mut()
            .find(|row| row.id == hidden)
            .unwrap()
            .cards = if removed_first {
            vec![removed.id, rejected.id, healthy.id]
        } else {
            vec![rejected.id, removed.id, healthy.id]
        };
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.shared_views = State::for_workspace(&app.durable);
        app.shared_views
            .initialized
            .extend([Scope::Channel(active), Scope::Channel(hidden)]);
        let mut view = View::empty(hidden);
        view.panes = vec![rejected.clone(), healthy.clone()];
        view.retired.push(removed.id);
        let mut full = model(identity, active, hidden, view);
        let mut owner = managed_owner(&rejected, hidden);
        owner.closed = true;
        full.agents.push(owner);
        app.workspace_model = full;
        for _ in 0..2 {
            drop(app.reconcile_shared_views());
            assert!(!app.card_state(removed.id).unwrap().existence.is_open());
            assert_eq!(
                app.durable
                    .channels
                    .iter()
                    .find(|row| row.id == hidden)
                    .unwrap()
                    .cards,
                vec![rejected.id, healthy.id]
            );
            assert_eq!(
                app.workspace.order().collect::<Vec<_>>(),
                vec![rejected.id, healthy.id]
            );
            assert_eq!(app.card_state(rejected.id).unwrap().kind, rejected.kind);
            assert_eq!(app.card_state(healthy.id).unwrap().kind, healthy.kind);
        }
    }
}

#[test]
fn quarantined_retained_cards_do_not_duplicate_or_replace_a_retained_editor() {
    for utility in [false, true] {
        retained_order_fixture(utility);
    }
}

fn retained_order_fixture(include_utility: bool) {
    let (mut app, active, hidden, mut rejected, identity) = cached_chat();
    current_owner(&mut rejected);
    app.card_state_mut(rejected.id).unwrap().kind = rejected.kind.clone();
    let mut healthy = sibling_chat(&rejected, "Healthy descriptor");
    install_held_pane(&mut app, hidden, &healthy);
    let editor = CardId::new();
    let project = app
        .durable
        .channels
        .iter()
        .find(|channel| channel.id == hidden)
        .unwrap()
        .project_id;
    let editor_kind = CardKind::Repository(crate::model::RepositoryView {
        project,
        repository: "/repo".into(),
        worktree: "/repo".into(),
        selected_file: Some("keep-editor.rs".into()),
    });
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.push(CardState {
        id: editor,
        generation: durable.generation,
        title: "Retained editor".into(),
        width_fraction: 0.62,
        kind: editor_kind.clone(),
        existence: Existence::Open,
        exposure: Exposure::Offscreen,
        residency: Residency::Cold,
    });
    durable.local_only_cards.extend([rejected.id, editor]);
    durable
        .channels
        .iter_mut()
        .find(|channel| channel.id == hidden)
        .unwrap()
        .cards = vec![rejected.id, editor, healthy.id];
    durable.active_channel = Some(hidden);
    let mut expected = vec![rejected.id, editor, healthy.id];
    if include_utility {
        let utility = CardId::new();
        durable.cards.push(CardState {
            id: utility,
            kind: CardKind::Settings,
            title: "Settings".into(),
            generation: durable.generation,
            width_fraction: 0.5,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hot,
        });
        expected.insert(1, utility);
        durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == hidden)
            .unwrap()
            .cards = expected.clone();
    }
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.shared_views = State::for_workspace(&app.durable);
    app.shared_views
        .initialized
        .extend([Scope::Channel(active), Scope::Channel(hidden)]);
    // A browser sibling is sufficient to exercise projection without dispatching
    // any native-provider work from this pure ordering regression.
    let (_, _, _, browser) = fixture();
    healthy.kind = browser.kind;
    app.card_state_mut(healthy.id).unwrap().kind = healthy.kind.clone();
    let mut view = View::empty(hidden);
    view.panes = vec![rejected.clone(), healthy.clone()];
    let mut full = model(identity, active, hidden, view);
    let mut owner = managed_owner(&rejected, hidden);
    owner.closed = true;
    full.agents.push(owner);
    app.workspace_model = full;
    drop(app.reconcile_shared_views());
    let channel = app
        .durable
        .channels
        .iter()
        .find(|channel| channel.id == hidden)
        .unwrap();
    assert_eq!(channel.cards, expected);
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), expected);
    drop(app.reconcile_shared_views());
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    drop(app.reconcile_shared_views());
    assert_eq!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == hidden)
            .unwrap()
            .cards,
        expected
    );
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), expected);
    assert_eq!(app.card_state(editor).unwrap().kind, editor_kind);
    assert!(app.card_state(editor).unwrap().existence.is_open());
    assert_eq!(app.card_state(editor).unwrap().title, "Retained editor");
    assert_eq!(app.card_state(healthy.id).unwrap().kind, healthy.kind);
    assert_eq!(
        app.durable
            .cards
            .iter()
            .map(|card| card.id)
            .collect::<HashSet<_>>()
            .len(),
        app.durable.cards.len(),
        "visible ordering cannot overwrite a distinct durable descriptor"
    );
}

/// Drive the real initial-model and SeedView acknowledgement reducer routes.
/// A Connected metadata snapshot alone does not yet admit the cached pane.
fn admit(
    app: &mut Cagents,
    active: ChannelId,
    hidden: ChannelId,
    model: crate::workspace_model::WorkspaceModel,
) {
    use crate::app::workspace_model_sync::ModelUpdate;
    let connected = app.apply_workspace_model_update(ModelUpdate::Connected(model.clone()));
    settle(app, connected);
    for channel in [active, hidden] {
        let applied = app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: Scope::Channel(channel),
            operation: None,
            model: model.clone(),
        });
        settle(app, applied);
    }
}

#[test]
fn retired_cached_shared_chat_executes_no_provider_start() {
    if isolated("retired_cached_shared_chat_executes_no_provider_start") {
        return;
    }
    let daemon = ScriptedDaemon::start();
    let (mut app, active, hidden, pane, identity) = cached_chat();
    let startup = app.initialize_runtimes();
    settle(&mut app, startup);
    assert!(
        daemon.requests().is_empty(),
        "the cache must not admit provider effects before the initial model"
    );
    let mut view = View::empty(hidden);
    view.retired.push(pane.id);
    admit(
        &mut app,
        active,
        hidden,
        model(identity, active, hidden, view),
    );
    assert!(!app.card_state(pane.id).unwrap().existence.is_open());
    assert!(
        daemon.requests().is_empty(),
        "a canonical tombstone must not leave an unreachable provider"
    );
}

#[test]
fn authoritative_offscreen_chat_executes_exact_descriptor_once() {
    if isolated("authoritative_offscreen_chat_executes_exact_descriptor_once") {
        return;
    }
    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    let CardKind::Chat(canonical) = &mut pane.kind else {
        unreachable!()
    };
    canonical.launch.recipe.arguments = vec!["--permission-mode=plan".into()];
    let expected = current_owner(&mut pane);
    let daemon = ScriptedDaemon::with_owner(expected.clone());
    app.chat_ui =
        chat_ui::State::with_drafts(HashMap::from([(pane.id, "Deferred shared draft".into())]));
    let startup = app.initialize_runtimes();
    settle(&mut app, startup);
    assert!(daemon.requests().is_empty());
    let mut view = View::empty(hidden);
    view.panes.push(pane.clone());
    admit(
        &mut app,
        active,
        hidden,
        model(identity, active, hidden, view),
    );
    let requests = daemon.requests();
    assert_eq!(
        requests.len(),
        1,
        "admit even an offscreen chat exactly once"
    );
    assert_eq!(requests[0].id, expected.id);
    assert_eq!(
        requests[0].launch, expected.launch,
        "the authoritative descriptor replaces the cache before effects"
    );
    assert_eq!(requests[0], expected);
    assert!(daemon.resolutions().is_empty());
    let repeated = app.reconcile_shared_views();
    settle(&mut app, repeated);
    assert_eq!(daemon.requests().len(), 1);
    assert_eq!(app.durable.active_channel, Some(active));
    assert_eq!(
        app.persistent_workspace()
            .chat_drafts
            .values()
            .find(|text| text.as_str() == "Deferred shared draft")
            .map(String::as_str),
        Some("Deferred shared draft")
    );
}

#[test]
fn local_only_cached_chat_still_restores_and_preserves_its_draft() {
    if isolated("local_only_cached_chat_still_restores_and_preserves_its_draft") {
        return;
    }
    let (mut app, _, _, mut pane, _) = cached_chat();
    let expected = current_owner(&mut pane);
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    let daemon = ScriptedDaemon::with_owner(expected.clone());
    Arc::make_mut(&mut app.durable)
        .local_only_cards
        .insert(pane.id);
    app.shared_views = State::for_workspace(&app.durable);
    app.chat_ui =
        chat_ui::State::with_drafts(HashMap::from([(pane.id, "Unsent original draft".into())]));
    assert!(
        app.shared_views.attached.contains(&pane.id),
        "local-only may also be present in the cached shared set"
    );
    let startup = app.initialize_runtimes();
    settle(&mut app, startup);
    let requests = daemon.requests();
    assert_eq!(
        requests.len(),
        1,
        "local-only restoration does not wait for a registry"
    );
    assert_eq!(
        requests[0].id,
        match &pane.kind {
            CardKind::Chat(session) => session.id,
            _ => unreachable!(),
        }
    );
    assert_eq!(requests[0], expected);
    assert!(daemon.resolutions().is_empty());
    assert_eq!(
        app.persistent_workspace()
            .chat_drafts
            .get(&pane.id)
            .map(String::as_str),
        Some("Unsent original draft")
    );
}

fn visible_text(app: &Cagents, card: CardId) -> String {
    use cosmic::iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    #[derive(Default)]
    struct Texts(Vec<String>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, _: cosmic::iced::Rectangle, text: &str) {
            self.0.push(text.to_owned());
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    let mut element = app.chat_view(card);
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(
            cosmic::iced::Size::ZERO,
            cosmic::iced::Size::new(900.0, 980.0),
        ),
    );
    let mut texts = Texts::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
    texts.0.join("\n")
}

#[test]
fn retired_cached_draft_stays_readable_and_cannot_start_provider() {
    if isolated("retired_cached_draft_stays_readable_and_cannot_start_provider") {
        return;
    }
    let daemon = ScriptedDaemon::start();
    let (mut app, active, hidden, pane, identity) = cached_chat();
    let text = "Never sent\nKeep exact spaces  ";
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(pane.id, text.into())]));
    let startup = app.initialize_runtimes();
    settle(&mut app, startup);
    let mut view = View::empty(hidden);
    view.retired.push(pane.id);
    admit(
        &mut app,
        active,
        hidden,
        model(identity, active, hidden, view),
    );
    assert!(app.chat_awaiting_admission(pane.id));
    let visible = visible_text(&app, pane.id);
    assert!(
        visible.contains(text) && visible.contains("Copy saved draft"),
        "{visible}"
    );
    assert!(
        !visible.contains("Closed in another window"),
        "cache is not closure proof"
    );
    let target = match app.cards.get(&pane.id).unwrap() {
        CardRuntime::Chat(runtime) => runtime.target,
        _ => panic!("saved draft must have a readable runtime"),
    };
    for action in [
        chat_ui::Action::Reconnect(target),
        chat_ui::Action::StartFresh(pane.id),
        chat_ui::Action::Switch(pane.id),
        chat_ui::Action::Submit(target, crate::chat::PromptMode::Send),
    ] {
        let task = app.handle_chat_ui(action);
        settle(&mut app, task);
    }
    assert!(daemon.requests().is_empty());
    let refused = app.begin_chat_close(pane.id);
    settle(&mut app, refused);
    assert!(
        app.card_state(pane.id).unwrap().existence.is_open(),
        "offstrip close must refuse without spending its restart fence"
    );
    assert!(app.durable.pending_chat_admissions.contains_key(&pane.id));
    let saved = app.persistent_workspace();
    assert_eq!(
        saved.chat_drafts.get(&pane.id).map(String::as_str),
        Some(text)
    );
    let disk = tempfile::tempdir().unwrap();
    let store = crate::persistence::StateStore::open(
        disk.path().join("state.sqlite"),
        crate::model::BootId::new("startup-test"),
    )
    .unwrap();
    store
        .save_workspace(
            &saved,
            crate::persistence::SnapshotBase {
                workspace: 0,
                cards: &Default::default(),
            },
        )
        .unwrap();
    let loaded = store.load_workspace(saved.id, None).unwrap().unwrap();
    assert_eq!(
        loaded.pending_chat_admissions,
        saved.pending_chat_admissions
    );
    let (mut restored, _, _, _, _) = cached_chat();
    restored.chat_ui = chat_ui::State::with_drafts(loaded.chat_drafts.clone());
    restored.durable = Arc::new(loaded);
    restored.shared_views = State::for_workspace(&restored.durable);
    restored.workspace = Cagents::rebuild_workspace(&restored.durable);
    restored.workspace_model = Default::default();
    let task = restored.initialize_runtimes();
    settle(&mut restored, task);
    assert!(restored.chat_awaiting_admission(pane.id));
    assert!(visible_text(&restored, pane.id).contains(text));
    assert!(
        daemon.requests().is_empty(),
        "save/load must not turn retained text into provider admission"
    );
    let select = restored.select_channel(hidden);
    settle(&mut restored, select);
    let close = restored.begin_chat_close(pane.id);
    settle(&mut restored, close);
    assert!(
        daemon.requests().is_empty(),
        "closing saved text cannot retire its unadmitted source"
    );
    assert!(!restored.card_state(pane.id).unwrap().existence.is_open());
    assert!(
        !restored
            .durable
            .pending_chat_admissions
            .contains_key(&pane.id)
    );
}

struct OrdinaryRetainedChatFixture {
    _directory: tempfile::TempDir,
    app: Cagents,
    full: crate::workspace_model::WorkspaceModel,
    loaded: WorkspaceState,
    entry: crate::persistence::SavedConversation,
}

fn ordinary_closed_retained_chat_fixture() -> OrdinaryRetainedChatFixture {
    use crate::persistence::{SnapshotBase, StateStore};
    let directory = tempfile::tempdir().unwrap();
    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    let CardKind::Chat(source) = &mut pane.kind else {
        unreachable!()
    };
    source.launch.recipe.working_directory = directory.path().into();
    let original = source.clone();
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    Arc::make_mut(&mut app.durable).active_channel = Some(hidden);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(
        pane.id,
        "Ordinary retained source draft".into(),
    )]));
    drop(app.initialize_runtimes());
    assert!(app.chat_awaiting_admission(pane.id));
    let CardKind::Chat(replacement) = &mut pane.kind else {
        unreachable!()
    };
    replacement.id = TerminalId::new();
    replacement.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
    replacement.proof = Some(scripted_proof());
    // Use the real incompatible-source producer, including its new retained
    // CardId, pending-admission payload and ordinary local custody marker.
    let (presentation, copy) = app.retain_unresolved_shared_upgrade(&pane);
    drop(presentation);
    let copy = copy.expect("the original unsent source receives a retained copy");
    assert!(app.chat_awaiting_admission(copy));
    assert!(app.durable.local_only_cards.contains(&copy));
    assert!(!app.pane_is_quarantined(copy));
    drop(app.begin_chat_close(copy));
    assert!(!app.card_state(copy).unwrap().existence.is_open());
    assert!(app.durable.local_only_cards.contains(&copy));
    assert!(!app.durable.pending_chat_admissions.contains_key(&copy));
    let mut view = View::empty(hidden);
    view.panes.push(pane.clone());
    let mut full = model(identity, active, hidden, view);
    full.agents.push(managed_owner(&pane, hidden));
    app.refresh_managed_pane_quarantine(&full);
    assert!(!app.pane_is_quarantined(copy));
    assert!(!app.durable.quarantined_panes.contains_key(&copy));
    let saved = app.persistent_workspace();
    let store = StateStore::open(
        directory.path().join("ordinary-retained-chat.sqlite3"),
        crate::model::BootId::new("ordinary-retained-chat"),
    )
    .unwrap();
    store
        .save_workspace(
            &saved,
            SnapshotBase {
                workspace: 0,
                cards: &Default::default(),
            },
        )
        .unwrap();
    let loaded = store.load_workspace(saved.id, None).unwrap().unwrap();
    assert!(loaded.local_only_cards.contains(&copy));
    assert!(!loaded.quarantined_panes.contains_key(&copy));
    let entry = store
        .saved_conversations()
        .unwrap()
        .into_iter()
        .find(|entry| entry.card == copy)
        .expect("ordinary closed retained source stays in saved History");
    assert!(!entry.open);
    assert!(!entry.quarantined);
    assert_eq!(entry.session, original);
    OrdinaryRetainedChatFixture {
        _directory: directory,
        app,
        full,
        loaded,
        entry,
    }
}

fn restore_ordinary_retained_chat(fixture: &OrdinaryRetainedChatFixture) -> Cagents {
    let (mut restored, _, _, _, _) = cached_chat();
    restored.durable = Arc::new(fixture.loaded.clone());
    restored.workspace = Cagents::rebuild_workspace(&restored.durable);
    restored.shared_views = State::for_workspace(&restored.durable);
    restored.workspace_model = fixture.full.clone();
    restored.refresh_managed_pane_quarantine(&fixture.full);
    assert!(!restored.pane_is_quarantined(fixture.entry.card));
    restored
}

#[test]
fn ordinary_retained_chat_copy_close_allows_same_process_reopen_and_restored_history() {
    for route in ["history", "reopen"] {
        let fixture = ordinary_closed_retained_chat_fixture();
        let entry = fixture.entry.clone();
        let copy = entry.card;
        let mut restored = if route == "history" {
            restore_ordinary_retained_chat(&fixture)
        } else {
            // Reopen owns only the original process's in-memory close queue.
            // It is not copied into the restarted application.
            fixture.app
        };
        let task = if route == "history" {
            restored.resume_saved_conversation(entry.clone())
        } else {
            restored.run_workspace_command(
                workspace_widget::GlobalShortcut::ReopenPane,
                keyboard::Modifiers::empty(),
            )
        };
        // Test the actual user reducer through insertion; provider I/O remains
        // lazy and is never executed by this pure regression.
        drop(task);
        assert!(restored.resume_ui.error.is_none(), "{route}");
        let reopened = restored.workspace.focused().unwrap();
        assert_ne!(reopened, copy);
        assert!(
            matches!(restored.card_state(reopened).map(|card| &card.kind),
            Some(CardKind::Chat(session)) if session.launch == entry.session.launch)
        );
        assert!(restored.terminal_starts.lock().unwrap().is_empty());
        assert!(!restored.pane_is_quarantined(copy));
    }
}

#[test]
fn ordinary_closed_retained_chat_history_and_same_process_reopen_dispatch_only_the_explicit_resume()
{
    assert!(
        crate::agent_execution::Authority::from_environment()
            .token()
            .is_none(),
        "provider-effect qualification requires a genuine User invocation before isolation; inherited authority must not be cleared"
    );
    if isolated(
        "ordinary_closed_retained_chat_history_and_same_process_reopen_dispatch_only_the_explicit_resume",
    ) {
        return;
    }
    let daemon = ScriptedDaemon::start();
    for (index, route) in ["history", "reopen"].into_iter().enumerate() {
        let fixture = ordinary_closed_retained_chat_fixture();
        let entry = fixture.entry.clone();
        let mut restored = if route == "history" {
            restore_ordinary_retained_chat(&fixture)
        } else {
            fixture.app
        };
        let task = if route == "history" {
            restored.resume_saved_conversation(entry.clone())
        } else {
            restored.run_workspace_command(
                workspace_widget::GlobalShortcut::ReopenPane,
                keyboard::Modifiers::empty(),
            )
        };
        settle(&mut restored, task);
        let requests = daemon.requests();
        assert_eq!(requests.len(), index + 1, "{route}");
        let request = requests.last().unwrap();
        assert_eq!(request.launch, entry.session.launch, "{route}");
        assert_ne!(request.id, entry.session.id);
        assert!(request.proof.is_none());
        assert!(!restored.pane_is_quarantined(entry.card));
        assert!(daemon.resolutions().is_empty());
    }
    assert!(
        !daemon
            .controls
            .lock()
            .unwrap()
            .iter()
            .any(|request| matches!(
                request,
                ClientRequest::ChatClose { .. }
                    | ClientRequest::ChatCloseRetained { .. }
                    | ClientRequest::CloseTerminalChecked { .. }
            ))
    );
}

#[test]
fn ordinary_retained_repository_close_keeps_custody_without_denial_and_allows_same_process_reopen()
{
    use crate::persistence::{SnapshotBase, StateStore};
    let (mut app, active, hidden, mut pane) = fixture();
    let runtime = RepositoryRuntime::dirty_fixture(pane.id);
    pane.kind = CardKind::Repository(runtime.descriptor.clone());
    let descriptor = &runtime.descriptor;
    app.sidebar_model.projects.push(SidebarProject::new(
        descriptor.project,
        RepositoryId::new(),
        "retained editor".into(),
        descriptor.repository.clone(),
        vec![crate::sidebar::SidebarWorktree::new(
            crate::model::WorktreeId::new(),
            descriptor.worktree.clone(),
            Some("main".into()),
            false,
            false,
            None,
            None,
            None,
        )],
        vec![],
    ));
    for channel in &mut Arc::make_mut(&mut app.durable).channels {
        channel.project_id = descriptor.project;
    }
    // Native channel entry establishes the directory and active_workspace
    // that scope snapshots need. Merely assigning active_channel would leave
    // an impossible active channel with no selected project directory.
    drop(app.select_channel(active));
    assert_eq!(app.durable.active_channel, Some(active));
    assert_eq!(app.active_workspace, Some(descriptor.worktree.clone()));
    let mut view = View::empty(active);
    view.panes.push(pane.clone());
    drop(app.project_shared_view(&view, &HashSet::from([pane.id])));
    app.cards
        .insert(pane.id, CardRuntime::Repository(Box::new(runtime)));
    let mut retired = View::empty(active);
    retired.retired.push(pane.id);
    // The actual omission reducer retains a dirty editor locally; this is
    // ordinary unsaved-work custody, with no managed-proof denial.
    drop(app.project_shared_view(&retired, &HashSet::new()));
    assert!(app.durable.local_only_cards.contains(&pane.id));
    assert!(!app.pane_is_quarantined(pane.id));
    assert!(
        matches!(app.cards.get(&pane.id), Some(CardRuntime::Repository(runtime)) if runtime.is_dirty())
    );
    assert_eq!(app.workspace.focused(), Some(pane.id));
    assert_eq!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == active)
            .unwrap()
            .focused,
        Some(pane.id)
    );
    drop(app.update(Message::CloseCard(pane.id)));
    assert!(app.dirty_repository_close.is_some());
    drop(app.update(Message::DiscardAndCloseRepository(pane.id)));
    assert!(app.dirty_repository_close.is_none());
    // Deliver the ordinary close continuation after the editor's explicit
    // discard, then its reserved native focus-handoff completion.
    drop(app.update(Message::CloseCard(pane.id)));
    assert!(
        app.workspace
            .pending_close()
            .is_some_and(|close| close.unmounting == pane.id)
    );
    drop(app.update(Message::CommitClose(pane.id)));
    assert!(!app.card_state(pane.id).unwrap().existence.is_open());
    assert!(app.durable.local_only_cards.contains(&pane.id));
    assert_eq!(app.workspace.focused(), None);
    let channel = app
        .durable
        .channels
        .iter()
        .find(|channel| channel.id == active)
        .unwrap();
    assert!(!channel.cards.contains(&pane.id));
    assert_eq!(channel.focused, None);
    let full = model(
        app.workspace_model.identity,
        active,
        hidden,
        View::empty(hidden),
    );
    app.refresh_managed_pane_quarantine(&full);
    assert!(!app.pane_is_quarantined(pane.id));
    let saved = app.persistent_workspace();
    let directory = tempfile::tempdir().unwrap();
    let store = StateStore::open(
        directory.path().join("ordinary-retained-editor.sqlite3"),
        crate::model::BootId::new("ordinary-retained-editor"),
    )
    .unwrap();
    store
        .save_workspace(
            &saved,
            SnapshotBase {
                workspace: 0,
                cards: &Default::default(),
            },
        )
        .unwrap();
    let loaded = store.load_workspace(saved.id, None).unwrap().unwrap();
    assert!(loaded.local_only_cards.contains(&pane.id));
    assert!(!loaded.quarantined_panes.contains_key(&pane.id));
    let mut restored = crate::app::tests::context_inspector_test_app(false);
    restored.durable = Arc::new(loaded);
    restored.shared_views = State::for_workspace(&restored.durable);
    restored.workspace = Cagents::rebuild_workspace(&restored.durable);
    restored.refresh_managed_pane_quarantine(&full);
    assert!(!restored.pane_is_quarantined(pane.id));
    assert!(!restored.card_state(pane.id).unwrap().existence.is_open());
    // The close queue belongs to the original process. The restarted editor's
    // saved source and custody marker separately remain free of denial.
    drop(app.run_workspace_command(
        workspace_widget::GlobalShortcut::ReopenPane,
        keyboard::Modifiers::empty(),
    ));
    let reopened = app.workspace.focused().expect("ordinary editor reopens");
    assert_ne!(reopened, pane.id);
    assert_eq!(app.card_state(reopened).unwrap().kind, pane.kind);
    assert!(app.recently_closed.is_empty());
    assert!(app.terminal_starts.lock().unwrap().is_empty());
}

#[test]
fn closed_local_dismissal_keeps_explicit_denial_after_exact_owner_returns() {
    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    current_owner(&mut pane);
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    let mut view = View::empty(hidden);
    view.panes.push(pane.clone());
    let mut full = model(identity, active, hidden, view);
    let mut owner = managed_owner(&pane, hidden);
    owner.closed = true;
    full.agents.push(owner);
    app.refresh_managed_pane_quarantine(&full);
    drop(app.begin_chat_close(pane.id));
    assert!(!app.card_state(pane.id).unwrap().existence.is_open());
    assert!(app.durable.local_only_cards.contains(&pane.id));
    assert_eq!(app.durable.quarantined_panes[&pane.id], pane.kind);
    app.shared_views = State::for_workspace(&app.durable);
    full.sequence += 1;
    full.agents[0].closed = false;
    app.refresh_managed_pane_quarantine(&full);
    assert!(app.pane_is_quarantined(pane.id));
    assert_eq!(app.durable.quarantined_panes[&pane.id], pane.kind);
    assert!(!app.card_state(pane.id).unwrap().existence.is_open());
    assert!(!app.chat_close_owned(pane.id));
    assert!(app.terminal_starts.lock().unwrap().is_empty());
}

#[test]
fn replacement_admission_keeps_old_draft_beside_exact_canonical_chat() {
    if isolated("replacement_admission_keeps_old_draft_beside_exact_canonical_chat") {
        return;
    }
    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    let original = pane.kind.clone();
    let CardKind::Chat(replacement) = &mut pane.kind else {
        unreachable!()
    };
    replacement.id = TerminalId::new();
    replacement.launch.session_id = Some("canonical-replacement-thread".into());
    let expected = current_owner(&mut pane);
    let daemon = ScriptedDaemon::with_owner(expected.clone());
    Arc::make_mut(&mut app.durable).active_channel = Some(hidden);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(pane.id, "Old source only".into())]));
    let startup = app.initialize_runtimes();
    settle(&mut app, startup);
    let mut view = View::empty(hidden);
    view.panes.push(pane.clone());
    admit(
        &mut app,
        active,
        hidden,
        model(identity, active, hidden, view),
    );
    assert_eq!(daemon.requests().len(), 1);
    assert_eq!(daemon.requests()[0].id, expected.id);
    assert_eq!(daemon.requests()[0], expected);
    assert!(daemon.resolutions().is_empty());
    let saved = app.persistent_workspace();
    assert!(!saved.chat_drafts.contains_key(&pane.id));
    let copy = *saved
        .chat_drafts
        .iter()
        .find(|(_, text)| text.as_str() == "Old source only")
        .unwrap()
        .0;
    assert_ne!(copy, pane.id);
    assert_eq!(app.card_state(copy).unwrap().kind, original);
    assert!(app.chat_awaiting_admission(copy));
    assert!(
        app.workspace.card(copy).is_some(),
        "saved copy must be immediately reachable"
    );
    assert!(visible_text(&app, copy).contains("Old source only"));
    assert!(saved.pending_chat_admissions.contains_key(&copy));
    assert!(!saved.pending_chat_admissions.contains_key(&pane.id));
    let disk = tempfile::tempdir().unwrap();
    let store = crate::persistence::StateStore::open(
        disk.path().join("retained-copy.sqlite"),
        crate::model::BootId::new("replacement-startup"),
    )
    .unwrap();
    store
        .save_workspace(
            &saved,
            crate::persistence::SnapshotBase {
                workspace: 0,
                cards: &Default::default(),
            },
        )
        .unwrap();
    let loaded = store.load_workspace(saved.id, None).unwrap().unwrap();
    let (mut restored, _, _, _, _) = cached_chat();
    restored.chat_ui = chat_ui::State::with_drafts(loaded.chat_drafts.clone());
    restored.durable = Arc::new(loaded);
    restored.workspace = Cagents::rebuild_workspace(&restored.durable);
    restored.shared_views = State::for_workspace(&restored.durable);
    restored.workspace_model = Default::default();
    let startup = restored.initialize_runtimes();
    settle(&mut restored, startup);
    assert!(restored.chat_awaiting_admission(copy));
    assert!(visible_text(&restored, copy).contains("Old source only"));
    assert_eq!(
        daemon.requests().len(),
        1,
        "a retained incompatible copy must stay inert after real storage roundtrip"
    );
}

#[test]
fn history_picker_does_not_focus_retained_unadmitted_cache() {
    if isolated("history_picker_does_not_focus_retained_unadmitted_cache") {
        return;
    }
    let daemon = ScriptedDaemon::start();
    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    let CardKind::Chat(session) = &mut pane.kind else {
        unreachable!()
    };
    session.launch.recipe.working_directory = PathBuf::from(std::env::var_os("HOME").unwrap());
    let saved_session = session.clone();
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    app.chat_ui =
        chat_ui::State::with_drafts(HashMap::from([(pane.id, "Retained source draft".into())]));
    let startup = app.initialize_runtimes();
    settle(&mut app, startup);
    let mut view = View::empty(hidden);
    view.retired.push(pane.id);
    admit(
        &mut app,
        active,
        hidden,
        model(identity, active, hidden, view),
    );
    let disk = tempfile::tempdir().unwrap();
    let store = crate::persistence::StateStore::open(
        disk.path().join("history.sqlite"),
        crate::model::BootId::new("history-startup"),
    )
    .unwrap();
    let saved = app.persistent_workspace();
    store
        .save_workspace(
            &saved,
            crate::persistence::SnapshotBase {
                workspace: 0,
                cards: &Default::default(),
            },
        )
        .unwrap();
    let entry = store
        .saved_conversations()
        .unwrap()
        .into_iter()
        .find(|entry| entry.card == pane.id)
        .unwrap();
    assert!(
        entry.open,
        "actual saved history marks this retained local card open"
    );
    let task = app.resume_saved_conversation(entry);
    settle(&mut app, task);
    let requests = daemon.requests();
    assert_eq!(
        requests.len(),
        1,
        "actual picker dispatch must open history, not focus inert text"
    );
    assert_eq!(requests[0].launch, saved_session.launch);
    assert_ne!(requests[0].id, saved_session.id);
    assert!(app.chat_awaiting_admission(pane.id));
    assert!(
        app.persistent_workspace()
            .chat_drafts
            .values()
            .any(|text| text == "Retained source draft")
    );
}

#[test]
fn startup_gate_waits_for_shared_authority_and_requires_offscreen_attachment() {
    if isolated("startup_gate_waits_for_shared_authority_and_requires_offscreen_attachment") {
        return;
    }
    use crate::app::workspace_model_sync::ModelUpdate;
    let (mut app, active, hidden, pane, identity) = cached_chat();
    let CardKind::Chat(expected) = &pane.kind else {
        unreachable!()
    };
    let expected = expected.clone();
    let mut canonical = expected.clone();
    canonical.proof = Some(scripted_proof());
    let daemon = ScriptedDaemon::with_attachment(
        Some(UpgradeScript {
            source: expected.clone(),
            canonical: canonical.clone(),
            refuse_lineage: false,
        }),
        true,
    );
    let startup = app.initialize_runtimes();
    settle(&mut app, startup);
    assert_eq!(app.startup_pane_check(), Ok(false));
    assert!(daemon.requests().is_empty());
    let mut view = View::empty(hidden);
    view.panes.push(pane.clone());
    let authoritative = model(identity, active, hidden, view);
    let connected = app.apply_workspace_model_update(ModelUpdate::Connected(authoritative.clone()));
    settle(&mut app, connected);
    assert_eq!(
        app.startup_pane_check(),
        Ok(false),
        "Connected is not SeedView admission"
    );
    assert!(daemon.requests().is_empty());
    let first = app.apply_workspace_model_update(ModelUpdate::ViewApplied {
        scope: Scope::Channel(active),
        operation: None,
        model: authoritative.clone(),
    });
    settle(&mut app, first);
    assert_eq!(
        app.startup_pane_check(),
        Ok(false),
        "hidden scope must also acknowledge"
    );
    assert!(daemon.requests().is_empty());
    let hidden_ack = app.apply_workspace_model_update(ModelUpdate::ViewApplied {
        scope: Scope::Channel(hidden),
        operation: None,
        model: authoritative,
    });
    settle(&mut app, hidden_ack);
    assert_eq!(daemon.requests().len(), 1);
    assert_eq!(daemon.requests()[0].launch, expected.launch);
    assert_eq!(daemon.requests()[0].id, expected.id);
    assert_eq!(daemon.requests(), vec![canonical]);
    assert_eq!(daemon.resolutions(), vec![expected.clone()]);
    assert_eq!(
        app.card_state(pane.id).unwrap().kind,
        CardKind::Chat(expected),
        "a refused attachment must retain its saved source"
    );
    assert_eq!(app.durable.active_channel, Some(active));
    assert!(
        app.startup_pane_check().is_err(),
        "a refused offscreen attachment cannot qualify"
    );
    app.cards.remove(&pane.id);
    assert_eq!(
        app.startup_pane_check(),
        Ok(false),
        "a missing required runtime cannot qualify"
    );
}

#[test]
fn exact_retained_saved_text_is_data_after_scope_admission_not_a_hosted_waiver() {
    if isolated("exact_retained_saved_text_is_data_after_scope_admission_not_a_hosted_waiver") {
        return;
    }
    let daemon = ScriptedDaemon::start();
    let (mut app, active, hidden, pane, identity) = cached_chat();
    let CardKind::Chat(source) = &pane.kind else {
        unreachable!()
    };
    let source = source.clone();
    let durable = Arc::make_mut(&mut app.durable);
    durable.local_only_cards.insert(pane.id);
    durable
        .pending_chat_admissions
        .insert(pane.id, source.clone());
    app.shared_views = State::for_workspace(&app.durable);
    app.chat_ui =
        chat_ui::State::with_drafts(HashMap::from([(pane.id, "Saved data only  ".into())]));
    let startup = app.initialize_runtimes();
    settle(&mut app, startup);
    assert_eq!(app.startup_pane_check(), Ok(false));
    let mut view = View::empty(hidden);
    view.retired.push(pane.id);
    admit(
        &mut app,
        active,
        hidden,
        model(identity, active, hidden, view),
    );
    assert!(daemon.requests().is_empty());
    assert_eq!(app.startup_pane_check(), Ok(true));
    let saved = app.persistent_workspace();
    assert_eq!(saved.pending_chat_admissions.get(&pane.id), Some(&source));
    assert_eq!(
        saved.chat_drafts.get(&pane.id).map(String::as_str),
        Some("Saved data only  ")
    );
    assert_eq!(
        app.card_state(pane.id).unwrap().kind,
        CardKind::Chat(source.clone())
    );
    assert!(visible_text(&app, pane.id).contains("Saved data only  "));
    Arc::make_mut(&mut app.durable)
        .local_only_cards
        .remove(&pane.id);
    assert!(
        app.startup_pane_check().is_err(),
        "a pending marker alone cannot waive a hosted pane"
    );
    let durable = Arc::make_mut(&mut app.durable);
    durable.local_only_cards.insert(pane.id);
    let mut wrong = source;
    wrong.launch.session_id = Some("another-source".into());
    durable.pending_chat_admissions.insert(pane.id, wrong);
    assert!(
        app.startup_pane_check().is_err(),
        "another source's marker cannot waive readiness"
    );
    assert!(daemon.requests().is_empty());
}

fn exercise_canonical_upgrade(refuse_lineage: bool, retire_before_ready: bool) {
    use crate::app::workspace_model_sync::ModelUpdate;
    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    let CardKind::Chat(source) = &pane.kind else {
        unreachable!()
    };
    let source = source.clone();
    let mut canonical = source.clone();
    canonical.proof = Some(HostSessionProof {
        host_session_id: "live-host".into(),
        boot_id: "boot".into(),
        host_incarnation: "live-incarnation".into(),
    });
    let daemon = ScriptedDaemon::with_upgrade(Some(UpgradeScript {
        source: source.clone(),
        canonical: canonical.clone(),
        refuse_lineage,
    }));
    let text = "Draft stays here  ";
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(pane.id, text.into())]));
    let disk = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        disk.path().join("upgrade.sqlite"),
        crate::model::BootId::new("boot"),
    )
    .unwrap();
    app.session = Some(writer);
    app.persist();
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(5))
        .unwrap();
    let startup = app.initialize_runtimes();
    settle(&mut app, startup);
    let (target, allocation) = match app.cards.get(&pane.id).unwrap() {
        CardRuntime::Chat(runtime) => (
            runtime.target,
            &**runtime as *const chat_ui::Runtime as usize,
        ),
        _ => panic!("saved source must have inert Content"),
    };
    let cursor = app.handle_chat_ui(chat_ui::Action::ComposerCursor(
        target,
        cosmic::iced::widget::text_editor::Cursor {
            position: cosmic::iced::widget::text_editor::Position { line: 0, column: 6 },
            selection: None,
        },
    ));
    settle(&mut app, cursor);
    pane.kind = CardKind::Chat(canonical.clone());
    let mut view = View::empty(hidden);
    view.panes.push(pane.clone());
    let authoritative = model(identity, active, hidden, view);
    let connected = app.apply_workspace_model_update(ModelUpdate::Connected(authoritative.clone()));
    settle(&mut app, connected);
    let first = app.apply_workspace_model_update(ModelUpdate::ViewApplied {
        scope: Scope::Channel(active),
        operation: None,
        model: authoritative.clone(),
    });
    settle(&mut app, first);
    let admission = app.apply_workspace_model_update(ModelUpdate::ViewApplied {
        scope: Scope::Channel(hidden),
        operation: None,
        model: authoritative,
    });
    if !refuse_lineage && !retire_before_ready {
        let before = app.persistent_workspace();
        assert_eq!(
            before
                .cards
                .iter()
                .find(|card| card.id == pane.id)
                .unwrap()
                .kind,
            CardKind::Chat(source.clone()),
            "projection cannot snapshot canonical ownership before lineage and writer receipt"
        );
        assert_eq!(
            before.chat_drafts.get(&pane.id).map(String::as_str),
            Some(text)
        );
    }
    if retire_before_ready {
        let mut retired = View::empty(hidden);
        retired.retired.push(pane.id);
        let mut newer = model(identity, active, hidden, retired);
        newer.sequence = 2;
        let projection = app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: Scope::Channel(hidden),
            operation: None,
            model: newer,
        });
        settle(&mut app, projection);
        assert!(!app.chat_start_pending(pane.id));
        assert!(!app.card_state(pane.id).unwrap().existence.is_open());
    }
    let mut ready_seen = false;
    let mut receipt_seen = false;
    settle_observing(&mut app, admission, |app, message| {
        if matches!(&message, Message::ChatUi(chat_ui::Action::Ready(..))) && !ready_seen {
            ready_seen = true;
            if !refuse_lineage && !retire_before_ready {
                assert_eq!(
                    app.persistent_workspace()
                        .cards
                        .iter()
                        .find(|card| card.id == pane.id)
                        .unwrap()
                        .kind,
                    CardKind::Chat(source.clone())
                );
            }
        }
        if let Message::ChatUi(chat_ui::Action::UpgradeCommitted(_, _, result)) = &message {
            assert!(
                result.is_ok(),
                "actual session writer must acknowledge: {result:?}"
            );
            receipt_seen = true;
        }
        Some(message)
    });
    assert!(ready_seen);
    assert_eq!(daemon.resolutions(), vec![source.clone()]);
    assert_eq!(
        daemon.requests(),
        if refuse_lineage {
            Vec::new()
        } else {
            vec![canonical.clone()]
        },
        "refused lineage cannot fall back to provider activation"
    );
    assert!(!app.chat_start_pending(pane.id));
    let saved = app.persistent_workspace();
    if retire_before_ready {
        assert!(
            !receipt_seen,
            "a stale Ready cannot authorize a writer rebind"
        );
        let copy = *saved
            .chat_drafts
            .iter()
            .find(|(_, draft)| draft.as_str() == text)
            .unwrap()
            .0;
        assert_ne!(copy, pane.id);
        assert!(app.chat_awaiting_admission(copy));
        assert_eq!(
            app.card_state(copy).unwrap().kind,
            CardKind::Chat(source.clone())
        );
        let CardRuntime::Chat(runtime) = app.cards.get(&copy).unwrap() else {
            unreachable!()
        };
        assert_eq!(
            &**runtime as *const chat_ui::Runtime as usize, allocation,
            "retirement must move the full original Content owner before the late Ready"
        );
        assert_eq!(saved.pending_chat_admissions.get(&copy), Some(&source));
        assert_eq!(app.startup_pane_check(), Ok(true));
        app.persist();
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        let loaded = app
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .load_workspace(saved.id, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded
                .cards
                .iter()
                .find(|card| card.id == copy)
                .unwrap()
                .kind,
            CardKind::Chat(source.clone())
        );
        assert_eq!(loaded.pending_chat_admissions.get(&copy), Some(&source));
        assert_eq!(
            loaded.chat_drafts.get(&copy).map(String::as_str),
            Some(text)
        );
        assert!(
            !loaded
                .cards
                .iter()
                .find(|card| card.id == pane.id)
                .unwrap()
                .existence
                .is_open()
        );
        return;
    }
    if refuse_lineage {
        assert!(
            !receipt_seen,
            "refused lineage cannot receive a writer rebind receipt"
        );
        let copy = *saved
            .chat_drafts
            .iter()
            .find(|(_, draft)| draft.as_str() == text)
            .unwrap()
            .0;
        assert_ne!(copy, pane.id);
        assert!(app.chat_awaiting_admission(copy));
        assert_eq!(
            app.card_state(copy).unwrap().kind,
            CardKind::Chat(source.clone())
        );
        assert_eq!(
            saved.pending_chat_admissions.get(&copy),
            match &app.card_state(copy).unwrap().kind {
                CardKind::Chat(session) => Some(session),
                _ => None,
            }
        );
        assert!(!saved.chat_drafts.contains_key(&pane.id));
        assert!(matches!(
            app.cards.get(&pane.id),
            Some(CardRuntime::Failed(_))
        ));
        assert!(
            app.startup_pane_check().is_err(),
            "required canonical recovery remains incomplete"
        );
    } else {
        assert!(
            receipt_seen,
            "strict wire proof must pass through the actual durable receipt"
        );
        assert_eq!(
            saved
                .cards
                .iter()
                .filter(|card| card.existence.is_open())
                .count(),
            1
        );
        assert_eq!(
            saved.chat_drafts.get(&pane.id).map(String::as_str),
            Some(text)
        );
        assert!(saved.pending_chat_admissions.is_empty());
        let runtime = match app.cards.get(&pane.id).unwrap() {
            CardRuntime::Chat(runtime) => runtime,
            _ => panic!("canonical chat must be attached"),
        };
        assert_eq!(
            &**runtime as *const chat_ui::Runtime as usize, allocation,
            "retain the original Content owner"
        );
        let target = runtime.target;
        let edit = app.handle_chat_ui(chat_ui::Action::Composer(
            target,
            cosmic::iced::widget::text_editor::Action::Edit(
                cosmic::iced::widget::text_editor::Edit::Paste(Arc::new("X".into())),
            ),
        ));
        settle(&mut app, edit);
        assert_eq!(
            app.persistent_workspace()
                .chat_drafts
                .get(&pane.id)
                .map(String::as_str),
            Some("Draft Xstays here  "),
            "an actual editor edit must use the pre-upgrade caret"
        );
    }
    if !refuse_lineage {
        assert_eq!(app.startup_pane_check(), Ok(true));
    }
    app.persist();
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(5))
        .unwrap();
    let stored = app
        .session
        .as_ref()
        .unwrap()
        .operation_store()
        .load_card(pane.id, None)
        .unwrap()
        .unwrap();
    assert_eq!(stored.kind, CardKind::Chat(canonical));
    if refuse_lineage {
        let loaded = app
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .load_workspace(saved.id, None)
            .unwrap()
            .unwrap();
        let copy = *loaded
            .chat_drafts
            .iter()
            .find(|(_, draft)| draft.as_str() == text)
            .unwrap()
            .0;
        assert_ne!(copy, pane.id);
        assert_eq!(
            loaded
                .cards
                .iter()
                .find(|card| card.id == copy)
                .unwrap()
                .kind,
            CardKind::Chat(source.clone())
        );
        assert_eq!(loaded.pending_chat_admissions.get(&copy), Some(&source));
    }
    assert!(app.card_state(pane.id).unwrap().existence.is_open());
}

#[test]
fn canonical_upgrade_authenticates_lineage_and_preserves_original_content_through_writer_receipt() {
    if isolated(
        "canonical_upgrade_authenticates_lineage_and_preserves_original_content_through_writer_receipt",
    ) {
        return;
    }
    exercise_canonical_upgrade(false, false);
}

#[test]
fn refused_canonical_upgrade_preserves_source_draft_without_provider_activation() {
    if isolated("refused_canonical_upgrade_preserves_source_draft_without_provider_activation") {
        return;
    }
    exercise_canonical_upgrade(true, false);
}

#[test]
fn canonical_retirement_before_upgrade_ready_preserves_source_content_and_ignores_late_attachment()
{
    if isolated(
        "canonical_retirement_before_upgrade_ready_preserves_source_content_and_ignores_late_attachment",
    ) {
        return;
    }
    exercise_canonical_upgrade(false, true);
}

#[test]
fn an_upgraded_visibility_reply_allows_peer_read_close_without_stopping_admission() {
    if isolated("an_upgraded_visibility_reply_allows_peer_read_close_without_stopping_admission") {
        return;
    }
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;

    fn exchange(
        stream: &mut UnixStream,
        proof: &HostSessionProof,
        request_id: u64,
        request: ClientRequest,
    ) -> ServerResponse {
        let connecting = matches!(&request, ClientRequest::Connect { .. });
        write_frame(
            stream,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id,
                proof: (!connecting).then(|| proof.clone()),
                request,
            },
        )
        .unwrap();
        let reply = read_frame::<ServerEnvelope>(stream).unwrap().unwrap();
        assert_eq!(reply.version, PROTOCOL_VERSION);
        assert_eq!(reply.request_id, Some(request_id));
        assert_eq!(&reply.proof, proof);
        match reply.message {
            ServerMessage::Response { response } => response,
            other => panic!("expected actual handshake reply: {other:?}"),
        }
    }

    let (_app, _, _, pane, _) = cached_chat();
    let CardKind::Chat(source) = pane.kind else {
        unreachable!()
    };
    let mut canonical = source.clone();
    let proof = HostSessionProof {
        host_session_id: "live-host".into(),
        boot_id: "boot".into(),
        host_incarnation: "live-incarnation".into(),
    };
    canonical.proof = Some(proof.clone());
    let daemon = ScriptedDaemon::with_upgrade(Some(UpgradeScript {
        source: source.clone(),
        canonical: canonical.clone(),
        refuse_lineage: false,
    }));
    for close_reader in [true, false] {
        let mut stream =
            UnixStream::connect(crate::terminal_ipc::daemon_socket_path().unwrap()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        match exchange(
            &mut stream,
            &proof,
            1,
            ClientRequest::Connect {
                expected_host: None,
                resume: vec![],
            },
        ) {
            ServerResponse::Connected {
                proof: connected,
                resumed,
            } => {
                assert_eq!(connected, proof);
                assert!(resumed.is_empty());
            }
            other => panic!("Connect must succeed: {other:?}"),
        }
        match exchange(
            &mut stream,
            &proof,
            2,
            ClientRequest::ResolveUpgradeChat {
                source: source.clone(),
            },
        ) {
            ServerResponse::UpgradeChatResolved { session } => assert_eq!(session, canonical),
            other => panic!("exact lineage must succeed: {other:?}"),
        }
        match exchange(
            &mut stream,
            &proof,
            3,
            ClientRequest::ChatAttach {
                session: canonical.clone(),
            },
        ) {
            ServerResponse::ChatAttached { session, snapshot } => {
                assert_eq!(session, canonical);
                assert!(snapshot.ready);
                assert_eq!(snapshot.session_id, canonical.launch.session_id);
            }
            other => panic!("exact attachment must succeed: {other:?}"),
        }
        let visibility = ClientRequest::ChatVisibility {
            chat_id: canonical.id,
            visible: false,
        };
        if close_reader {
            // Keep the write half live so the real server receives the whole
            // request after its peer has already stopped accepting replies.
            stream.shutdown(Shutdown::Read).unwrap();
            write_frame(
                &mut stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: 4,
                    proof: Some(proof.clone()),
                    request: visibility,
                },
            )
            .unwrap();
            assert_eq!(
                daemon
                    .visibility_canceled
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap(),
                4
            );
        } else {
            assert!(matches!(
                exchange(&mut stream, &proof, 4, visibility),
                ServerResponse::Accepted
            ));
        }
        assert_eq!(daemon.visibility_cancellations.load(Ordering::Acquire), 1);
    }
    assert_eq!(daemon.resolutions(), vec![source.clone(), source]);
    assert_eq!(daemon.requests(), vec![canonical.clone(), canonical]);
    assert!(matches!(
        daemon.visibility_canceled.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    // Drop must join both actual server connections without masking a panic.
    drop(daemon);
}
