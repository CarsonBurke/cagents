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
    for variable in [
        crate::agent_execution::AUTHORITY_ENV,
        "CAGENTS_AGENT_TOKEN",
        "CAGENTS_SESSION_TOKEN",
    ] {
        assert!(
            std::env::var_os(variable).is_none(),
            "startup admission qualification requires a genuine User/CI launch; inherited role variables must not be replaced"
        );
    }
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
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    // Isolate this User-only fixture without rewriting the authority slot.
    for (name, _) in std::env::vars_os() {
        if name != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV) {
            command.env_remove(name);
        }
    }
    let output = command
        .args([
            "--exact",
            &format!("app::shared_views::tests::startup_admission::{name}"),
            "--nocapture",
        ])
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
    // This parent exercises a live publisher waiting for its seed ACK. The
    // default headless sender has no receiver and correctly means Closed.
    let (publisher, _requests) = async_channel::unbounded();
    app.workspace_model_tx = publisher;
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

/// A real framed publisher exchange, separate from the chat fixture. The
/// refusal read is on the same pinned connection as the rejected SeedView.
fn framed_seed(
    edit: ModelEdit,
    model: crate::workspace_model::WorkspaceModel,
    refuse: bool,
) -> Vec<crate::app::workspace_model_sync::ModelUpdate> {
    let (stream, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    peer.set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let expected = edit.clone();
    let server = std::thread::spawn(move || {
        let proof = HostSessionProof {
            host_session_id: "publisher-host".into(),
            boot_id: "boot".into(),
            host_incarnation: "publisher-incarnation".into(),
        };
        let connected: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        assert!(matches!(connected.request, ClientRequest::Connect { .. }));
        let reply = |peer: &mut std::os::unix::net::UnixStream, request_id, message| {
            write_frame(
                peer,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request_id),
                    proof: proof.clone(),
                    message,
                },
            )
            .unwrap();
        };
        reply(
            &mut peer,
            connected.request_id,
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        assert_eq!(request.proof, Some(proof.clone()));
        assert_eq!(
            request.request,
            ClientRequest::EditWorkspaceModel { edit: expected }
        );
        if refuse {
            reply(
                &mut peer,
                request.request_id,
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::PermissionDenied,
                    "private-server-detail-must-not-be-rendered",
                    false,
                )),
            );
            let read: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(read.proof, Some(proof.clone()));
            assert_eq!(read.request, ClientRequest::ReadWorkspaceModel);
            reply(
                &mut peer,
                read.request_id,
                ServerMessage::Response {
                    response: ServerResponse::WorkspaceModel { model },
                },
            );
        } else {
            reply(
                &mut peer,
                request.request_id,
                ServerMessage::Response {
                    response: ServerResponse::WorkspaceModel { model },
                },
            );
        }
    });
    let mut client = crate::control::ControlClient::from_stream(stream).unwrap();
    let (tx, rx) = async_channel::unbounded();
    let result = crate::app::workspace_model_sync::send_seed_for_test(&mut client, edit, &tx);
    assert_eq!(result.is_err(), refuse);
    server.join().unwrap();
    let mut updates = Vec::new();
    while let Ok(update) = rx.try_recv() {
        updates.push(update);
    }
    assert_eq!(updates.len(), 2, "Started plus exactly one final outcome");
    updates
}

fn queued_seed(rx: &async_channel::Receiver<ModelEdit>, scope: &Scope) -> ModelEdit {
    let mut found = None;
    while let Ok(edit) = rx.try_recv() {
        if matches!(&edit, ModelEdit::SeedView { scope: queued, .. } if queued == scope) {
            assert!(
                found.replace(edit).is_none(),
                "one seed per admission attempt"
            );
        }
    }
    found.expect("the real Connected/publication path queued this scope")
}

fn apply_seed_updates(
    app: &mut Cagents,
    updates: Vec<crate::app::workspace_model_sync::ModelUpdate>,
) {
    for update in updates {
        let task = app.apply_workspace_model_update(update);
        settle(app, task);
    }
}

fn acknowledge_destination_seed(
    app: &mut Cagents,
    rx: &async_channel::Receiver<ModelEdit>,
    source: &Scope,
    destination: Scope,
    canonical: &crate::workspace_model::WorkspaceModel,
) -> ModelEdit {
    let queued = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
    let seed = |scope: &Scope| {
        let mut seeds = queued.iter().filter(
            |edit| matches!(edit, ModelEdit::SeedView { scope: queued, .. } if queued == scope),
        );
        let seed = seeds.next().expect("Connected queued this scope").clone();
        assert!(seeds.next().is_none(), "one seed per scope");
        seed
    };
    apply_seed_updates(
        app,
        framed_seed(seed(&destination), canonical.clone(), false),
    );
    assert!(app.shared_views.initialized.contains(&destination));
    assert!(!app.shared_views.seeding.contains(&destination));
    seed(source)
}

