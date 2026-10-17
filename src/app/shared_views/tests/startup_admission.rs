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
        atomic::{AtomicBool, Ordering},
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

struct ScriptedDaemon {
    providers: Arc<Mutex<Vec<Session>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ScriptedDaemon {
    fn start() -> Self {
        let socket = crate::terminal_ipc::daemon_socket_path().unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let providers = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let recorded = providers.clone();
        let stop = stopped.clone();
        let worker = std::thread::spawn(move || {
            let proof = HostSessionProof {
                host_session_id: "live-host".into(),
                boot_id: "boot".into(),
                host_incarnation: "live-incarnation".into(),
            };
            while !stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("scripted daemon accept: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                while let Some(request) = read_frame::<ClientEnvelope>(&mut stream).unwrap() {
                    let message = match request.request {
                        ClientRequest::Connect { .. } => ServerMessage::Response {
                            response: ServerResponse::Connected {
                                proof: proof.clone(),
                                resumed: Vec::new(),
                            },
                        },
                        ClientRequest::ResolveUpgradeChat { .. } => {
                            ServerMessage::Error(ProtocolError::new(
                                ProtocolErrorCode::TerminalUnavailable,
                                "No upgrade lineage in this fixture",
                                false,
                            ))
                        }
                        ClientRequest::ChatStart { session, .. }
                        | ClientRequest::ChatAttach { session } => {
                            recorded.lock().unwrap().push(session);
                            // Record actual provider admission on the wire, but
                            // never start a provider or create a reader worker.
                            ServerMessage::Error(ProtocolError::new(
                                ProtocolErrorCode::TerminalUnavailable,
                                "Scripted provider admission recorded",
                                false,
                            ))
                        }
                        other => panic!("unexpected startup request: {other:?}"),
                    };
                    write_frame(
                        &mut stream,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(request.request_id),
                            proof: proof.clone(),
                            message,
                        },
                    )
                    .unwrap();
                }
            }
        });
        Self {
            providers,
            stopped,
            worker: Some(worker),
        }
    }

    fn requests(&self) -> Vec<Session> {
        self.providers.lock().unwrap().clone()
    }
}

impl Drop for ScriptedDaemon {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn settle(app: &mut Cagents, task: Task<Message>) {
    use cosmic::iced::futures::StreamExt;
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
                if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                    message @ Message::ChatUi(_),
                )) = action
                {
                    pending.push_back(app.update(message));
                }
            }
        }
    });
}

fn cached_chat() -> (Cagents, ChannelId, ChannelId, Pane, uuid::Uuid) {
    let (mut app, active, hidden, mut pane) = fixture();
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
    let daemon = ScriptedDaemon::start();
    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    app.chat_ui =
        chat_ui::State::with_drafts(HashMap::from([(pane.id, "Deferred shared draft".into())]));
    let startup = app.initialize_runtimes();
    settle(&mut app, startup);
    assert!(daemon.requests().is_empty());
    let CardKind::Chat(canonical) = &mut pane.kind else {
        unreachable!()
    };
    canonical.launch.recipe.arguments = vec!["--permission-mode=plan".into()];
    let expected = canonical.clone();
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
    let repeated = app.reconcile_shared_views();
    settle(&mut app, repeated);
    assert_eq!(daemon.requests().len(), 1);
    assert_eq!(app.durable.active_channel, Some(active));
    assert_eq!(
        app.persistent_workspace()
            .chat_drafts
            .get(&pane.id)
            .map(String::as_str),
        Some("Deferred shared draft")
    );
}

#[test]
fn local_only_cached_chat_still_restores_and_preserves_its_draft() {
    if isolated("local_only_cached_chat_still_restores_and_preserves_its_draft") {
        return;
    }
    let daemon = ScriptedDaemon::start();
    let (mut app, _, _, pane, _) = cached_chat();
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
    assert_eq!(
        app.persistent_workspace()
            .chat_drafts
            .get(&pane.id)
            .map(String::as_str),
        Some("Unsent original draft")
    );
}
