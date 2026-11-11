use super::*;
use crate::{
    agent_execution::{HostedSession, Surface},
    chat::{Launch, NativeContinuity, Session},
    model::HostIncarnation,
    terminal_ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
    upgrade::floor::Record,
    workspace_model::views::{Pane, View},
};
use std::{
    os::unix::{fs::PermissionsExt, net::UnixListener, net::UnixStream},
    sync::atomic::{AtomicBool, Ordering},
};

/// Each variant gets private paths and its own process, so action-owned ledger
/// reads and real client Tasks cannot see another test's daemon or floor files.
fn isolated_case() -> Option<String> {
    const CHILD: &str = "CAGENTS_NOTIFICATION_HISTORY_TEST";
    if let Ok(case) = std::env::var(CHILD) {
        return Some(case);
    }
    for case in [
        "exact",
        "native-thread",
        "launch",
        "missing-proof",
        "incomplete-proof",
        "source-proof",
        "missing-ledger",
        "ambiguous",
        "tombstone",
        "closed-project",
        "wrong-project",
    ] {
        let root = tempfile::Builder::new()
            .prefix("cnh-")
            .tempdir_in("/var/tmp")
            .unwrap();
        let name = std::thread::current().name().unwrap().to_owned();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", &name, "--nocapture"])
            .env(CHILD, case);
        // Inherit authority unchanged; only application paths and the fake
        // executable search path belong to this fixture.
        for (variable, leaf) in [
            ("HOME", "home"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_RUNTIME_DIR", "run"),
            ("TMPDIR", "tmp"),
        ] {
            let path = root.path().join(leaf);
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            child.env(variable, path);
        }
        child.env("PATH", "/usr/bin:/bin");
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "notification history {case}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }
    None
}

struct HistoryDaemon {
    attached: Arc<Mutex<Vec<Session>>>,
    forbidden: Arc<Mutex<Vec<String>>>,
    stopped: Arc<AtomicBool>,
    sockets: Arc<Mutex<Vec<UnixStream>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl HistoryDaemon {
    fn start(reader: Session) -> Self {
        let path = crate::terminal_ipc::daemon_socket_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let attached = Arc::new(Mutex::new(Vec::new()));
        let forbidden = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let sockets = Arc::new(Mutex::new(Vec::new()));
        let (receipts, rejected, stop, connections) = (
            attached.clone(),
            forbidden.clone(),
            stopped.clone(),
            sockets.clone(),
        );
        let worker = std::thread::spawn(move || {
            let mut clients = Vec::new();
            while !stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("history fixture accept: {error}"),
                };
                let mut held = connections.lock().unwrap();
                if stop.load(Ordering::Acquire) {
                    break;
                }
                held.push(stream.try_clone().unwrap());
                drop(held);
                let (reader, receipts, rejected) =
                    (reader.clone(), receipts.clone(), rejected.clone());
                clients.push(std::thread::spawn(move || {
                    let proof = reader.proof.clone().unwrap();
                    while let Ok(Some(request)) = read_frame::<ClientEnvelope>(&mut stream) {
                        let message = match request.request {
                            ClientRequest::Connect { .. } => ServerMessage::Response {
                                response: ServerResponse::Connected {
                                    proof: proof.clone(),
                                    resumed: Vec::new(),
                                },
                            },
                            ClientRequest::ChatAttach { session } => {
                                assert_eq!(session, reader);
                                receipts.lock().unwrap().push(session.clone());
                                ServerMessage::Response {
                                    response: ServerResponse::ChatAttached {
                                        snapshot: crate::chat::Snapshot {
                                            ready: true,
                                            exited: true,
                                            session_id: session.launch.session_id.clone(),
                                            native_continuity: NativeContinuity::ReconnectOnly,
                                            read_only_reason: Some("Saved update history".into()),
                                            ..Default::default()
                                        },
                                        session,
                                    },
                                }
                            }
                            ClientRequest::ChatRead { chat_id, .. } => ServerMessage::Response {
                                response: ServerResponse::ChatSnapshot {
                                    chat_id,
                                    snapshot: None,
                                },
                            },
                            ClientRequest::ChatVisibility { .. }
                            | ClientRequest::ChatAcknowledge { .. }
                            | ClientRequest::ChatDetach { .. } => ServerMessage::Response {
                                response: ServerResponse::Accepted,
                            },
                            other => {
                                rejected.lock().unwrap().push(format!("{other:?}"));
                                ServerMessage::Error(ProtocolError::new(
                                    ProtocolErrorCode::TerminalUnavailable,
                                    "Only exact history attachment is supported by this fixture",
                                    false,
                                ))
                            }
                        };
                        if write_frame(
                            &mut stream,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id: Some(request.request_id),
                                proof: proof.clone(),
                                message,
                            },
                        )
                        .is_err()
                        {
                            break;
                        }
                    }
                }));
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            attached,
            forbidden,
            stopped,
            sockets,
            worker: Some(worker),
        }
    }
}

impl Drop for HistoryDaemon {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        for socket in self.sockets.lock().unwrap().iter() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
        self.worker.take().unwrap().join().unwrap();
    }
}

/// Poll the real chat client, then deliver its Ready result. Other navigation
/// effects are outside this protocol regression and must not outlive the test.
fn attach_history(app: &mut Cagents, task: Task<Message>) {
    use cosmic::iced::futures::StreamExt;
    let _renderer = crate::app::tests::headless_renderer();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut stream = cosmic::iced::runtime::task::into_stream(task).unwrap();
        loop {
            let action = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("history attachment must settle")
                .expect("notification must schedule the history attachment");
            if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                message @ Message::ChatUi(super::super::super::chat_ui::Action::Ready(..)),
            )) = action
            {
                drop(app.update(message));
                break;
            }
        }
    });
}