fn managed_chat(pane: &Pane, placement: Scope) -> crate::agent_execution::Agent {
    use crate::agent_execution::{Agent, HostedSession, Origin, Spec, Surface, Target};
    let CardKind::Chat(session) = &pane.kind else {
        unreachable!()
    };
    Agent {
        name: None,
        id: crate::model::AgentId::from_uuid(pane.id.as_uuid()),
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

#[test]
fn refused_seed_has_finite_failure_and_exact_retry_reopens_saved_chat_without_starting_provider() {
    if isolated(
        "refused_seed_has_finite_failure_and_exact_retry_reopens_saved_chat_without_starting_provider",
    ) {
        return;
    }
    use crate::app::workspace_model_sync::ModelUpdate;
    crate::chat::attachment_assets::with_test_image(|image| {
        let (mut app, active, hidden, mut pane, identity) = cached_chat();
        let CardKind::Chat(source) = &mut pane.kind else {
            unreachable!()
        };
        source.proof = Some(HostSessionProof {
            host_session_id: "live-host".into(),
            boot_id: "boot".into(),
            host_incarnation: "live-incarnation".into(),
        });
        let source = source.clone();
        app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
        let daemon = ScriptedDaemon::with_upgrade(Some(UpgradeScript {
            source: source.clone(),
            canonical: source.clone(),
            refuse_lineage: false,
        }));
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        let draft = "Unsent source text  ";
        app.chat_ui = chat_ui::State::with_draft_attachments(
            HashMap::from([(pane.id, draft.into())]),
            HashMap::from([(pane.id, vec![image.clone()])]),
        );
        let task = app.initialize_runtimes();
        settle(&mut app, task);
        let CardRuntime::Chat(runtime) = app.cards.get(&pane.id).unwrap() else {
            unreachable!()
        };
        let allocation = &**runtime as *const chat_ui::Runtime as usize;
        let target = runtime.target;
        let cursor = app.handle_chat_ui(chat_ui::Action::ComposerCursor(
            target,
            cosmic::iced::widget::text_editor::Cursor {
                position: cosmic::iced::widget::text_editor::Position { line: 0, column: 4 },
                selection: None,
            },
        ));
        settle(&mut app, cursor);
        assert_eq!(app.startup_pane_check(), Ok(false));
        let mut view = View::empty(hidden);
        view.panes.push(pane.clone());
        let canonical = model(identity, active, hidden, view);
        let connected = app.apply_workspace_model_update(ModelUpdate::Connected(canonical.clone()));
        settle(&mut app, connected);
        let scope = Scope::Channel(hidden);
        let seed = queued_seed(&rx, &scope);
        let active_ack = app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: Scope::Channel(active),
            operation: None,
            model: canonical.clone(),
        });
        settle(&mut app, active_ack);
        let failed = framed_seed(seed, canonical.clone(), true);
        let old_started = failed[0].clone();
        let old_refused = failed[1].clone();
        apply_seed_updates(&mut app, failed);
        let failure = app
            .shared_chat_admission_failure(pane.id)
            .expect("finite failure, not indefinite reconnect");
        assert!(!failure.message().contains("private-server-detail"));
        assert!(
            app.startup_pane_check()
                .unwrap_err()
                .contains("Saved workspace recovery failed")
        );
        let retry = failure.retry().unwrap();
        assert_eq!(retry, retry.clone());
        assert!(!format!("{retry:?}").contains(&source.id.to_string()));
        for _ in 0..3 {
            let read = app.apply_workspace_model_update(ModelUpdate::Read(canonical.clone()));
            settle(&mut app, read);
            app.publish_view_changes();
            app.persist();
        }
        let connected_again =
            app.apply_workspace_model_update(ModelUpdate::Connected(canonical.clone()));
        settle(&mut app, connected_again);
        assert!(
            rx.is_empty(),
            "read/persist/reconnect cannot replay permanent refusal"
        );
        assert!(daemon.requests().is_empty());
        assert!(app.chat_awaiting_admission(pane.id));
        assert_eq!(
            app.durable.pending_chat_admissions.get(&pane.id),
            Some(&source)
        );
        let task = app.retry_shared_chat_admission(retry.clone());
        settle(&mut app, task);
        assert!(app.shared_chat_admission_failure(pane.id).is_none());
        assert!(
            app.chat_awaiting_admission(pane.id),
            "retry is not admission"
        );
        let next = queued_seed(&rx, &scope);
        let task = app.retry_shared_chat_admission(retry.clone());
        settle(&mut app, task);
        assert!(
            rx.is_empty(),
            "a queued old retry cannot duplicate the new attempt"
        );
        apply_seed_updates(&mut app, vec![old_refused.clone()]);
        assert!(
            app.shared_chat_admission_failure(pane.id).is_none(),
            "old refusal cannot poison a fresh queued attempt"
        );
        let accepted = framed_seed(next, canonical.clone(), false);
        apply_seed_updates(&mut app, accepted);
        assert_eq!(
            daemon.requests(),
            vec![source.clone()],
            "exact ChatAttach only; ChatStart panics in this fixture"
        );
        assert!(
            daemon.resolutions().is_empty(),
            "no history substitution or lineage rewrite"
        );
        assert!(!app.chat_awaiting_admission(pane.id));
        assert!(!app.durable.pending_chat_admissions.contains_key(&pane.id));
        assert_eq!(app.startup_pane_check(), Ok(true));
        let CardRuntime::Chat(runtime) = app.cards.get(&pane.id).unwrap() else {
            unreachable!()
        };
        assert_eq!(
            &**runtime as *const chat_ui::Runtime as usize, allocation,
            "retain the Content owner and its caret"
        );
        let current_target = runtime.target;
        assert_eq!(current_target.card, target.card);
        assert!(
            current_target.generation > target.generation,
            "canonical reattachment must fence callbacks issued to the inert runtime"
        );
        assert_eq!(runtime.session, source);
        let saved = app.persistent_workspace();
        assert_eq!(
            saved.chat_drafts.get(&pane.id).map(String::as_str),
            Some(draft)
        );
        assert_eq!(saved.chat_draft_attachments[&pane.id], [image.clone()]);
        assert!(
            saved
                .channels
                .iter()
                .find(|channel| channel.id == hidden)
                .unwrap()
                .cards
                .contains(&pane.id)
        );
        assert_eq!(
            saved.active_channel,
            Some(active),
            "offscreen recovery does not steal selection"
        );
        let stale_cursor = app.handle_chat_ui(chat_ui::Action::ComposerCursor(
            target,
            cosmic::iced::widget::text_editor::Cursor {
                position: cosmic::iced::widget::text_editor::Position { line: 0, column: 0 },
                selection: None,
            },
        ));
        settle(&mut app, stale_cursor);
        let stale_edit = app.handle_chat_ui(chat_ui::Action::Composer(
            target,
            cosmic::iced::widget::text_editor::Action::Edit(
                cosmic::iced::widget::text_editor::Edit::Paste(Arc::new("OLD".into())),
            ),
        ));
        settle(&mut app, stale_edit);
        assert_eq!(
            app.persistent_workspace()
                .chat_drafts
                .get(&pane.id)
                .map(String::as_str),
            Some(draft),
            "an old UI callback cannot edit the recovered conversation"
        );
        let current_edit = app.handle_chat_ui(chat_ui::Action::Composer(
            current_target,
            cosmic::iced::widget::text_editor::Action::Edit(
                cosmic::iced::widget::text_editor::Edit::Paste(Arc::new("X".into())),
            ),
        ));
        settle(&mut app, current_edit);
        assert_eq!(
            app.persistent_workspace()
                .chat_drafts
                .get(&pane.id)
                .map(String::as_str),
            Some("UnseXnt source text  "),
            "the current callback must use the preserved pre-admission caret; stale cursor movement is ignored"
        );
        apply_seed_updates(&mut app, vec![old_started, old_refused]);
        let stale = app.retry_shared_chat_admission(retry);
        settle(&mut app, stale);
        assert!(app.shared_chat_admission_failure(pane.id).is_none());
        assert_eq!(daemon.requests().len(), 1);
        assert!(rx.is_empty());
    });
}

