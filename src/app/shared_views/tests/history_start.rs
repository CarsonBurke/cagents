//! Public History dispatch and native-client Tasks, with a deliberately delayed
//! UI receipt. The daemon has accepted the first start while the UI still owns
//! its Starting phase: an authoritative descriptor echo must not start it again.
use super::*;
use crate::{
    chat::{Launch, Session},
    persistence::SavedConversation,
    terminal_ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
};
use std::{
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

fn isolated(name: &str) -> bool {
    const CHILD: &str = "CAGENTS_HISTORY_START_TEST";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return false;
    }
    let root = tempfile::Builder::new()
        .prefix("chs-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            &format!("app::shared_views::tests::history_start::{name}"),
            "--nocapture",
        ])
        .env_clear()
        .env(CHILD, name)
        .env("PATH", "/usr/bin:/bin");
    for (variable, leaf) in [
        ("HOME", "home"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_RUNTIME_DIR", "run"),
        ("TMPDIR", "tmp"),
    ] {
        let directory = root.path().join(leaf);
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        command.env(variable, directory);
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "isolated History test failed: {}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
    true
}

#[derive(Clone)]
struct Preflight {
    seen: crossbeam_channel::Sender<()>,
    release: crossbeam_channel::Sender<bool>,
    released: crossbeam_channel::Receiver<bool>,
    reserved: crossbeam_channel::Sender<()>,
    reservation: crossbeam_channel::Receiver<()>,
    rejected: crossbeam_channel::Sender<()>,
    rejection: crossbeam_channel::Receiver<()>,
}

struct Daemon {
    starts: Arc<Mutex<Vec<Session>>>,
    attaches: Arc<Mutex<Vec<Session>>>,
    refusals: Arc<Mutex<Vec<String>>>,
    stopped: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<UnixStream>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Daemon {
    fn start(fail_first: bool) -> Self {
        Self::configured(fail_first, None, false)
    }
    fn lost_start_reply() -> Self {
        Self::configured(false, None, true)
    }
    fn preflight() -> (
        Self,
        crossbeam_channel::Receiver<()>,
        crossbeam_channel::Sender<bool>,
    ) {
        let (seen, observation) = crossbeam_channel::bounded(1);
        let (release, released) = crossbeam_channel::bounded(2);
        let (reserved, reservation) = crossbeam_channel::bounded(1);
        let (rejected, rejection) = crossbeam_channel::bounded(1);
        let script = Preflight {
            seen,
            release: release.clone(),
            released,
            reserved,
            reservation,
            rejected,
            rejection,
        };
        (
            Self::configured(false, Some(script), false),
            observation,
            release,
        )
    }
    fn configured(fail_first: bool, preflight: Option<Preflight>, lose_first_reply: bool) -> Self {
        let socket = crate::terminal_ipc::daemon_socket_path().unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let starts = Arc::new(Mutex::new(Vec::new()));
        let attaches = Arc::new(Mutex::new(Vec::new()));
        let refusals = Arc::new(Mutex::new(Vec::new()));
        let admitted = Arc::new(Mutex::new(Vec::<Session>::new()));
        let pending = Arc::new(Mutex::new(HashMap::<TerminalId, Session>::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let (recorded, stop, sockets) = (starts.clone(), stopped.clone(), connections.clone());
        let attached = attaches.clone();
        let rejected = refusals.clone();
        let worker = std::thread::spawn(move || {
            let mut workers = Vec::new();
            while !stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("History daemon accept: {error}"),
                };
                let mut held = sockets.lock().unwrap();
                if stop.load(Ordering::Acquire) {
                    break;
                }
                held.push(stream.try_clone().unwrap());
                drop(held);
                let (recorded, stop) = (recorded.clone(), stop.clone());
                let (admitted, attached) = (admitted.clone(), attached.clone());
                let preflight = preflight.clone();
                let pending = pending.clone();
                let rejected = rejected.clone();
                workers.push(std::thread::spawn(move || {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let proof = HostSessionProof {
                        host_session_id: "history-host".into(),
                        boot_id: "history-boot".into(),
                        host_incarnation: "history-incarnation".into(),
                    };
                    loop {
                        let request = match read_frame::<ClientEnvelope>(&mut stream) {
                            Ok(Some(request)) => request,
                            Ok(None) => break,
                            Err(_) if stop.load(Ordering::Acquire) => break,
                            Err(crate::terminal_ipc::FrameError::Io(error))
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) =>
                            {
                                continue;
                            }
                            Err(error) => panic!("History daemon read: {error}"),
                        };
                        let response = match request.request {
                            ClientRequest::Connect { .. } => ServerResponse::Connected {
                                proof: proof.clone(),
                                resumed: Vec::new(),
                            },
                            // The first provider is already admitted when its
                            // Ready is held at the UI boundary. Keep inventory
                            // faithful: a duplicate Restore must attach to it,
                            // never manufacture another empty-inventory start.
                            ClientRequest::ChatList => {
                                let mut sessions = admitted.lock().unwrap().clone();
                                sessions.extend(pending.lock().unwrap().values().cloned());
                                if let Some(script) = &preflight {
                                    // Snapshot inventory while first preflight
                                    // is blocked, before its reservation exists.
                                    assert!(sessions.is_empty());
                                    script.release.send(true).unwrap();
                                    script
                                        .reservation
                                        .recv_timeout(Duration::from_secs(5))
                                        .unwrap();
                                }
                                ServerResponse::ChatListed { sessions }
                            }
                            ClientRequest::ChatAttach { session } => {
                                assert!(admitted.lock().unwrap().contains(&session));
                                attached.lock().unwrap().push(session.clone());
                                ServerResponse::ChatAttached {
                                    snapshot: crate::chat::Snapshot {
                                        ready: true,
                                        session_id: session.launch.session_id.clone(),
                                        native_continuity: session.native_continuity.clone(),
                                        ..Default::default()
                                    },
                                    session,
                                }
                            }
                            ClientRequest::ChatPrepareResume { source } => {
                                assert!(admitted.lock().unwrap().contains(&source));
                                ServerResponse::ChatResumePrepared { session: source }
                            }
                            ClientRequest::ChatStart { mut session, .. } => {
                                let count = {
                                    let mut starts = recorded.lock().unwrap();
                                    starts.push(session.clone());
                                    starts.len()
                                };
                                let already_admitted = admitted.lock().unwrap().iter().any(|old| old.id == session.id);
                                let already_pending = pending.lock().unwrap().contains_key(&session.id);
                                if (fail_first && count == 1) || already_admitted || already_pending {
                                    let message = if already_admitted {
                                        "This exact conversation already has a daemon writer or unconfirmed backend work; attach to its existing chat"
                                    } else if already_pending {
                                        "Chat is already starting"
                                    } else {
                                        "History fixture first start deliberately refused"
                                    };
                                    let code = if already_pending || already_admitted {
                                        ProtocolErrorCode::InvalidRequest
                                    } else {
                                        ProtocolErrorCode::TerminalUnavailable
                                    };
                                    rejected.lock().unwrap().push(message.to_owned());
                                    write_frame(
                                        &mut stream,
                                        &ServerEnvelope {
                                            version: PROTOCOL_VERSION,
                                            request_id: Some(request.request_id),
                                            proof: proof.clone(),
                                            message: ServerMessage::Error(ProtocolError::new(
                                                code,
                                                message,
                                                false,
                                            )),
                                        },
                                    )
                                    .unwrap();
                                    // The duplicate's actual guard error is on
                                    // the wire before the first Ready releases.
                                    if let Some(script) = &preflight {
                                        script.rejected.send(()).unwrap();
                                    }
                                    continue;
                                }
                                let await_duplicate = if let Some(script) = &preflight {
                                    assert_eq!(count, 1);
                                    script.seen.send(()).unwrap();
                                    script
                                        .released
                                        .recv_timeout(Duration::from_secs(5))
                                        .unwrap()
                                } else {
                                    false
                                };
                                session.proof = Some(proof.clone());
                                if let Some(script) = &preflight {
                                    pending.lock().unwrap().insert(session.id, session.clone());
                                    script.reserved.send(()).unwrap();
                                    if await_duplicate {
                                        script
                                            .rejection
                                            .recv_timeout(Duration::from_secs(5))
                                            .unwrap();
                                    }
                                }
                                pending.lock().unwrap().remove(&session.id);
                                // The real HOST publishes installed Claude
                                // continuity at successful admission, while a
                                // pending request remains ReconnectOnly.
                                session.native_continuity = crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
                                admitted.lock().unwrap().push(session.clone());
                                if lose_first_reply && count == 1 {
                                    // Admission succeeded; only its reply is lost.
                                    // Retrying must discover this owner, not start twice.
                                    stream.shutdown(std::net::Shutdown::Both).unwrap();
                                    break;
                                }
                                ServerResponse::ChatAttached {
                                    snapshot: crate::chat::Snapshot {
                                        ready: true,
                                        session_id: session.launch.session_id.clone(),
                                        native_continuity: session.native_continuity.clone(),
                                        ..Default::default()
                                    },
                                    session,
                                }
                            }
                            ClientRequest::ChatVisibility { .. }
                            | ClientRequest::ChatAcknowledge { .. } => ServerResponse::Accepted,
                            other => panic!("unexpected History request: {other:?}"),
                        };
                        write_frame(
                            &mut stream,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id: Some(request.request_id),
                                proof: proof.clone(),
                                message: ServerMessage::Response { response },
                            },
                        )
                        .unwrap();
                    }
                }));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            starts,
            attaches,
            refusals,
            stopped,
            connections,
            worker: Some(worker),
        }
    }
    fn starts(&self) -> Vec<Session> {
        self.starts.lock().unwrap().clone()
    }
    fn refusals(&self) -> Vec<String> {
        self.refusals.lock().unwrap().clone()
    }
    fn activations(&self) -> usize {
        self.starts.lock().unwrap().len() + self.attaches.lock().unwrap().len()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        for stream in self.connections.lock().unwrap().iter() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        self.worker.take().unwrap().join().unwrap();
    }
}

fn begin_history() -> (tempfile::TempDir, Cagents, Scope, Task<Message>) {
    let root = tempfile::tempdir().unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.settings.hibernation_delay_ms = 0;
    app.lifecycle = LifecycleManager::new(LifecycleBudget {
        max_hot_cards: app.settings.max_hot_cards,
        offscreen_grace: Duration::ZERO,
    });
    app.sidebar_model.projects.push(
        crate::sidebar::SidebarProject::new(
            ProjectId::new(),
            crate::repository::RepositoryId::new(),
            "History project".into(),
            root.path().to_path_buf(),
            Vec::new(),
            Vec::new(),
        )
        .with_kind(crate::model::ProjectKind::Folder),
    );
    let scope = Scope::Worktree {
        repository: root.path().to_path_buf(),
        path: root.path().to_path_buf(),
    };
    app.workspace_model.identity = uuid::Uuid::new_v4();
    app.workspace_model.views = vec![View::empty(scope.clone())];
    app.shared_views.initialized.insert(scope.clone());
    let entry = SavedConversation {
        card: CardId::new(),
        title: "Saved History thread".into(),
        open: false,
        quarantined: false,
        updated_at_unix_ms: 1,
        session: Session {
            id: TerminalId::new(),
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
            launch: Launch {
                provider: crate::agent::Provider::Claude,
                session_id: Some("00000000-0000-4000-8000-000000000001".into()),
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: root.path().to_path_buf(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
        },
    };
    // Use the actual History reducer and indexed Saved row, not insert/open
    // helpers. The indexer's I/O task is intentionally not run in this fixture.
    drop(app.update(Message::ResumeConversation(
        crate::app::resume_ui::Action::Open,
    )));
    drop(app.update(Message::ResumeConversation(
        crate::app::resume_ui::Action::Indexed(
            1,
            crate::app::resume_ui::Progress::Saved {
                entries: vec![entry.clone()],
                seeds: Arc::from([]),
                error: None,
            },
        ),
    )));
    let task = app.update(Message::ResumeConversation(
        crate::app::resume_ui::Action::Resume(entry.card),
    ));
    (root, app, scope, task)
}

fn hold_ready(app: &mut Cagents, task: Task<Message>) -> (chat_ui::Target, Message) {
    let mut held = None;
    super::startup_admission::settle_observing(app, task, |_, message| {
        if let Message::ChatUi(chat_ui::Action::Ready(target, _)) = &message {
            assert!(
                held.is_none(),
                "one History dispatch must produce one Ready"
            );
            held = Some((*target, message));
            None
        } else {
            Some(message)
        }
    });
    held.expect("History must actually execute its public native-client task")
}

fn echo(app: &mut Cagents, scope: &Scope, change: impl FnOnce(&mut Pane)) -> Task<Message> {
    let card = app
        .durable
        .cards
        .iter()
        .find(|card| matches!(card.kind, CardKind::Chat(_)) && card.existence.is_open())
        .unwrap();
    let mut pane = Pane::from_card(card, None).unwrap();
    change(&mut pane);
    let mut model = app.workspace_model.clone();
    model.sequence += 1;
    let view = model
        .views
        .iter_mut()
        .find(|view| &view.scope == scope)
        .unwrap();
    view.revision += 1;
    view.panes = vec![pane];
    app.apply_workspace_model_update(crate::app::workspace_model_sync::ModelUpdate::ViewApplied {
        scope: scope.clone(),
        operation: None,
        model,
    })
}

#[test]
fn history_echo_keeps_the_original_start_and_usable_ready() {
    if isolated("history_echo_keeps_the_original_start_and_usable_ready") {
        return;
    }
    let daemon = Daemon::start(false);
    let (_root, mut app, scope, task) = begin_history();
    let (original, ready) = hold_ready(&mut app, task);
    assert!(app.chat_start_pending(original.card));
    assert_eq!(daemon.starts().len(), 1);
    let echoed = echo(&mut app, &scope, |pane| {
        pane.title = "Echoed title".into();
        pane.width_fraction = 0.65;
    });
    super::startup_admission::settle_observing(&mut app, echoed, |_, message| Some(message));
    let ready = app.update(ready);
    super::startup_admission::settle_observing(&mut app, ready, |_, message| Some(message));
    assert_eq!(
        daemon.activations(),
        1,
        "canonical echo must not dispatch a second activation"
    );
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&original.card) else {
        panic!("original Ready must install the usable chat");
    };
    assert_eq!(
        runtime.target, original,
        "echo must not replace the pending Target"
    );
    assert!(
        runtime
            .session
            .proof
            .as_ref()
            .is_some_and(HostSessionProof::is_complete)
    );
    let edit = app.update(Message::ChatUi(chat_ui::Action::Composer(
        original,
        cosmic::iced::widget::text_editor::Action::Edit(
            cosmic::iced::widget::text_editor::Edit::Paste(Arc::new("Usable after History".into())),
        ),
    )));
    super::startup_admission::settle_observing(&mut app, edit, |_, message| Some(message));
    assert_eq!(
        app.persistent_workspace()
            .chat_drafts
            .get(&original.card)
            .map(String::as_str),
        Some("Usable after History")
    );
}

#[test]
fn changed_history_descriptor_supersedes_the_pending_start() {
    if isolated("changed_history_descriptor_supersedes_the_pending_start") {
        return;
    }
    let daemon = Daemon::start(false);
    let (_root, mut app, scope, task) = begin_history();
    let (original, stale) = hold_ready(&mut app, task);
    let changed = echo(&mut app, &scope, |pane| {
        let CardKind::Chat(session) = &mut pane.kind else {
            unreachable!()
        };
        // Same actor id: an id-only reservation incorrectly suppresses this.
        session.launch.session_id = Some("00000000-0000-4000-8000-000000000002".into());
        session.launch.recipe.arguments.push("--changed".into());
    });
    let (replacement, _) = hold_ready(&mut app, changed);
    assert_ne!(replacement, original);
    assert_eq!(
        daemon.starts().len(),
        1,
        "a changed descriptor must be checked against the admitted actor, not restarted"
    );
    let stale = app.update(stale);
    super::startup_admission::settle_observing(&mut app, stale, |_, message| Some(message));
    assert!(
        app.chat_start_pending(original.card),
        "stale Ready must not finish the replacement"
    );
    assert!(
        !matches!(app.cards.get(&original.card), Some(CardRuntime::Chat(runtime)) if runtime.target == original)
    );
}

#[test]
fn failed_history_start_can_retry() {
    if isolated("failed_history_start_can_retry") {
        return;
    }
    let daemon = Daemon::start(true);
    let (_root, mut app, _scope, task) = begin_history();
    super::startup_admission::settle_observing(&mut app, task, |_, message| Some(message));
    let card = app
        .durable
        .cards
        .iter()
        .find(|card| matches!(card.kind, CardKind::Chat(_)))
        .unwrap()
        .id;
    assert!(!app.chat_start_pending(card));
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
    let CardKind::Chat(source) = app.card_state(card).unwrap().kind.clone() else {
        unreachable!()
    };
    assert_eq!(
        app.chat_retry_intent(card, &source),
        crate::chat::Intent::RetryStart
    );
    let mut changed = source.clone();
    changed
        .launch
        .recipe
        .arguments
        .push("--different-conversation-config".into());
    assert_eq!(
        app.chat_retry_intent(card, &changed),
        crate::chat::Intent::Restore
    );
    // Exercise the same Retry action the Failed card exposes. No direct
    // removal of the failed runtime may manufacture a fresh reservation.
    let retry = app.update(Message::RetryCard(card));
    super::startup_admission::settle_observing(&mut app, retry, |_, message| Some(message));
    assert_eq!(daemon.starts().len(), 2);
    assert!(!app.chat_start_pending(card));
    assert!(
        matches!(app.cards.get(&card), Some(CardRuntime::Chat(runtime)) if runtime.session.proof.as_ref().is_some_and(HostSessionProof::is_complete))
    );
}

#[test]
fn history_retry_after_lost_start_reply_attaches_without_another_start() {
    if isolated("history_retry_after_lost_start_reply_attaches_without_another_start") {
        return;
    }
    let daemon = Daemon::lost_start_reply();
    let (_root, mut app, _scope, task) = begin_history();
    super::startup_admission::settle_observing(&mut app, task, |_, message| Some(message));
    let card = app
        .durable
        .cards
        .iter()
        .find(|card| matches!(card.kind, CardKind::Chat(_)))
        .unwrap()
        .id;
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
    assert_eq!(daemon.starts().len(), 1);
    let retry = app.update(Message::RetryCard(card));
    super::startup_admission::settle_observing(&mut app, retry, |_, message| Some(message));
    assert_eq!(
        daemon.starts().len(),
        1,
        "lost acknowledgement must not duplicate the provider start"
    );
    assert_eq!(daemon.attaches.lock().unwrap().len(), 1);
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
        panic!("retry must attach the admitted exact owner")
    };
    assert_eq!(runtime.session, daemon.attaches.lock().unwrap()[0]);
    assert_eq!(
        app.chat_retry_intent(card, &runtime.session),
        crate::chat::Intent::Restore
    );
}

#[test]
fn history_echo_during_preflight_preserves_the_only_start_receipt() {
    if isolated("history_echo_during_preflight_preserves_the_only_start_receipt") {
        return;
    }
    let (daemon, first_seen, release_first) = Daemon::preflight();
    let (_root, mut app, scope, task) = begin_history();
    // Poll the genuine History Task on its own executor, collecting rather
    // than applying its UI messages. Widget focus effects are irrelevant to
    // this provider-preflight oracle; the other test operates those widgets.
    let worker = std::thread::spawn(move || {
        use cosmic::iced::futures::StreamExt;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let mut messages = Vec::new();
                if let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) {
                    while let Some(action) =
                        tokio::time::timeout(Duration::from_secs(5), stream.next())
                            .await
                            .expect("bounded History preflight Task")
                    {
                        if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(message)) =
                            action
                        {
                            messages.push(message);
                        }
                    }
                }
                messages
            })
    });
    first_seen
        .recv_timeout(Duration::from_secs(5))
        .expect("first actual ChatStart reaches preflight");
    assert_eq!(daemon.starts().len(), 1);
    let echoed = echo(&mut app, &scope, |_| {});
    // In the unfixed path, the second real client snapshots empty inventory,
    // then lets the first preflight reserve its actor before the duplicate
    // ChatStart hits that reservation. The server rejects that second start
    // before releasing the first Ready. All ordering uses explicit barriers.
    super::startup_admission::settle_observing(&mut app, echoed, |_, message| Some(message));
    let duplicate_notice_before_first_ready = app.cards.values().any(|runtime|
        matches!(runtime, CardRuntime::Failed(error) if error.contains("Chat is already starting")));
    // Fixed projection schedules no second activation. Release the first
    // preflight explicitly now; it must never depend on a duplicate request.
    let _ = release_first.try_send(false);
    let messages = worker.join().unwrap();
    let mut original = None;
    for message in messages {
        if let Message::ChatUi(chat_ui::Action::Ready(target, _)) = &message {
            original = Some(*target);
        }
        let task = app.update(message);
        super::startup_admission::settle_observing(&mut app, task, |_, message| Some(message));
    }
    let original = original.expect("first start returns its genuine Ready");
    let ready_installed = matches!(app.cards.get(&original.card), Some(CardRuntime::Chat(runtime))
        if runtime.target == original && runtime.session.proof.as_ref().is_some_and(HostSessionProof::is_complete));
    if daemon.starts().len() > 1 {
        assert_eq!(
            daemon.refusals(),
            vec!["Chat is already starting".to_owned()],
            "control must observe the real duplicate-start guard error"
        );
        assert!(
            duplicate_notice_before_first_ready,
            "the duplicate error must reach UI before applying first Ready"
        );
        assert!(
            !ready_installed,
            "control must lose the first successful UI receipt"
        );
        eprintln!(
            "preflight control: Chat is already starting; first successful Ready was fenced out"
        );
    }
    assert_eq!(
        daemon.starts().len(),
        1,
        "History echo must not create a duplicate ChatStart during preflight"
    );
    assert!(!app.chat_start_pending(original.card));
    assert!(
        matches!(app.cards.get(&original.card), Some(CardRuntime::Chat(runtime)) if runtime.target == original && runtime.session.proof.as_ref().is_some_and(HostSessionProof::is_complete)),
        "the original successful Ready must remain usable"
    );
}
