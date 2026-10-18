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

#[derive(Clone)]
struct UpgradeScript {
    source: Session,
    canonical: Session,
    refuse_lineage: bool,
}

struct ScriptedDaemon {
    providers: Arc<Mutex<Vec<Session>>>,
    resolutions: Arc<Mutex<Vec<Session>>>,
    connections: Arc<Mutex<Vec<std::os::unix::net::UnixStream>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ScriptedDaemon {
    fn start() -> Self {
        Self::with_upgrade(None)
    }

    fn with_upgrade(upgrade: Option<UpgradeScript>) -> Self {
        let socket = crate::terminal_ipc::daemon_socket_path().unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let providers = Arc::new(Mutex::new(Vec::new()));
        let resolutions = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let recorded = providers.clone();
        let resolved = resolutions.clone();
        let sockets = connections.clone();
        let stop = stopped.clone();
        let worker = std::thread::spawn(move || {
            let proof = HostSessionProof {
                host_session_id: "live-host".into(),
                boot_id: "boot".into(),
                host_incarnation: "live-incarnation".into(),
            };
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
                                    "strict upgrade cannot start a provider"
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
                                    ServerMessage::Response {
                                        response: ServerResponse::ChatAttached {
                                            snapshot: crate::chat::Snapshot {
                                                session_id: session.launch.session_id.clone(),
                                                ready: true,
                                                ..Default::default()
                                            },
                                            session,
                                        },
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
    let daemon = ScriptedDaemon::start();
    let (mut app, active, hidden, mut pane, identity) = cached_chat();
    Arc::make_mut(&mut app.durable).active_channel = Some(hidden);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(pane.id, "Old source only".into())]));
    let original = pane.kind.clone();
    let startup = app.initialize_runtimes();
    settle(&mut app, startup);
    let CardKind::Chat(replacement) = &mut pane.kind else {
        unreachable!()
    };
    replacement.id = TerminalId::new();
    replacement.launch.session_id = Some("canonical-replacement-thread".into());
    let expected = replacement.clone();
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
    let daemon = ScriptedDaemon::start();
    let (mut app, active, hidden, pane, identity) = cached_chat();
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
    let CardKind::Chat(expected) = pane.kind else {
        unreachable!()
    };
    assert_eq!(daemon.requests().len(), 1);
    assert_eq!(daemon.requests()[0].launch, expected.launch);
    assert_eq!(daemon.requests()[0].id, expected.id);
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
    exercise_canonical_upgrade_with_echoes(refuse_lineage, retire_before_ready, false);
}

fn exercise_canonical_upgrade_with_echoes(
    refuse_lineage: bool,
    retire_before_ready: bool,
    duplicate_echoes: bool,
) {
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
        model: authoritative.clone(),
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
    let mut strict_target = None;
    let mut echo_tasks = Vec::new();
    let mut echoes = 0;
    settle_observing(&mut app, admission, |app, message| {
        let boundary = match &message {
            Message::ChatUi(chat_ui::Action::Ready(target, _)) if !ready_seen => Some(*target),
            Message::ChatUi(chat_ui::Action::UpgradeCommitted(target, _, result)) => {
                assert!(
                    result.is_ok(),
                    "the real writer must acknowledge before UI receipt delivery"
                );
                Some(*target)
            }
            _ => None,
        };
        if duplicate_echoes && let Some(receipt_target) = boundary {
            if let Some(original) = strict_target {
                assert_eq!(
                    receipt_target, original,
                    "the writer receipt must retain the strict attachment Target"
                );
            } else {
                strict_target = Some(receipt_target);
            }
            let before_target = match app.cards.get(&pane.id).unwrap() {
                CardRuntime::Chat(runtime) => {
                    assert_eq!(&**runtime as *const chat_ui::Runtime as usize, allocation);
                    runtime.target
                }
                _ => panic!("pending strict upgrade must retain original Content"),
            };
            // The first echo precedes Ready application. The second precedes
            // UI consumption of the actual writer ACK, which has already
            // committed on its own thread. Neither echo may reinterpret S's
            // Content as canonical A or authorize a second attachment.
            echoes += 1;
            let mut duplicate = authoritative.clone();
            duplicate.sequence += echoes;
            let view = duplicate
                .views
                .iter_mut()
                .find(|view| view.scope == Scope::Channel(hidden))
                .unwrap();
            view.revision += echoes;
            echo_tasks.push(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: Scope::Channel(hidden),
                operation: None,
                model: duplicate,
            }));
            let CardRuntime::Chat(runtime) = app.cards.get(&pane.id).unwrap() else {
                panic!("canonical echo must retain the pending original Content");
            };
            assert_eq!(&**runtime as *const chat_ui::Runtime as usize, allocation);
            assert_eq!(
                runtime.target, before_target,
                "echo cannot replace the Content owner's target"
            );
            assert_eq!(runtime.session, source);
            let snapshot = app.persistent_workspace();
            assert_eq!(
                snapshot
                    .cards
                    .iter()
                    .find(|card| card.id == pane.id)
                    .unwrap()
                    .kind,
                CardKind::Chat(source.clone())
            );
            assert_eq!(
                snapshot.pending_chat_admissions.get(&pane.id),
                Some(&source)
            );
            assert_eq!(
                snapshot.chat_drafts.get(&pane.id).map(String::as_str),
                Some(text)
            );
            assert_eq!(
                snapshot
                    .cards
                    .iter()
                    .filter(|card| card.existence.is_open())
                    .count(),
                1,
                "same canonical echoes must not manufacture an inert copy"
            );
            assert!(app.chat_start_pending(pane.id));
            assert_eq!(
                app.startup_pane_check(),
                Ok(false),
                "UI receipt is still pending"
            );
            assert_eq!(daemon.resolutions(), vec![source.clone()]);
            assert_eq!(daemon.requests(), vec![canonical.clone()]);
            // Queue a real ordinary snapshot while the pending source is S.
            // The existing writer translation must preserve its ordered rebind.
            app.persist();
        }
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
    // Returned projection work joins the existing task harness only after its
    // executor has exited. No nested settle/Tokio runtime occurs in observers.
    for task in echo_tasks {
        settle(&mut app, task);
    }
    if duplicate_echoes {
        assert_eq!(
            echoes, 2,
            "exercise both pre-Ready and pre-UI-ACK boundaries"
        );
        let CardRuntime::Chat(runtime) = app.cards.get(&pane.id).unwrap() else {
            panic!("confirmed strict attachment remains usable");
        };
        assert_eq!(Some(runtime.target), strict_target);
    }
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
fn canonical_upgrade_duplicate_echoes_preserve_pending_source_through_ui_receipt() {
    if isolated("canonical_upgrade_duplicate_echoes_preserve_pending_source_through_ui_receipt") {
        return;
    }
    exercise_canonical_upgrade_with_echoes(false, false, true);
}