#[test]
fn failed_seed_retry_rejects_stale_owner_source_registry_and_attempt() {
    if isolated("failed_seed_retry_rejects_stale_owner_source_registry_and_attempt") {
        return;
    }
    use crate::app::workspace_model_sync::ModelUpdate;
    let daemon = ScriptedDaemon::start();
    let (mut app, active, hidden, pane, identity) = cached_chat();
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    let startup = app.initialize_runtimes();
    settle(&mut app, startup);
    let mut view = View::empty(hidden);
    view.panes.push(pane.clone());
    let canonical = model(identity, active, hidden, view);
    let connected = app.apply_workspace_model_update(ModelUpdate::Connected(canonical.clone()));
    settle(&mut app, connected);
    let scope = Scope::Channel(hidden);
    let failed = framed_seed(queued_seed(&rx, &scope), canonical, true);
    // Local presentation edits during the request must not make its Started
    // basis disappear: it is the immutable queued panes, not current panes.
    let title = app.card_state(pane.id).unwrap().title.clone();
    app.card_state_mut(pane.id).unwrap().title = "Renamed while the seed was in flight".into();
    apply_seed_updates(&mut app, vec![failed[0].clone()]);
    let ModelUpdate::ViewSeedRefused {
        scope: failed_scope,
        attempt,
        ..
    } = &failed[1]
    else {
        unreachable!()
    };
    apply_seed_updates(
        &mut app,
        vec![
            ModelUpdate::ViewSeedRefused {
                scope: failed_scope.clone(),
                attempt: *attempt,
                registry: Some(uuid::Uuid::new_v4()),
                error: "foreign registry".into(),
            },
            ModelUpdate::ViewSeedRefused {
                scope: failed_scope.clone(),
                attempt: uuid::Uuid::new_v4(),
                registry: Some(identity),
                error: "stale attempt".into(),
            },
        ],
    );
    assert!(app.shared_chat_admission_failure(pane.id).is_none());
    assert_eq!(app.startup_pane_check(), Ok(false));
    apply_seed_updates(&mut app, vec![failed[1].clone()]);
    let retry = app
        .shared_chat_admission_failure(pane.id)
        .unwrap()
        .retry()
        .unwrap();
    app.card_state_mut(pane.id).unwrap().title = title;
    // Every mutation is an actual owning source field, not a replacement token
    // fabricated from the current pane. Restore it only for the next control.
    app.workspace_model.identity = uuid::Uuid::new_v4();
    drop(app.retry_shared_chat_admission(retry.clone()));
    assert!(rx.is_empty());
    app.workspace_model.identity = identity;
    app.resume_ui.open = true;
    drop(app.retry_shared_chat_admission(retry.clone()));
    assert!(rx.is_empty(), "retry cannot bypass another modal");
    app.resume_ui.open = false;
    let existence = app.card_state(pane.id).unwrap().existence.clone();
    app.card_state_mut(pane.id).unwrap().existence = Existence::Closing {
        operation_id: crate::model::OperationId::new(),
    };
    drop(app.retry_shared_chat_admission(retry.clone()));
    assert!(rx.is_empty());
    app.card_state_mut(pane.id).unwrap().existence = existence;
    let original = app.card_state(pane.id).unwrap().kind.clone();
    let CardKind::Chat(mut replaced) = original.clone() else {
        unreachable!()
    };
    replaced
        .launch
        .recipe
        .arguments
        .push("changed-source".into());
    app.card_state_mut(pane.id).unwrap().kind = CardKind::Chat(replaced);
    drop(app.retry_shared_chat_admission(retry.clone()));
    assert!(rx.is_empty());
    app.card_state_mut(pane.id).unwrap().kind = original;
    assert!(app.shared_chat_admission_failure(pane.id).is_some());
    {
        let durable = Arc::make_mut(&mut app.durable);
        durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == hidden)
            .unwrap()
            .cards
            .retain(|id| *id != pane.id);
        durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == active)
            .unwrap()
            .cards
            .push(pane.id);
    }
    drop(app.retry_shared_chat_admission(retry.clone()));
    assert!(
        rx.is_empty(),
        "a same-card/same-session scope move invalidates old action"
    );
    {
        let durable = Arc::make_mut(&mut app.durable);
        durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == active)
            .unwrap()
            .cards
            .retain(|id| *id != pane.id);
        durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == hidden)
            .unwrap()
            .cards
            .push(pane.id);
        durable.local_only_cards.insert(pane.id);
    }
    drop(app.retry_shared_chat_admission(retry.clone()));
    assert!(rx.is_empty());
    Arc::make_mut(&mut app.durable)
        .local_only_cards
        .remove(&pane.id);
    assert!(app.shared_chat_admission_failure(pane.id).is_some());
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == scope)
        .unwrap()
        .panes
        .clear();
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(active))
        .unwrap()
        .panes
        .push(pane.clone());
    drop(app.retry_shared_chat_admission(retry.clone()));
    assert!(
        rx.is_empty(),
        "canonical foreign ownership cannot be seeded over"
    );
    app.workspace_model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(active))
        .unwrap()
        .panes
        .clear();
    app.shared_views
        .seed_admissions
        .get_mut(&scope)
        .unwrap()
        .attempt = Some(uuid::Uuid::new_v4());
    drop(app.retry_shared_chat_admission(retry.clone()));
    assert!(
        rx.is_empty(),
        "older failure cannot retry a newer operation"
    );
    app.shared_views
        .seed_admissions
        .get_mut(&scope)
        .unwrap()
        .attempt = Some(retry.0.attempt);
    assert!(app.shared_chat_admission_failure(pane.id).is_some());
    {
        let durable = Arc::make_mut(&mut app.durable);
        durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == hidden)
            .unwrap()
            .cards
            .retain(|id| *id != pane.id);
        durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == active)
            .unwrap()
            .cards
            .push(pane.id);
    }
    app.publish_view_changes();
    assert!(
        app.shared_chat_admission_failure(pane.id).is_none(),
        "the normal projection boundary removes the previous scope's error too"
    );
    {
        let durable = Arc::make_mut(&mut app.durable);
        durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == active)
            .unwrap()
            .cards
            .retain(|id| *id != pane.id);
        durable
            .channels
            .iter_mut()
            .find(|channel| channel.id == hidden)
            .unwrap()
            .cards
            .push(pane.id);
    }
    drop(app.retry_shared_chat_admission(retry));
    assert!(
        rx.is_empty(),
        "returning to the same scope cannot revive an invalidated action"
    );
    assert!(daemon.requests().is_empty());
    assert!(app.chat_awaiting_admission(pane.id));

    crate::chat::attachment_assets::with_test_image(|image| {
        let (mut app, active, hidden, pane, registry_a) = cached_chat();
        let scope = Scope::Channel(hidden);
        let CardKind::Chat(source) = &pane.kind else {
            unreachable!()
        };
        let source = source.clone();
        let draft = "Unsent across a warm registry replacement  ";
        app.chat_ui = chat_ui::State::with_draft_attachments(
            HashMap::from([(pane.id, draft.into())]),
            HashMap::from([(pane.id, vec![image.clone()])]),
        );
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.initialize_runtimes());
        let mut view = View::empty(hidden);
        view.panes.push(pane.clone());
        let canonical_a = model(registry_a, active, hidden, view);
        let connected =
            app.apply_workspace_model_update(ModelUpdate::Connected(canonical_a.clone()));
        settle(&mut app, connected);
        let refused_a = framed_seed(queued_seed(&rx, &scope), canonical_a.clone(), true);
        apply_seed_updates(&mut app, refused_a.clone());
        let retry_a = app
            .shared_chat_admission_failure(pane.id)
            .unwrap()
            .retry()
            .unwrap();
        drop(app.retry_shared_chat_admission(retry_a.clone()));
        let accepted_a = framed_seed(queued_seed(&rx, &scope), canonical_a.clone(), false);
        apply_seed_updates(&mut app, vec![accepted_a[0].clone()]);
        assert_eq!(
            app.workspace_model.identity, registry_a,
            "use a warm installed A"
        );
        assert_eq!(app.durable.workspace_model_identity, Some(registry_a));
        let CardRuntime::Chat(runtime) = &app.cards[&pane.id] else {
            unreachable!()
        };
        let allocation = &**runtime as *const chat_ui::Runtime as usize;
        let original_target = runtime.target;

        let registry_b = uuid::Uuid::new_v4();
        let mut missing_b = canonical_a.clone();
        missing_b.identity = registry_b;
        missing_b.channels.retain(|channel| channel.id != hidden);
        missing_b.views.retain(|view| view.scope != scope);
        let connected = app.apply_workspace_model_update(ModelUpdate::Connected(missing_b.clone()));
        settle(&mut app, connected);
        assert_eq!(app.workspace_model, missing_b);
        assert_eq!(app.durable.workspace_model_identity, Some(registry_b));
        assert!(
            app.durable
                .channels
                .iter()
                .any(|channel| channel.id == hidden && channel.cards.contains(&pane.id)),
            "a replacement registry has not deleted the old channel"
        );
        assert!(
            app.shared_views
                .seed_admissions
                .values()
                .all(|seed| seed.registry == registry_b)
        );
        let queued = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(
            queued
                .iter()
                .any(|edit| matches!(edit, ModelEdit::CreateChannel { id, .. } if *id == hidden))
        );
        let mut seeds = queued.into_iter().filter(
            |edit| matches!(edit, ModelEdit::SeedView { scope: queued, .. } if queued == &scope),
        );
        let seed_b = seeds
            .next()
            .expect("the installed B snapshot publishes its seed");
        assert!(
            seeds.next().is_none(),
            "join must not queue an A seed before B"
        );
        assert!(app.shared_views.seed_admissions[&scope].attempt.is_none());
        apply_seed_updates(&mut app, vec![refused_a[1].clone(), accepted_a[1].clone()]);
        drop(app.retry_shared_chat_admission(retry_a.clone()));
        assert!(app.shared_views.seed_admissions[&scope].attempt.is_none());
        assert!(!app.shared_views.seed_admissions[&scope].failed);
        assert!(rx.is_empty());

        let refused_b = framed_seed(seed_b, missing_b.clone(), true);
        apply_seed_updates(&mut app, vec![refused_b[0].clone()]);
        let ModelUpdate::ViewSeedStarted {
            attempt: attempt_b, ..
        } = &refused_b[0]
        else {
            unreachable!()
        };
        assert_eq!(
            app.shared_views.seed_admissions[&scope].attempt,
            Some(*attempt_b)
        );
        apply_seed_updates(&mut app, vec![refused_a[1].clone(), accepted_a[1].clone()]);
        assert_eq!(
            app.shared_views.seed_admissions[&scope].attempt,
            Some(*attempt_b)
        );
        apply_seed_updates(&mut app, vec![refused_b[1].clone()]);
        let failure_b = app.shared_chat_admission_failure(pane.id).unwrap();
        let retry_b = failure_b.retry().unwrap();
        apply_seed_updates(&mut app, vec![refused_a[1].clone(), accepted_a[1].clone()]);
        drop(app.retry_shared_chat_admission(retry_a.clone()));
        assert_eq!(
            app.shared_chat_admission_failure(pane.id).unwrap().retry(),
            Some(retry_b.clone())
        );
        assert_eq!(
            app.shared_views.seed_admissions[&scope].attempt,
            Some(*attempt_b)
        );
        assert!(app.startup_pane_check().is_err());
        let CardRuntime::Chat(runtime) = &app.cards[&pane.id] else {
            unreachable!()
        };
        assert_eq!(
            (
                &**runtime as *const chat_ui::Runtime as usize,
                runtime.target
            ),
            (allocation, original_target)
        );
        assert!(runtime.awaiting_admission());
        assert_eq!(runtime.session, source);
        assert_eq!(
            app.durable.pending_chat_admissions.get(&pane.id),
            Some(&source)
        );
        let saved = app.persistent_workspace();
        assert_eq!(saved.chat_drafts[&pane.id], draft);
        assert_eq!(saved.chat_draft_attachments[&pane.id], [image.clone()]);
        assert!(daemon.requests().is_empty());

        drop(app.retry_shared_chat_admission(retry_b));
        let seed_b = queued_seed(&rx, &scope);
        let mut admitted_b = canonical_a;
        admitted_b.identity = registry_b;
        admitted_b.sequence = missing_b.sequence + 1;
        let accepted_b = framed_seed(seed_b, admitted_b.clone(), false);
        apply_seed_updates(&mut app, vec![accepted_b[0].clone()]);
        let ModelUpdate::ViewSeedStarted {
            attempt: retry_attempt_b,
            ..
        } = &accepted_b[0]
        else {
            unreachable!()
        };
        apply_seed_updates(&mut app, vec![refused_a[1].clone(), accepted_a[1].clone()]);
        drop(app.retry_shared_chat_admission(retry_a));
        assert_eq!(
            app.shared_views.seed_admissions[&scope].registry,
            registry_b
        );
        assert_eq!(
            app.shared_views.seed_admissions[&scope].attempt,
            Some(*retry_attempt_b)
        );
        assert!(!app.shared_views.seed_admissions[&scope].failed);
        assert!(rx.is_empty());
        drop(app.apply_workspace_model_update(accepted_b[1].clone()));
        assert_eq!(app.workspace_model, admitted_b);
        assert!(app.shared_views.initialized.contains(&scope));
        assert!(!app.shared_views.seeding.contains(&scope));
        assert!(!app.shared_views.seed_admissions.contains_key(&scope));
        assert!(!app.durable.pending_chat_admissions.contains_key(&pane.id));
        assert!(app.chat_start_pending(pane.id));
        let CardRuntime::Chat(runtime) = &app.cards[&pane.id] else {
            unreachable!()
        };
        let admitted_target = runtime.target;
        drop(app.apply_workspace_model_update(accepted_b[1].clone()));
        let CardRuntime::Chat(runtime) = &app.cards[&pane.id] else {
            unreachable!()
        };
        assert_eq!(
            runtime.target, admitted_target,
            "B admits this source only once"
        );
        assert_eq!(app.local_view_cards(&scope), [pane.id]);
        let saved = app.persistent_workspace();
        assert_eq!(saved.chat_drafts[&pane.id], draft);
        assert_eq!(saved.chat_draft_attachments[&pane.id], [image.clone()]);
        assert!(rx.is_empty());
        assert!(
            daemon.requests().is_empty(),
            "registry recovery only authorizes the exact attachment task"
        );
    });
}