#[test]
fn paused_history_notification_attaches_without_execution_and_preserves_live_guards() {
    let Some(case) = isolated_case() else {
        return;
    };
    let (mut app, mut agent) = scheduled_attention_fixture();
    let console = agent.console.take().unwrap();
    let mut recipe = console.launch;
    recipe.program = "never-execute-notification-history-provider".into();
    let source = Session {
        id: TerminalId::new(),
        proof: console.continuity.live_proof(),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: NativeContinuity::Durable {
            provider_version: "fixture".into(),
        },
        launch: Launch {
            provider: agent.spec.provider,
            session_id: Some("exact-stopped-native-thread".into()),
            recipe,
        },
    };
    agent.spec.surface = Surface::Chat;
    agent.session = Some(HostedSession::Chat(source.clone()));
    let mut reader = source.clone();
    reader.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("history-reader");
    reader.native_continuity = NativeContinuity::ReconnectOnly;
    if case != "missing-ledger" {
        Record::new(source.proof.clone().unwrap(), vec![source.clone()], vec![])
            .save()
            .unwrap();
    }
    match case.as_str() {
        "native-thread" => reader.launch.session_id = Some("wrong-thread".into()),
        "launch" => reader.launch.recipe.arguments.push("wrong-launch".into()),
        "missing-proof" => reader.proof = None,
        "incomplete-proof" => {
            reader.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("");
        }
        "source-proof" => {
            let Some(HostedSession::Chat(original)) = agent.session.as_mut() else {
                unreachable!()
            };
            original.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("unissued");
        }
        "closed-project" => {
            app.project_ui
                .closed
                .insert(agent.spec.target.repository.clone());
        }
        "wrong-project" => {
            app.workspace_model.channels[0].repository = "/different-repository".into();
        }
        _ => {}
    }
    let card = CardId::from_uuid(agent.id.as_uuid());
    let mut saved = super::super::super::tests::terminal_card_state();
    saved.id = card;
    saved.title = "Paused scheduled conversation".into();
    saved.kind = CardKind::Chat(reader.clone());
    let pane = Pane::from_card(&saved, Some(agent.spec.target.directory.clone())).unwrap();
    let mut view = View::empty(agent.placement.clone());
    view.panes.push(pane.clone());
    app.workspace_model.views = vec![view.clone()];
    if case == "ambiguous" {
        let mut duplicate = view;
        duplicate.scope = crate::agent_execution::Placement::Channel(ChannelId::new());
        app.workspace_model.views.push(duplicate);
    }
    if case == "tombstone" {
        saved.existence = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: Some("closed locally".into()),
            },
        };
        Arc::make_mut(&mut app.durable).cards.push(saved.clone());
    }
    app.workspace_model.agents.push(agent.clone());
    let original_agent = agent.clone();
    let before = app.durable.clone();
    let history = crate::upgrade::floor::load().unwrap();
    if case == "exact" {
        let display = app
            .resolved_activation_agent(source.id, Some(&history))
            .unwrap();
        assert!(
            app.adopt_execution_agent_activation_display(
                &display.agent,
                display.display,
                display.session,
            )
            .is_none(),
            "history membership must not become live execution ownership"
        );
    }
    let daemon = (case == "exact").then(|| HistoryDaemon::start(reader.clone()));
    let task = app.update_notifications(Action::Activated(Target::Session(source.id)));
    if let Some(daemon) = daemon {
        assert_eq!(app.workspace.focused(), Some(card));
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(reader.clone())
        );
        attach_history(&mut app, task);
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Chat(runtime))
            if runtime.session == reader && runtime.restored_pane_ready() == Ok(true))
        );
        drop(app.update_notifications(Action::Activated(Target::Session(source.id))));
        assert_eq!(app.durable.cards.len(), 1);

        // Reusing the same attached reader at a newly authoritative scope must
        // preserve its runtime and the original execution placement.
        let destination = ChannelId::new();
        let mut local = app.durable.channels[0].clone();
        local.id = destination;
        // This template is now populated, unlike the pre-adoption fixture.
        // A fresh destination must not duplicate the source's membership:
        // doing so correctly makes adoption consider it already in place.
        local.cards.clear();
        local.focused = None;
        Arc::make_mut(&mut app.durable).channels.push(local);
        let mut shared = app.workspace_model.channels[0].clone();
        shared.id = destination;
        app.workspace_model.channels.push(shared);
        app.workspace_model.views[0].scope =
            crate::agent_execution::Placement::Channel(destination);
        app.workspace_model.sequence += 1;
        assert_eq!(
            app.durable
                .channels
                .iter()
                .filter(|channel| channel.cards.contains(&card))
                .map(|channel| channel.id)
                .collect::<Vec<_>>(),
            [agent.channel().unwrap()],
            "only the source owns the card before the authoritative move"
        );
        drop(app.update_notifications(Action::Activated(Target::Session(source.id))));
        assert_eq!(app.durable.active_channel, Some(destination));
        assert_eq!(app.workspace.focused(), Some(card));
        assert_eq!(
            app.durable
                .channels
                .iter()
                .filter(|channel| channel.cards.contains(&card))
                .map(|channel| channel.id)
                .collect::<Vec<_>>(),
            [destination],
            "adoption must transfer membership rather than duplicate it"
        );
        assert_eq!(app.workspace_model.agents, [original_agent], "{case}");
        let attached = daemon.attached.clone();
        let forbidden = daemon.forbidden.clone();
        // Include requests observed while the real client and fixture finish,
        // rather than inspecting counters before background readers stop.
        drop(app);
        drop(daemon);
        assert_eq!(*attached.lock().unwrap(), [reader]);
        assert!(forbidden.lock().unwrap().is_empty());
    } else {
        drop(task);
        assert_eq!(*app.durable, *before, "{case}");
        assert!(!app.chat_start_pending(card), "{case}");
        assert!(!app.cards.contains_key(&card), "{case}");
        assert_eq!(app.workspace_model.agents, [original_agent], "{case}");
    }
}
