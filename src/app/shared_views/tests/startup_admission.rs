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

    fn with_attachment(upgrade: Option<UpgradeScript>, refuse_attachment: bool) -> Self {
        let socket = crate::terminal_ipc::daemon_socket_path().unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let providers = Arc::new(Mutex::new(Vec::new()));
        let resolutions = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let visibility_cancellations = Arc::new(AtomicUsize::new(0));
        let (visibility_canceled_tx, visibility_canceled) = std::sync::mpsc::channel();
        let cancellations = visibility_cancellations.clone();
        let recorded = providers.clone();
        let resolved = resolutions.clone();
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
                        let message = match request.request {
                            ClientRequest::Connect { .. } => ServerMessage::Response {
                                response: ServerResponse::Connected {
                                    proof: proof.clone(),
                                    resumed: Vec::new(),
                                },
                            },
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
            quarantined_managed_panes(&authoritative, &app.durable, &HashSet::new(), &pending),
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
            quarantined_managed_panes(&full, &app.durable, &HashSet::new(), &pending),
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
    let original_focus = app.workspace.focused();
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