#[test]
fn failed_seed_retry_cannot_authorize_a_same_scope_canonical_successor() {
    if isolated("failed_seed_retry_cannot_authorize_a_same_scope_canonical_successor") {
        return;
    }
    use crate::app::workspace_model_sync::ModelUpdate;
    let daemon = ScriptedDaemon::start();
    for changed in ["session", "proof", "recipe", "title"] {
        let (mut app, active, hidden, pane, identity) = cached_chat();
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        let draft = "Original refused conversation draft  ";
        app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(pane.id, draft.into())]));
        let startup = app.initialize_runtimes();
        settle(&mut app, startup);
        let mut view = View::empty(hidden);
        view.panes.push(pane.clone());
        let canonical = model(identity, active, hidden, view);
        let connected = app.apply_workspace_model_update(ModelUpdate::Connected(canonical.clone()));
        settle(&mut app, connected);
        let scope = Scope::Channel(hidden);
        let failed = framed_seed(queued_seed(&rx, &scope), canonical.clone(), true);
        apply_seed_updates(&mut app, failed);
        let retry = app
            .shared_chat_admission_failure(pane.id)
            .unwrap()
            .retry()
            .unwrap();
        let CardKind::Chat(source) = &pane.kind else {
            unreachable!()
        };

        // A real newer watcher snapshot can replace the canonical actor while
        // failed seeding intentionally keeps its old local runtime unprojected.
        let mut newer = canonical;
        newer.sequence += 1;
        let successor = newer
            .views
            .iter_mut()
            .find(|view| view.scope == scope)
            .unwrap();
        let successor = successor
            .panes
            .iter_mut()
            .find(|entry| entry.id == pane.id)
            .unwrap();
        let CardKind::Chat(session) = &mut successor.kind else {
            unreachable!()
        };
        match changed {
            "session" => session.id = TerminalId::new(),
            "proof" => session.proof.as_mut().unwrap().host_incarnation = "new-incarnation".into(),
            "recipe" => session.launch.recipe.arguments.push("new-source".into()),
            "title" => successor.title = "Newer presentation only".into(),
            _ => unreachable!(),
        }
        let read = app.apply_workspace_model_update(ModelUpdate::Read(newer.clone()));
        settle(&mut app, read);
        assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        assert!(app.chat_awaiting_admission(pane.id));
        assert_eq!(
            app.durable.pending_chat_admissions.get(&pane.id),
            Some(source)
        );
        let task = app.retry_shared_chat_admission(retry);
        settle(&mut app, task);
        if changed == "title" {
            let next = queued_seed(&rx, &scope);
            assert!(matches!(next, ModelEdit::SeedView { .. }));
        } else {
            assert!(
                rx.is_empty(),
                "{changed}: an old retry cannot admit a newer canonical actor"
            );
            assert_eq!(app.workspace_model, newer);
            assert!(app.shared_views.seed_admissions.get(&scope).unwrap().failed);
            let failure = app.shared_chat_admission_failure(pane.id).unwrap();
            assert!(
                failure.retry().is_none(),
                "do not leave an ineffective retry available"
            );
            assert!(failure.message().contains("Close and reopen this window"));
        }
        assert_eq!(
            app.persistent_workspace()
                .chat_drafts
                .get(&pane.id)
                .map(String::as_str),
            Some(draft),
            "{changed}: the original unsent draft remains intact"
        );
        assert!(
            daemon.requests().is_empty(),
            "retry is never provider admission"
        );
    }
}

#[test]
fn failed_scope_retry_preserves_a_changed_deleted_or_closed_sibling_and_both_drafts() {
    if isolated("failed_scope_retry_preserves_a_changed_deleted_or_closed_sibling_and_both_drafts")
    {
        return;
    }
    use crate::app::workspace_model_sync::ModelUpdate;
    let daemon = ScriptedDaemon::start();
    for changed in [
        "session",
        "proof",
        "recipe",
        "canonical_deleted",
        "deleted_before_refusal",
        "canonical_retired",
        "canonical_moved",
        "canonical_moved_pruned",
        "canonical_moved_managed",
        "canonical_moved_managed_pruned",
        "initial_moved",
        "initial_moved_managed",
        "initial_moved_before_started",
        "initial_moved_managed_before_started",
        "local_closed",
        "local_removed",
        "title",
        "legacy_unshared",
    ] {
        let (mut app, active, hidden, pane, identity) = cached_chat();
        let mut sibling = pane.clone();
        sibling.id = CardId::new();
        let CardKind::Chat(session) = &mut sibling.kind else {
            unreachable!()
        };
        session.id = TerminalId::new();
        session.launch.session_id = Some("00000000-0000-4000-8000-000000000002".into());
        let mut sibling_card = app.card_state(pane.id).unwrap().clone();
        sibling_card.id = sibling.id;
        sibling_card.kind = sibling.kind.clone();
        {
            let durable = Arc::make_mut(&mut app.durable);
            durable.cards.push(sibling_card);
            durable
                .channels
                .iter_mut()
                .find(|channel| channel.id == hidden)
                .unwrap()
                .cards
                .push(sibling.id);
        }
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.shared_views = State::for_workspace(&app.durable);
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        let drafts = HashMap::from([
            (pane.id, "Unchanged retry pane draft  ".to_owned()),
            (sibling.id, "Original failed sibling draft  ".to_owned()),
        ]);
        app.chat_ui = chat_ui::State::with_drafts(drafts.clone());
        let startup = app.initialize_runtimes();
        settle(&mut app, startup);
        let mut view = View::empty(hidden);
        view.panes = vec![pane.clone()];
        if changed != "legacy_unshared" {
            view.panes.push(sibling.clone());
        }
        let mut canonical = model(identity, active, hidden, view);
        if changed.contains("managed") {
            canonical.agents = vec![managed_chat(&sibling, Scope::Channel(hidden))];
        }
        let connected = app.apply_workspace_model_update(ModelUpdate::Connected(canonical.clone()));
        settle(&mut app, connected);
        let scope = Scope::Channel(hidden);
        let source_seed =
            acknowledge_destination_seed(&mut app, &rx, &scope, Scope::Channel(active), &canonical);
        let failed = framed_seed(source_seed, canonical.clone(), true);
        if changed.starts_with("initial_moved") {
            let before_started = changed.ends_with("before_started");
            let source_members = app.local_view_cards(&scope);
            let destination = Scope::Channel(active);
            let destination_members = app.local_view_cards(&destination);
            let allocations = [&pane, &sibling].map(|pane| {
                let CardRuntime::Chat(runtime) = &app.cards[&pane.id] else {
                    unreachable!()
                };
                (
                    &**runtime as *const chat_ui::Runtime as usize,
                    runtime.target,
                )
            });
            if !before_started {
                apply_seed_updates(&mut app, vec![failed[0].clone()]);
            }
            let mut moved = canonical.clone();
            moved.sequence += 1;
            moved
                .views
                .iter_mut()
                .find(|view| view.scope == scope)
                .unwrap()
                .panes
                .retain(|pane| pane.id != sibling.id);
            moved
                .views
                .iter_mut()
                .find(|view| view.scope == destination)
                .unwrap()
                .panes
                .push(sibling.clone());
            if changed.contains("managed") {
                moved.agents = vec![managed_chat(&sibling, destination.clone())];
                assert!(
                    crate::workspace_model::views::managed_display(&moved, &moved.agents[0])
                        .is_some_and(|display| display.scope == destination)
                );
            }
            let read = app.apply_workspace_model_update(ModelUpdate::Read(moved.clone()));
            settle(&mut app, read);
            for refused in [false, true] {
                if refused {
                    if before_started {
                        apply_seed_updates(&mut app, vec![failed[0].clone()]);
                    }
                    apply_seed_updates(&mut app, vec![failed[1].clone()]);
                }
                let seed = &app.shared_views.seed_admissions[&scope];
                assert_eq!(seed.failed, refused, "{changed}");
                if !refused {
                    assert!(seed.failed_chats.is_empty() && seed.retry_sources.is_empty());
                    assert_eq!(seed.attempt.is_none(), before_started);
                }
                assert_eq!(app.local_view_cards(&scope), source_members, "{changed}");
                assert_eq!(
                    app.local_view_cards(&destination),
                    destination_members,
                    "{changed}"
                );
                for (index, original) in [&pane, &sibling].iter().enumerate() {
                    let CardKind::Chat(source) = &original.kind else {
                        unreachable!()
                    };
                    let CardRuntime::Chat(runtime) = &app.cards[&original.id] else {
                        unreachable!()
                    };
                    assert_eq!(
                        (
                            &**runtime as *const chat_ui::Runtime as usize,
                            runtime.target
                        ),
                        allocations[index],
                        "{changed}"
                    );
                    assert!(runtime.awaiting_admission(), "{changed}");
                    assert_eq!(&runtime.session, source);
                    assert_eq!(
                        app.durable.pending_chat_admissions.get(&original.id),
                        Some(source)
                    );
                    let failure = app.shared_chat_admission_failure(original.id);
                    if refused {
                        let failure = failure.unwrap();
                        assert!(failure.retry().is_none());
                        assert!(failure.message().contains("Close and reopen this window"));
                    } else {
                        assert!(failure.is_none(), "a watcher is not a refusal: {changed}");
                    }
                }
                let saved = app.persistent_workspace();
                for original in [&pane, &sibling] {
                    assert_eq!(
                        saved.chat_drafts.get(&original.id),
                        drafts.get(&original.id)
                    );
                }
                assert_eq!(app.workspace_model, moved);
                assert!(daemon.requests().is_empty(), "{changed}");
                assert!(rx.is_empty(), "{changed}: no compensating transfer or seed");
            }
            continue;
        }
        if changed == "deleted_before_refusal" {
            let mut deleted = canonical.clone();
            deleted.sequence += 1;
            deleted
                .views
                .iter_mut()
                .find(|view| view.scope == scope)
                .unwrap()
                .panes
                .retain(|entry| entry.id != sibling.id);
            let read = app.apply_workspace_model_update(ModelUpdate::Read(deleted));
            settle(&mut app, read);
        }
        apply_seed_updates(&mut app, failed);
        let retry = app
            .shared_chat_admission_failure(pane.id)
            .unwrap()
            .retry()
            .unwrap();
        let sibling_retry = app
            .shared_chat_admission_failure(sibling.id)
            .unwrap()
            .retry()
            .unwrap();
        let attempt = app.shared_views.seed_admissions[&scope].attempt;
        let source_members = app.local_view_cards(&scope);
        let destination_members = app.local_view_cards(&Scope::Channel(active));

        // The retained action belongs to A. Only B's current source/placement
        // changes in the actual newer watcher snapshot, while projection waits.
        let mut newer = canonical;
        newer.sequence += 1;
        let hidden_view = newer
            .views
            .iter_mut()
            .find(|view| view.scope == scope)
            .unwrap();
        if matches!(
            changed,
            "canonical_deleted"
                | "deleted_before_refusal"
                | "canonical_retired"
                | "canonical_moved"
                | "canonical_moved_pruned"
                | "canonical_moved_managed"
                | "canonical_moved_managed_pruned"
                | "legacy_unshared"
        ) {
            hidden_view.panes.retain(|entry| entry.id != sibling.id);
            if changed == "canonical_retired" {
                hidden_view.retired.push(sibling.id);
            }
        } else {
            let entry = hidden_view
                .panes
                .iter_mut()
                .find(|entry| entry.id == sibling.id)
                .unwrap();
            let CardKind::Chat(session) = &mut entry.kind else {
                unreachable!()
            };
            match changed {
                "session" => session.id = TerminalId::new(),
                "proof" => session.proof.as_mut().unwrap().host_incarnation = "new-sibling".into(),
                "recipe" => session
                    .launch
                    .recipe
                    .arguments
                    .push("new-sibling-source".into()),
                "title" => entry.title = "New sibling title only".into(),
                "local_closed" | "local_removed" => {}
                _ => unreachable!(),
            }
        }
        if matches!(
            changed,
            "canonical_moved"
                | "canonical_moved_pruned"
                | "canonical_moved_managed"
                | "canonical_moved_managed_pruned"
        ) {
            newer
                .views
                .iter_mut()
                .find(|view| view.scope == Scope::Channel(active))
                .unwrap()
                .panes
                .push(sibling.clone());
            if !newer.agents.is_empty() {
                newer.agents = vec![managed_chat(&sibling, Scope::Channel(active))];
                assert!(
                    crate::workspace_model::views::managed_display(&newer, &newer.agents[0])
                        .is_some_and(|display| display.scope == Scope::Channel(active))
                );
            }
        }
        if matches!(
            changed,
            "canonical_moved_pruned" | "canonical_moved_managed_pruned"
        ) {
            // A normal publication between installing authority and projecting
            // it can prune the old row. The original refusal still owns B.
            assert!(app.install_workspace_model_snapshot(newer.clone()));
            app.publish_view_changes();
            assert!(app.shared_chat_admission_failure(sibling.id).is_none());
        }
        let read = app.apply_workspace_model_update(ModelUpdate::Read(newer.clone()));
        settle(&mut app, read);
        if matches!(
            changed,
            "canonical_moved"
                | "canonical_moved_pruned"
                | "canonical_moved_managed"
                | "canonical_moved_managed_pruned"
        ) {
            assert_eq!(app.local_view_cards(&scope), source_members);
            assert_eq!(
                app.local_view_cards(&Scope::Channel(active)),
                destination_members
            );
            let failure = app.shared_chat_admission_failure(sibling.id).unwrap();
            assert!(failure.retry().is_none());
            assert!(failure.message().contains("Close and reopen this window"));
        }
        if changed == "local_closed" {
            app.card_state_mut(sibling.id).unwrap().existence = Existence::Closing {
                operation_id: crate::model::OperationId::new(),
            };
        } else if changed == "local_removed" {
            Arc::make_mut(&mut app.durable)
                .channels
                .iter_mut()
                .find(|channel| channel.id == hidden)
                .unwrap()
                .cards
                .retain(|id| *id != sibling.id);
        }
        app.publish_view_changes();
        let before_cards = [pane.id, sibling.id].map(|id| app.card_state(id).unwrap().clone());
        let before_members = app.local_view_cards(&scope);
        let task = app.retry_shared_chat_admission(retry);
        settle(&mut app, task);
        if matches!(changed, "title" | "legacy_unshared") {
            let ModelEdit::SeedView { panes, .. } = queued_seed(&rx, &scope) else {
                unreachable!()
            };
            assert!(
                panes
                    .iter()
                    .any(|entry| entry.id == pane.id && entry.kind == pane.kind)
            );
            assert!(
                panes
                    .iter()
                    .any(|entry| entry.id == sibling.id && entry.kind == sibling.kind)
            );
        } else {
            assert!(
                rx.is_empty(),
                "{changed}: A cannot release B's failed admission"
            );
            let seed = &app.shared_views.seed_admissions[&scope];
            assert!(seed.failed && !seed.retry_available);
            assert_eq!(seed.attempt, attempt);
            assert!(app.shared_views.seeding.contains(&scope));
            let failure = app.shared_chat_admission_failure(pane.id).unwrap();
            assert!(failure.retry().is_none());
            assert!(failure.message().contains("Close and reopen this window"));
            let stale = app.retry_shared_chat_admission(sibling_retry);
            settle(&mut app, stale);
            assert!(
                rx.is_empty(),
                "a sibling's retained action cannot reopen that scope either"
            );
        }
        assert_eq!(
            app.workspace_model, newer,
            "{changed}: preserve authoritative sibling changes"
        );
        assert_eq!(
            [pane.id, sibling.id].map(|id| app.card_state(id).unwrap().clone()),
            before_cards
        );
        assert_eq!(app.local_view_cards(&scope), before_members);
        for original in [&pane, &sibling] {
            let CardKind::Chat(source) = &original.kind else {
                unreachable!()
            };
            assert!(app.chat_awaiting_admission(original.id));
            assert_eq!(
                app.durable.pending_chat_admissions.get(&original.id),
                Some(source)
            );
            let CardRuntime::Chat(runtime) = app.cards.get(&original.id).unwrap() else {
                unreachable!()
            };
            assert_eq!(&runtime.session, source);
            assert_eq!(
                app.persistent_workspace().chat_drafts.get(&original.id),
                drafts.get(&original.id)
            );
        }
        assert!(
            daemon.requests().is_empty(),
            "retry never attaches either provider"
        );
    }
}

#[test]
fn retried_seed_ack_preserves_every_failed_source_across_reordered_snapshots() {
    if isolated("retried_seed_ack_preserves_every_failed_source_across_reordered_snapshots") {
        return;
    }
    use crate::app::workspace_model_sync::ModelUpdate;
    let daemon = ScriptedDaemon::start();
    crate::chat::attachment_assets::with_test_image(|image| {
        for changed_card in [0, 1] {
            for changed in [
                "session",
                "proof",
                "recipe",
                "canonical_moved",
                "canonical_moved_managed",
                "initial_moved",
                "initial_moved_managed",
                "title",
                "legacy_unshared",
            ] {
                for earlier_read in [false, true] {
                    let case =
                        format!("card {changed_card}, {changed}, earlier read {earlier_read}");
                    let (mut app, active, hidden, pane, identity) = cached_chat();
                    let mut sibling = pane.clone();
                    sibling.id = CardId::new();
                    let CardKind::Chat(session) = &mut sibling.kind else {
                        unreachable!()
                    };
                    session.id = TerminalId::new();
                    session.launch.session_id = Some("00000000-0000-4000-8000-000000000002".into());
                    let mut state = app.card_state(pane.id).unwrap().clone();
                    state.id = sibling.id;
                    state.kind = sibling.kind.clone();
                    {
                        let durable = Arc::make_mut(&mut app.durable);
                        durable.cards.push(state);
                        durable
                            .channels
                            .iter_mut()
                            .find(|channel| channel.id == hidden)
                            .unwrap()
                            .cards
                            .push(sibling.id);
                    }
                    app.workspace = Cagents::rebuild_workspace(&app.durable);
                    app.shared_views = State::for_workspace(&app.durable);
                    let mut destination_pane = pane.clone();
                    destination_pane.id = CardId::new();
                    destination_pane.title = "Destination browser".into();
                    destination_pane.kind = CardKind::Browser(BrowserSession {
                        id: BrowserId::new(),
                        current_url: "https://example.com".parse().unwrap(),
                        history: vec![],
                        history_index: 0,
                        scroll_x: 0.,
                        scroll_y: 0.,
                        fidelity: BrowserRestoreFidelity::UrlOnly,
                    });
                    let originals = [pane, sibling];
                    let drafts = HashMap::from([
                        (originals[0].id, "Clicked conversation draft  ".to_owned()),
                        (originals[1].id, "Sibling conversation draft  ".to_owned()),
                    ]);
                    app.chat_ui = chat_ui::State::with_draft_attachments(
                        drafts.clone(),
                        originals
                            .iter()
                            .map(|pane| (pane.id, vec![image.clone()]))
                            .collect(),
                    );
                    let (tx, rx) = async_channel::unbounded();
                    app.workspace_model_tx = tx;
                    drop(app.initialize_runtimes());
                    let scope = Scope::Channel(hidden);
                    let mut view = View::empty(hidden);
                    view.panes = originals
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| changed != "legacy_unshared" || *index != changed_card)
                        .map(|(_, pane)| pane.clone())
                        .collect();
                    let mut canonical = model(identity, active, hidden, view);
                    let destination = Scope::Channel(active);
                    canonical
                        .views
                        .iter_mut()
                        .find(|view| view.scope == destination)
                        .unwrap()
                        .panes
                        .push(destination_pane.clone());
                    if changed.contains("managed") {
                        canonical.agents =
                            vec![managed_chat(&originals[changed_card], scope.clone())];
                    }
                    drop(
                        app.apply_workspace_model_update(ModelUpdate::Connected(canonical.clone())),
                    );
                    let source_seed = acknowledge_destination_seed(
                        &mut app,
                        &rx,
                        &scope,
                        destination.clone(),
                        &canonical,
                    );
                    if changed.starts_with("initial_moved") {
                        let source_members = app.local_view_cards(&scope);
                        let destination_members = app.local_view_cards(&destination);
                        let allocations = originals.each_ref().map(|pane| {
                            let CardRuntime::Chat(runtime) = &app.cards[&pane.id] else {
                                unreachable!()
                            };
                            (
                                &**runtime as *const chat_ui::Runtime as usize,
                                runtime.target,
                            )
                        });
                        let mut moved = canonical.clone();
                        moved.sequence += 1;
                        moved
                            .views
                            .iter_mut()
                            .find(|view| view.scope == scope)
                            .unwrap()
                            .panes
                            .retain(|pane| pane.id != originals[changed_card].id);
                        let destination_view = moved
                            .views
                            .iter_mut()
                            .find(|view| view.scope == destination)
                            .unwrap();
                        destination_view.panes[0].title = "Updated destination browser".into();
                        destination_view.panes.push(originals[changed_card].clone());
                        if changed.contains("managed") {
                            moved.agents =
                                vec![managed_chat(&originals[changed_card], destination.clone())];
                            assert!(
                                crate::workspace_model::views::managed_display(
                                    &moved,
                                    &moved.agents[0],
                                )
                                .is_some_and(|display| display.scope == destination)
                            );
                        }
                        let mut returned = canonical.clone();
                        returned.sequence = moved.sequence + 1;
                        returned
                            .views
                            .iter_mut()
                            .find(|view| view.scope == destination)
                            .unwrap()
                            .panes[0]
                            .title = "Updated destination browser".into();
                        let accepted = framed_seed(
                            source_seed,
                            if earlier_read {
                                returned.clone()
                            } else {
                                moved.clone()
                            },
                            false,
                        );
                        apply_seed_updates(&mut app, vec![accepted[0].clone()]);
                        if earlier_read {
                            let read =
                                app.apply_workspace_model_update(ModelUpdate::Read(moved.clone()));
                            settle(&mut app, read);
                            assert!(!app.shared_views.seed_admissions[&scope].failed, "{case}");
                            assert_eq!(app.local_view_cards(&scope), source_members, "{case}");
                            assert_eq!(
                                app.local_view_cards(&destination),
                                destination_members,
                                "{case}"
                            );
                            for (index, pane) in originals.iter().enumerate() {
                                let CardRuntime::Chat(runtime) = &app.cards[&pane.id] else {
                                    unreachable!()
                                };
                                assert_eq!(
                                    (
                                        &**runtime as *const chat_ui::Runtime as usize,
                                        runtime.target
                                    ),
                                    allocations[index]
                                );
                                assert!(runtime.awaiting_admission(), "{case}");
                                assert!(
                                    app.shared_chat_admission_failure(pane.id).is_none(),
                                    "{case}"
                                );
                            }
                            assert!(daemon.requests().is_empty(), "{case}");
                        }
                        let ack = app.apply_workspace_model_update(accepted[1].clone());
                        if earlier_read {
                            drop(ack);
                            assert!(
                                !app.shared_views.seed_admissions.contains_key(&scope),
                                "{case}"
                            );
                            for pane in &originals {
                                assert!(
                                    !app.durable.pending_chat_admissions.contains_key(&pane.id),
                                    "{case}"
                                );
                                assert!(app.chat_start_pending(pane.id), "{case}");
                            }
                            let admitted_targets = originals.each_ref().map(|pane| {
                                let CardRuntime::Chat(runtime) = &app.cards[&pane.id] else {
                                    unreachable!()
                                };
                                runtime.target
                            });
                            drop(app.apply_workspace_model_update(accepted[1].clone()));
                            for (index, pane) in originals.iter().enumerate() {
                                let CardRuntime::Chat(runtime) = &app.cards[&pane.id] else {
                                    unreachable!()
                                };
                                assert_eq!(runtime.target, admitted_targets[index], "{case}");
                                assert!(app.chat_start_pending(pane.id), "{case}");
                            }
                            // Once the original ACK succeeds, the same peer move
                            // is ordinary canonical projection, not a lasting fence.
                            moved.sequence = returned.sequence + 1;
                            let read =
                                app.apply_workspace_model_update(ModelUpdate::Read(moved.clone()));
                            settle(&mut app, read);
                            assert!(
                                !app.local_view_cards(&scope)
                                    .contains(&originals[changed_card].id),
                                "{case}"
                            );
                            assert!(
                                app.local_view_cards(&destination)
                                    .contains(&originals[changed_card].id),
                                "{case}"
                            );
                        } else {
                            settle(&mut app, ack);
                            assert!(app.shared_views.seed_admissions[&scope].failed, "{case}");
                            assert_eq!(app.local_view_cards(&scope), source_members, "{case}");
                            assert_eq!(
                                app.local_view_cards(&destination),
                                destination_members,
                                "{case}"
                            );
                            for (index, pane) in originals.iter().enumerate() {
                                let CardKind::Chat(source) = &pane.kind else {
                                    unreachable!()
                                };
                                let CardRuntime::Chat(runtime) = &app.cards[&pane.id] else {
                                    unreachable!()
                                };
                                assert_eq!(
                                    (
                                        &**runtime as *const chat_ui::Runtime as usize,
                                        runtime.target
                                    ),
                                    allocations[index]
                                );
                                assert!(runtime.awaiting_admission(), "{case}");
                                assert_eq!(
                                    app.durable.pending_chat_admissions.get(&pane.id),
                                    Some(source),
                                    "{case}"
                                );
                                let failure = app.shared_chat_admission_failure(pane.id).unwrap();
                                assert!(failure.retry().is_none(), "{case}");
                                assert!(
                                    failure.message().contains("Close and reopen this window"),
                                    "{case}"
                                );
                            }
                        }
                        assert_eq!(app.workspace_model, moved, "{case}");
                        assert_eq!(
                            app.card_state(destination_pane.id).unwrap().title,
                            "Updated destination browser",
                            "{case}"
                        );
                        let saved = app.persistent_workspace();
                        for pane in &originals {
                            assert_eq!(
                                saved.chat_drafts.get(&pane.id),
                                drafts.get(&pane.id),
                                "{case}"
                            );
                            assert_eq!(
                                saved.chat_draft_attachments[&pane.id],
                                [image.clone()],
                                "{case}"
                            );
                        }
                        assert!(daemon.requests().is_empty(), "{case}");
                        assert!(rx.is_empty(), "{case}: no compensating transfer or seed");
                        continue;
                    }
                    let refused = framed_seed(source_seed, canonical.clone(), true);
                    apply_seed_updates(&mut app, refused);
                    let source_members = app.local_view_cards(&scope);
                    let destination_members = app.local_view_cards(&destination);
                    let retry = app
                        .shared_chat_admission_failure(originals[0].id)
                        .unwrap()
                        .retry()
                        .unwrap();
                    let allocations = originals.each_ref().map(|pane| {
                        let CardRuntime::Chat(runtime) = &app.cards[&pane.id] else {
                            unreachable!()
                        };
                        (
                            &**runtime as *const chat_ui::Runtime as usize,
                            runtime.target,
                        )
                    });
                    drop(app.retry_shared_chat_admission(retry.clone()));
                    assert!(app.shared_chat_admission_failure(originals[0].id).is_none());
                    let next = queued_seed(&rx, &scope);
                    let mut newer = canonical.clone();
                    newer.sequence += 1;
                    let view = newer
                        .views
                        .iter_mut()
                        .find(|view| view.scope == scope)
                        .unwrap();
                    if matches!(changed, "canonical_moved" | "canonical_moved_managed") {
                        view.panes
                            .retain(|pane| pane.id != originals[changed_card].id);
                        let destination_view = newer
                            .views
                            .iter_mut()
                            .find(|view| view.scope == destination)
                            .unwrap();
                        destination_view.panes[0].title = "Updated destination browser".into();
                        destination_view.panes.push(originals[changed_card].clone());
                        if changed == "canonical_moved_managed" {
                            newer.agents =
                                vec![managed_chat(&originals[changed_card], destination.clone())];
                            assert!(
                                crate::workspace_model::views::managed_display(
                                    &newer,
                                    &newer.agents[0],
                                )
                                .is_some_and(|display| display.scope == destination)
                            );
                        }
                    } else if changed == "legacy_unshared" {
                        view.panes.push(originals[changed_card].clone());
                    } else {
                        let successor = view
                            .panes
                            .iter_mut()
                            .find(|pane| pane.id == originals[changed_card].id)
                            .unwrap();
                        let CardKind::Chat(session) = &mut successor.kind else {
                            unreachable!()
                        };
                        match changed {
                            "session" => session.id = TerminalId::new(),
                            "proof" => {
                                session.proof.as_mut().unwrap().host_incarnation =
                                    "new-owner".into()
                            }
                            "recipe" => session.launch.recipe.arguments.push("new-source".into()),
                            "title" => successor.title = "Presentation only".into(),
                            _ => unreachable!(),
                        }
                    }
                    // Real publisher order is Started then ACK. A watcher can
                    // supply newer authority in between, or the reply can be
                    // the first evidence of a successor. An older ACK must not
                    // erase the newer watcher snapshot before this check.
                    let accepted = framed_seed(
                        next,
                        if earlier_read {
                            canonical
                        } else {
                            newer.clone()
                        },
                        false,
                    );
                    apply_seed_updates(&mut app, vec![accepted[0].clone()]);
                    if earlier_read {
                        let read =
                            app.apply_workspace_model_update(ModelUpdate::Read(newer.clone()));
                        settle(&mut app, read);
                        if matches!(changed, "canonical_moved" | "canonical_moved_managed") {
                            assert_eq!(app.local_view_cards(&scope), source_members, "{case}");
                            assert_eq!(
                                app.local_view_cards(&destination),
                                destination_members,
                                "{case}"
                            );
                            for pane in &originals {
                                assert!(app.chat_awaiting_admission(pane.id), "{case}");
                                assert!(
                                    app.durable.pending_chat_admissions.contains_key(&pane.id),
                                    "{case}"
                                );
                            }
                            assert!(daemon.requests().is_empty(), "{case}");
                        }
                    }
                    let ack = app.apply_workspace_model_update(accepted[1].clone());
                    let allowed = matches!(changed, "title" | "legacy_unshared");
                    if allowed {
                        // Positive controls inspect synchronous admission; do
                        // not run provider work for these source-only controls.
                        drop(ack);
                        assert!(app.shared_views.initialized.contains(&scope), "{case}");
                        assert!(!app.shared_views.seeding.contains(&scope), "{case}");
                        assert!(
                            !app.shared_views.seed_admissions.contains_key(&scope),
                            "{case}"
                        );
                        for pane in &originals {
                            assert!(
                                !app.durable.pending_chat_admissions.contains_key(&pane.id),
                                "{case}"
                            );
                            assert!(app.chat_start_pending(pane.id), "{case}");
                        }
                    } else {
                        settle(&mut app, ack);
                        assert!(!app.shared_views.initialized.contains(&scope), "{case}");
                        assert!(app.shared_views.seeding.contains(&scope), "{case}");
                        let failure = app.shared_chat_admission_failure(originals[0].id).unwrap();
                        assert!(failure.retry().is_none(), "{case}");
                        assert!(
                            failure.message().contains("Close and reopen this window"),
                            "{case}"
                        );
                        assert!(app.startup_pane_check().is_err(), "{case}");
                        drop(app.retry_shared_chat_admission(retry));
                        assert!(
                            rx.is_empty(),
                            "{case}: old callback cannot release the refused ACK"
                        );
                        for (index, pane) in originals.iter().enumerate() {
                            let CardRuntime::Chat(runtime) = &app.cards[&pane.id] else {
                                unreachable!()
                            };
                            assert_eq!(
                                (
                                    &**runtime as *const chat_ui::Runtime as usize,
                                    runtime.target
                                ),
                                allocations[index],
                                "{case}"
                            );
                            assert_eq!(
                                CardKind::Chat(runtime.session.clone()),
                                pane.kind,
                                "{case}"
                            );
                            assert!(runtime.awaiting_admission(), "{case}");
                            assert!(!app.chat_start_pending(pane.id), "{case}");
                            let CardKind::Chat(source) = &pane.kind else {
                                unreachable!()
                            };
                            assert_eq!(
                                app.durable.pending_chat_admissions.get(&pane.id),
                                Some(source),
                                "{case}"
                            );
                        }
                        assert_eq!(app.local_view_cards(&scope), source_members, "{case}");
                        assert_eq!(
                            app.local_view_cards(&destination),
                            destination_members,
                            "{case}"
                        );
                        if matches!(changed, "canonical_moved" | "canonical_moved_managed") {
                            assert_eq!(
                                app.card_state(destination_pane.id).unwrap().title,
                                "Updated destination browser",
                                "the admitted destination still projects its unrelated pane: {case}"
                            );
                        }
                    }
                    assert_eq!(app.workspace_model, newer, "{case}");
                    let saved = app.persistent_workspace();
                    for pane in &originals {
                        assert_eq!(
                            saved.chat_drafts.get(&pane.id),
                            drafts.get(&pane.id),
                            "{case}"
                        );
                        assert_eq!(
                            saved.chat_draft_attachments[&pane.id],
                            [image.clone()],
                            "{case}"
                        );
                    }
                    assert!(
                        daemon.requests().is_empty(),
                        "{case}: changed ACK cannot attach a successor"
                    );
                }
            }
        }
    });
}

#[test]
fn closed_or_full_publisher_reports_seed_failure_without_admitting_cached_chat() {
    if isolated("closed_or_full_publisher_reports_seed_failure_without_admitting_cached_chat") {
        return;
    }
    use crate::app::workspace_model_sync::ModelUpdate;
    let daemon = ScriptedDaemon::start();
    for closed in [false, true] {
        let (mut app, active, hidden, pane, identity) = cached_chat();
        let (tx, rx) = async_channel::bounded(1);
        // Full and Closed both fail actual try_send, without an observer thread.
        tx.try_send(ModelEdit::SeedView {
            scope: Scope::Channel(active),
            panes: vec![],
            closed: vec![],
        })
        .unwrap();
        if closed {
            rx.close();
        }
        app.workspace_model_tx = tx;
        let startup = app.initialize_runtimes();
        settle(&mut app, startup);
        let mut view = View::empty(hidden);
        view.panes.push(pane.clone());
        let connected = app.apply_workspace_model_update(ModelUpdate::Connected(model(
            identity, active, hidden, view,
        )));
        settle(&mut app, connected);
        let failure = app.shared_chat_admission_failure(pane.id).unwrap();
        assert!(
            failure
                .message()
                .contains(if closed { "stopped" } else { "full" })
        );
        assert!(app.startup_pane_check().is_err());
        if closed {
            assert!(failure.message().contains("close and reopen this window"));
            assert!(
                failure.retry().is_none(),
                "a stopped publisher must not offer a dead-end retry"
            );
        } else {
            let old = failure.retry().unwrap();
            drop(app.retry_shared_chat_admission(old.clone()));
            let new = app
                .shared_chat_admission_failure(pane.id)
                .unwrap()
                .retry()
                .unwrap();
            assert_ne!(
                old, new,
                "a refused queue retry remains a new finite failure"
            );
            drop(app.retry_shared_chat_admission(old));
            assert_eq!(
                app.shared_chat_admission_failure(pane.id).unwrap().retry(),
                Some(new)
            );
        }
        assert!(app.chat_awaiting_admission(pane.id));
        assert_eq!(rx.len(), 1, "no duplicate provider or hidden queue growth");
    }
    assert!(daemon.requests().is_empty());
}
#[path = "../../chat_ui/pending_admission_retry_view_tests.rs"]
mod retry_view;
