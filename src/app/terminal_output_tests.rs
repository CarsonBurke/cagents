//! Real hosted-client delivery into Application::update; no daemon or PTY.
use super::*;
use crate::terminal::{
    RenderSnapshot,
    ipc::{
        ClientEnvelope, ClientRequest, FrameCodec, FrameError, MAX_FRAME_BYTES, PROTOCOL_VERSION,
        ProtocolError, ProtocolErrorCode, ServerEnvelope, ServerMessage, ServerResponse,
        TerminalDescriptor, TerminalSnapshot, WireTerminalEvent, write_frame,
    },
};
use std::{
    cell::Cell,
    os::unix::net::UnixListener,
    sync::atomic::{AtomicBool, Ordering},
    thread,
};

thread_local! {
    static PROJECTIONS: Cell<usize> = const { Cell::new(0) };
    static PROTECTIONS: Cell<usize> = const { Cell::new(0) };
}
pub(super) fn record_projection() {
    PROJECTIONS.set(PROJECTIONS.get() + 1);
}
pub(super) fn record_protection() {
    PROTECTIONS.set(PROTECTIONS.get() + 1);
}
fn counts() -> (usize, usize) {
    (PROJECTIONS.get(), PROTECTIONS.get())
}
fn reset() {
    PROJECTIONS.set(0);
    PROTECTIONS.set(0);
}

#[derive(Debug)]
enum ObservedInput {
    Connect,
    Paste(String),
    Input(Vec<u8>),
    Key(crate::terminal::KeyInput),
}

struct Peer {
    _directory: tempfile::TempDir,
    handle: HostedTerminalHandle,
    send: crossbeam_channel::Sender<ServerMessage>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    sequence: u64,
    reject_next_paste: Arc<AtomicBool>,
    reject_next_key: Arc<std::sync::Mutex<Option<ProtocolError>>>,
    observed: crossbeam_channel::Receiver<ObservedInput>,
}
impl Peer {
    fn new(session: TerminalSession, proof: HostSessionProof) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("output.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let id = session.id;
        let mut descriptor = TerminalDescriptor::relaunchable(id, session.launch.clone(), None);
        descriptor.session = session;
        descriptor.attached = true;
        let (send, messages) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let expected_proof = proof.clone();
        let reject_next_paste = Arc::new(AtomicBool::new(false));
        let reject_paste = Arc::clone(&reject_next_paste);
        let reject_next_key = Arc::new(std::sync::Mutex::new(None));
        let reject_key = Arc::clone(&reject_next_key);
        let (observe, observed) = crossbeam_channel::unbounded();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                if stopping.load(Ordering::Relaxed) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline);
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("private accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_millis(10)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
            let mut connected = false;
            while !stopping.load(Ordering::Relaxed) {
                for message in messages.try_iter() {
                    if write_frame(
                        &mut stream,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: None,
                            proof: proof.clone(),
                            message,
                        },
                    )
                    .is_err()
                    {
                        return;
                    }
                }
                let request: ClientEnvelope = match codec.read(&mut stream) {
                    Ok(Some(request)) => request,
                    Ok(None) => return,
                    Err(FrameError::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => panic!("private decode: {error}"),
                };
                // Observe only fixture input/connection kinds, never copy an
                // Authenticate request or its inherited opaque authority.
                let observed_request = match &request.request {
                    ClientRequest::Connect { .. } => Some(ObservedInput::Connect),
                    ClientRequest::Paste { text, .. } => Some(ObservedInput::Paste(text.clone())),
                    ClientRequest::Input { bytes, .. } => Some(ObservedInput::Input(bytes.clone())),
                    ClientRequest::Key { input, .. } => Some(ObservedInput::Key(input.clone())),
                    _ => None,
                };
                let message = if !connected {
                    assert!(matches!(request.request, ClientRequest::Connect { .. }));
                    connected = true;
                    ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: vec![descriptor.clone()],
                        },
                    }
                } else if matches!(request.request, ClientRequest::Paste { .. })
                    && reject_paste.swap(false, Ordering::Relaxed)
                {
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::QueueFull,
                        "the terminal command queue is full",
                        true,
                    ))
                } else if matches!(request.request, ClientRequest::Key { .. })
                    && let Some(error) = reject_key.lock().unwrap().take()
                {
                    ServerMessage::Error(error)
                } else if matches!(request.request, ClientRequest::CopyTerminalSelection { .. }) {
                    // Control errors carry the request ID. The hosted client
                    // restores the exact pending operation before UI delivery.
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::InvalidRequest,
                        "fixture copy refused",
                        false,
                    ))
                } else {
                    ServerMessage::Response {
                        response: ServerResponse::Accepted,
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
                    return;
                }
                if let Some(request) = observed_request {
                    observe.send(request).unwrap();
                }
            }
        });
        let handle =
            HostedTerminalHandle::connect_at(socket, id, Some(expected_proof), |_| {}).unwrap();
        let peer = Self {
            _directory: directory,
            handle,
            send,
            stop,
            worker: Some(worker),
            sequence: 0,
            reject_next_paste,
            reject_next_key,
            observed,
        };
        peer.handle
            .wait_until_attached(Duration::from_secs(3))
            .unwrap();
        peer
    }
    fn publish(&mut self, event: Option<WireTerminalEvent>) -> u64 {
        self.sequence += 1;
        let id = self.handle.terminal_id();
        if let Some(event) = event {
            self.send
                .send(ServerMessage::Event {
                    terminal_id: id,
                    sequence: self.sequence,
                    event,
                })
                .unwrap();
        }
        let mut snapshot = RenderSnapshot::empty();
        snapshot.generation = self.sequence;
        snapshot.activity = Some(crate::model::TerminalOutputActivity {
            sequence: self.sequence,
            last_output_at_unix_ms: 0,
        });
        self.send
            .send(ServerMessage::Snapshot {
                terminal_id: id,
                snapshot: TerminalSnapshot::full(&snapshot),
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.handle.generation() < self.sequence {
            assert!(Instant::now() < deadline, "actual hosted delivery");
            thread::sleep(Duration::from_millis(1));
        }
        self.sequence
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn fixture() -> (Cagents, CardId, Peer) {
    let mut app = super::tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let CardKind::Terminal(mut session) = app.card_state(card).unwrap().kind.clone() else {
        panic!()
    };
    let proof = HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("output-test"),
        boot_id: crate::model::BootId::new("output-test"),
        host_incarnation: crate::model::HostIncarnation::new("output-test"),
    };
    session.continuity = TerminalContinuity::Live {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    app.card_state_mut(card).unwrap().kind = CardKind::Terminal(session.clone());
    let peer = Peer::new(session, proof);
    app.cards
        .insert(card, CardRuntime::Terminal(peer.handle.clone().into()));
    drop(app.update(Message::TerminalWake {
        card,
        generation: 0,
    }));
    reset();
    (app, card, peer)
}
fn apply(app: &mut Cagents, card: CardId, peer: &mut Peer, event: Option<WireTerminalEvent>) {
    let generation = peer.publish(event);
    drop(app.update(Message::TerminalWake { card, generation }));
}

#[test]
fn grid_output_keeps_workspace_and_protection_preparation_idle() {
    let (mut app, card, mut peer) = fixture();
    let template = app.card_state(card).unwrap().clone();
    for index in 0..200 {
        let mut cold = template.clone();
        cold.id = CardId::new();
        cold.exposure = Exposure::Offscreen;
        cold.residency = Residency::Cold;
        Arc::make_mut(&mut app.durable).cards.push(cold);
        app.known_worktree_roots
            .insert(PathBuf::from(format!("/output-fixture/root-{index}")));
    }
    let durable = Arc::clone(&app.durable);
    app.status = Some(TransientNotice::warning("output test notice"));
    for _ in 0..32 {
        apply(&mut app, card, &mut peer, None);
    }
    assert_eq!(counts(), (0, 0));
    assert!(Arc::ptr_eq(&durable, &app.durable));
    assert_eq!(peer.handle.snapshot().generation, 32);
    assert!(
        app.notices
            .entries
            .iter()
            .any(|notice| notice.notice.message == "output test notice")
    );
}

#[test]
fn semantic_terminal_changes_invalidate_but_identical_reports_do_not() {
    let (mut app, card, mut peer) = fixture();
    let events = [
        WireTerminalEvent::TitleChanged {
            title: "output agent".into(),
        },
        WireTerminalEvent::WorkingDirectoryChanged {
            path: "/output-fixture/new".into(),
        },
        WireTerminalEvent::ResumeHintChanged {
            hint: Some("resume-output".into()),
        },
        WireTerminalEvent::AgentActivityChanged {
            activity: Some(crate::agent::Activity {
                provider: crate::agent::Provider::Claude,
                state: crate::agent::ActivityState::Working,
            }),
        },
    ];
    for event in events {
        reset();
        apply(&mut app, card, &mut peer, Some(event.clone()));
        assert_eq!(counts(), (1, 1), "{event:?}");
        reset();
        let durable = Arc::clone(&app.durable);
        apply(&mut app, card, &mut peer, Some(event));
        assert_eq!(counts(), (0, 0));
        assert!(Arc::ptr_eq(&durable, &app.durable));
    }
    reset();
    apply(&mut app, card, &mut peer, Some(WireTerminalEvent::Bell));
    assert_eq!(counts(), (1, 1));
    reset();
    apply(
        &mut app,
        card,
        &mut peer,
        Some(WireTerminalEvent::Completed {
            exit_code: 0,
            signal: None,
            intentional: false,
        }),
    );
    assert_eq!(counts(), (1, 1));
    assert!(ended_terminal(app.card_state(card)).is_some());
}

#[test]
fn clipboard_reply_is_delivered_even_when_no_metadata_changed() {
    use cosmic::iced::futures::StreamExt;
    let (mut app, card, mut peer) = fixture();
    let request = uuid::Uuid::new_v4();
    app.terminal_find.entry(card).or_default().pending_copy = Some(request);
    let generation = peer.publish(Some(WireTerminalEvent::ClipboardText {
        request,
        text: "copied output".into(),
    }));
    let task = app.update(Message::TerminalWake { card, generation });
    assert_eq!(counts(), (0, 0));
    assert_eq!(app.terminal_find[&card].pending_copy, None);
    let mut stream = iced::runtime::task::into_stream(task).expect("copy task retained");
    let action = cosmic::iced::futures::executor::block_on(stream.next()).unwrap();
    assert!(matches!(action, iced::runtime::Action::Clipboard(
        iced::runtime::clipboard::Action::Write { contents, .. }
    ) if contents == "copied output"));
}

#[tokio::test]
async fn grid_output_still_schedules_live_find_refresh() {
    use cosmic::iced::futures::StreamExt;
    let (mut app, card, mut peer) = fixture();
    app.terminal_find.entry(card).or_default().open = true;
    drop(app.handle_terminal_find(terminal_ui::Action::Query(card, "needle".into())));
    reset();
    let generation = peer.publish(None);
    let task = app.update(Message::TerminalWake { card, generation });
    assert_eq!(counts(), (0, 0));
    let mut stream = iced::runtime::task::into_stream(task).expect("Find timer retained");
    let refreshed = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(action) = stream.next().await {
            if matches!(action, iced::runtime::Action::Output(cosmic::Action::App(
                Message::TerminalFind(terminal_ui::Action::Refresh(target, _))
            )) if target == card)
            {
                return true;
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(refreshed);
}

#[test]
fn root_exit_keeps_the_live_descendant_surface_and_controls() {
    let (mut app, card, mut peer) = fixture();
    apply(
        &mut app,
        card,
        &mut peer,
        Some(WireTerminalEvent::Exited {
            exit_code: 0,
            signal: None,
            intentional: false,
        }),
    );
    assert_eq!(counts(), (0, 0));
    assert!(ended_terminal(app.card_state(card)).is_none());
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Terminal(_))
    ));
    apply(
        &mut app,
        card,
        &mut peer,
        Some(WireTerminalEvent::TitleChanged {
            title: "descendant still alive".into(),
        }),
    );
    assert_eq!(counts(), (1, 1));
}

#[test]
fn terminal_control_failure_reports_a_notice_and_clears_pending_copy() {
    let (mut app, card, peer) = fixture();
    let request = uuid::Uuid::new_v4();
    app.terminal_find.entry(card).or_default().pending_copy = Some(request);
    peer.handle
        .copy_selection(
            request,
            crate::terminal::SearchMatch {
                start_row: 0,
                start_column: 0,
                end_row: 0,
                end_column: 1,
            },
        )
        .unwrap();
    let events = peer.handle.events();
    let deadline = Instant::now() + Duration::from_secs(3);
    while events.is_empty() {
        assert!(
            Instant::now() < deadline,
            "actual failed Copy request reply"
        );
        thread::sleep(Duration::from_millis(1));
    }
    drop(app.update(Message::TerminalWake {
        card,
        generation: peer.handle.generation(),
    }));
    assert_eq!(counts(), (1, 1));
    assert_eq!(app.terminal_find[&card].pending_copy, None);
    assert!(
        app.notices
            .entries
            .iter()
            .any(|notice| notice.notice.message == "fixture copy refused")
    );
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Terminal(_))
    ));
}

#[test]
fn confirmed_host_proof_refreshes_uncertain_terminal_metadata() {
    let (mut app, card, mut peer) = fixture();
    app.uncertain_terminal_proofs
        .insert(card, peer.handle.proof().unwrap());
    apply(&mut app, card, &mut peer, None);
    assert_eq!(counts(), (1, 1));
    assert!(!app.uncertain_terminal_proofs.contains_key(&card));
    reset();
    apply(&mut app, card, &mut peer, None);
    assert_eq!(counts(), (0, 0));
}

#[test]
fn rejected_paste_keeps_the_actual_app_surface_proof_output_and_next_control() {
    let (mut app, card, mut peer) = fixture();
    let proof = peer.handle.proof().unwrap();
    let continuity = terminal_continuity(app.card_state(card)).unwrap().clone();
    let recovery_attempts = app.terminal_recovery_attempts.clone();
    let uncertain = app.uncertain_terminal_proofs.clone();
    let mut requests = peer.observed.try_iter().collect::<Vec<_>>();
    peer.reject_next_paste.store(true, Ordering::Relaxed);
    app.settings.confirm_multiline_paste = false;
    drop(app.update(Message::TerminalFind(terminal_ui::Action::Paste(
        card,
        "refused original input".into(),
    ))));
    // The normal user Paste update performs its existing projection. Measure
    // only the following typed rejection wake, not that setup work.
    let durable = Arc::clone(&app.durable);
    reset();
    let events = peer.handle.events();
    let deadline = Instant::now() + Duration::from_secs(3);
    while events.is_empty() {
        assert!(Instant::now() < deadline, "actual correlated paste refusal");
        thread::sleep(Duration::from_millis(1));
    }
    drop(app.update(Message::TerminalWake {
        card,
        generation: peer.handle.generation(),
    }));
    assert_eq!(counts(), (0, 0), "rejection changes only a notice");
    assert!(Arc::ptr_eq(&durable, &app.durable));
    assert_eq!(terminal_continuity(app.card_state(card)), Some(&continuity));
    assert_eq!(app.terminal_recovery_attempts, recovery_attempts);
    assert_eq!(app.uncertain_terminal_proofs, uncertain);
    assert!(
        matches!(app.cards.get(&card), Some(CardRuntime::Terminal(surface))
        if surface.terminal_id() == peer.handle.terminal_id() && surface.host_proof() == Some(proof.clone()))
    );
    assert!(ended_terminal(app.card_state(card)).is_none());
    assert!(app.notices.entries.iter().any(|entry| {
        entry.notice.severity == NoticeSeverity::Warning
            && entry.notice.message.contains("input queue is full")
            && entry.notice.message.contains("try again")
    }));
    // New output is delivered by this same open socket after the refused
    // command, and a distinct user command still reaches that exact peer.
    let generation = peer.publish(None);
    drop(app.update(Message::TerminalWake { card, generation }));
    assert_eq!(peer.handle.snapshot().generation, generation);
    peer.handle.input(b"after refusal".to_vec()).unwrap();
    loop {
        let request = peer.observed.recv_timeout(Duration::from_secs(3)).unwrap();
        let done = matches!(&request, ObservedInput::Input(bytes) if bytes == b"after refusal");
        requests.push(request);
        if done {
            break;
        }
    }
    requests.extend(peer.observed.try_iter());
    assert_eq!(
        requests
            .iter()
            .filter(|r| matches!(r, ObservedInput::Connect))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| matches!(r, ObservedInput::Paste(text)
        if text == "refused original input"))
            .count(),
        1,
        "never automatically replay refused input"
    );
    assert!(
        !peer.worker.as_ref().unwrap().is_finished(),
        "the same connection survives"
    );
    assert_eq!(peer.handle.proof(), Some(proof));
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Terminal(_))
    ));
}

#[test]
fn delayed_input_rejection_cannot_replace_a_newer_fatal_or_completed_terminal_outcome() {
    for completed in [false, true] {
        let (mut app, card, mut peer) = fixture();
        peer.reject_next_paste.store(true, Ordering::Relaxed);
        // A real server event precedes the correlated rejection on this same
        // framed connection. This also models a coalesced rejection released
        // after an already queued, newer terminal outcome.
        let outcome = if completed {
            WireTerminalEvent::Completed {
                exit_code: 7,
                signal: None,
                intentional: false,
            }
        } else {
            WireTerminalEvent::Error {
                operation: "read PTY".into(),
                message: "fixture fatal outcome".into(),
            }
        };
        let generation = peer.publish(Some(outcome));
        peer.handle
            .paste("stale input", crate::terminal::PasteOrigin::Clipboard)
            .unwrap();
        loop {
            let request = peer.observed.recv_timeout(Duration::from_secs(3)).unwrap();
            if matches!(request, ObservedInput::Paste(ref text) if text == "stale input") {
                break;
            }
        }
        let events = peer.handle.events();
        let deadline = Instant::now() + Duration::from_secs(3);
        while events.len() < 2 {
            assert!(
                Instant::now() < deadline,
                "both actual terminal events must arrive"
            );
            thread::yield_now();
        }
        app.status = Some(TransientNotice::warning("preserve existing notice"));
        drop(app.update(Message::TerminalWake { card, generation }));
        if completed {
            assert!(ended_terminal(app.card_state(card)).is_some());
            assert!(
                app.notices
                    .entries
                    .iter()
                    .any(|entry| entry.notice.severity == NoticeSeverity::Error
                        && entry.notice.message == "terminal exited with status 7")
            );
        } else {
            assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
            assert!(
                app.notices
                    .entries
                    .iter()
                    .any(|entry| entry.notice.severity == NoticeSeverity::Error
                        && entry.notice.message.contains("fixture fatal outcome"))
            );
        }
        assert!(
            !app.notices
                .entries
                .iter()
                .any(|entry| entry.notice.message.starts_with("Terminal input rejected"))
        );
    }
}

#[test]
fn input_notice_preserves_the_actual_live_app_original_and_next_input() {
    let (mut app, card, mut peer) = fixture();
    let proof = peer.handle.proof().unwrap();
    let continuity = terminal_continuity(app.card_state(card)).unwrap().clone();
    let recovery_attempts = app.terminal_recovery_attempts.clone();
    let uncertain = app.uncertain_terminal_proofs.clone();
    let mut requests = peer.observed.try_iter().collect::<Vec<_>>();
    peer.handle.input(b"accepted prefix".to_vec()).unwrap();
    loop {
        let request = peer.observed.recv_timeout(Duration::from_secs(3)).unwrap();
        let accepted =
            matches!(&request, ObservedInput::Input(bytes) if bytes == b"accepted prefix");
        requests.push(request);
        if accepted {
            break;
        }
    }
    let durable = Arc::clone(&app.durable);
    reset();
    const MESSAGE: &str = "Unsent input was canceled. Previously sent input was preserved.";
    apply(
        &mut app,
        card,
        &mut peer,
        Some(WireTerminalEvent::InputNotice {
            operation: "cancel terminal input".into(),
            message: MESSAGE.into(),
        }),
    );
    assert_eq!(counts(), (0, 0), "input notice changes only presentation");
    assert!(Arc::ptr_eq(&durable, &app.durable));
    assert_eq!(terminal_continuity(app.card_state(card)), Some(&continuity));
    assert_eq!(app.terminal_recovery_attempts, recovery_attempts);
    assert_eq!(app.uncertain_terminal_proofs, uncertain);
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Terminal(surface))
            if surface.terminal_id() == peer.handle.terminal_id()
                && surface.host_proof() == Some(proof.clone())
    ));
    assert!(ended_terminal(app.card_state(card)).is_none());
    assert!(app.notices.entries.iter().any(|entry| {
        entry.notice.severity == NoticeSeverity::Warning && entry.notice.message == MESSAGE
    }));
    // A different output generation and a new user interrupt still use the
    // original open connection; the accepted prefix must never be replayed.
    peer.sequence += 1;
    let generation = peer.sequence;
    let mut output = RenderSnapshot::empty();
    output.generation = generation;
    output.columns = 1;
    output.rows = 1;
    output.scroll.total = 1;
    output.scroll.len = 1;
    output.lines = vec![Arc::new(crate::terminal::RowSnapshot {
        cells: vec![crate::terminal::CellSnapshot {
            text: crate::terminal::CellText::new("Z"),
            ..Default::default()
        }],
        ..Default::default()
    })]
    .into();
    peer.send
        .send(ServerMessage::Snapshot {
            terminal_id: peer.handle.terminal_id(),
            snapshot: TerminalSnapshot::full(&output),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while peer.handle.generation() < generation {
        assert!(
            Instant::now() < deadline,
            "output after the nonfatal notice"
        );
        thread::yield_now();
    }
    drop(app.update(Message::TerminalWake { card, generation }));
    assert_eq!(peer.handle.snapshot().generation, generation);
    assert_eq!(peer.handle.snapshot().lines[0].cells[0].text.as_str(), "Z");
    peer.handle.input(vec![3]).unwrap();
    loop {
        let request = peer.observed.recv_timeout(Duration::from_secs(3)).unwrap();
        let interrupt = matches!(&request, ObservedInput::Input(bytes) if bytes == &[3]);
        requests.push(request);
        if interrupt {
            break;
        }
    }
    requests.extend(peer.observed.try_iter());
    assert_eq!(
        requests
            .iter()
            .filter(|request| matches!(request, ObservedInput::Connect))
            .count(),
        1,
        "the nonfatal notice must not reconnect the original attachment"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| matches!(request,
            ObservedInput::Input(bytes) if bytes == b"accepted prefix"))
            .count(),
        1,
        "never replay previously delivered input"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| matches!(request,
            ObservedInput::Input(bytes) if bytes == &[3]))
            .count(),
        1,
        "one new user interrupt, no automatic retry"
    );
    assert!(!peer.worker.as_ref().unwrap().is_finished());
    assert_eq!(peer.handle.proof(), Some(proof));
}

#[test]
fn late_input_notice_cannot_replace_a_fatal_or_completed_original() {
    for completed in [false, true] {
        let (mut app, card, mut peer) = fixture();
        let outcome = if completed {
            WireTerminalEvent::Completed {
                exit_code: 7,
                signal: None,
                intentional: false,
            }
        } else {
            // Even identical recovery wording is fatal when the trusted
            // producer classified it as Error. App never matches error text.
            WireTerminalEvent::Error {
                operation: "cancel terminal input".into(),
                message: "Unsent input was canceled. Previously sent input was preserved.".into(),
            }
        };
        peer.publish(Some(outcome));
        let generation = peer.publish(Some(WireTerminalEvent::InputNotice {
            operation: "cancel terminal input".into(),
            message: "late notice must not replace the original outcome".into(),
        }));
        drop(app.update(Message::TerminalWake { card, generation }));
        if completed {
            assert!(ended_terminal(app.card_state(card)).is_some());
            assert!(app.notices.entries.iter().any(|entry| {
                entry.notice.severity == NoticeSeverity::Error
                    && entry.notice.message == "terminal exited with status 7"
            }));
        } else {
            assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
            assert!(app.notices.entries.iter().any(|entry| {
                entry.notice.severity == NoticeSeverity::Error
                    && entry
                        .notice
                        .message
                        .contains("Previously sent input was preserved")
            }));
        }
        assert!(!app.notices.entries.iter().any(|entry| {
            entry
                .notice
                .message
                .contains("late notice must not replace")
        }));
    }
}

#[test]
fn refused_key_before_completion_keeps_the_actual_app_surface_and_exit_status() {
    for (message, retryable) in [
        ("the terminal worker has closed", true),
        ("terminal output has completed", false),
    ] {
        let (mut app, card, mut peer) = fixture();
        let proof = peer.handle.proof().unwrap();
        let continuity = terminal_continuity(app.card_state(card)).unwrap().clone();
        let uncertain = app.uncertain_terminal_proofs.clone();
        let recovery_attempts = app.terminal_recovery_attempts.clone();
        let mut observed = peer.observed.try_iter().collect::<Vec<_>>();
        *peer.reject_next_key.lock().unwrap() = Some(ProtocolError::new(
            ProtocolErrorCode::TerminalUnavailable,
            message,
            retryable,
        ));
        let key = crate::terminal::KeyInput {
            action: crate::terminal::KeyAction::Release,
            key: crate::terminal::TerminalKey::Enter,
            modifiers: crate::terminal::KeyModifiers::default(),
            text: None,
            unshifted_codepoint: None,
        };
        peer.handle.key(key.clone()).unwrap();
        loop {
            let request = peer.observed.recv_timeout(Duration::from_secs(3)).unwrap();
            let done = matches!(&request, ObservedInput::Key(actual) if *actual == key);
            observed.push(request);
            if done {
                break;
            }
        }
        let events = peer.handle.events();
        let deadline = Instant::now() + Duration::from_secs(3);
        while events.is_empty() {
            assert!(Instant::now() < deadline, "actual correlated key refusal");
            thread::sleep(Duration::from_millis(1));
        }
        // Force the refusal through App before any completion exists. A
        // generic Error here used to replace the retained terminal surface.
        let durable = Arc::clone(&app.durable);
        drop(app.update(Message::TerminalWake {
            card,
            generation: peer.handle.generation(),
        }));
        assert!(Arc::ptr_eq(&durable, &app.durable));
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Terminal(surface))
            if surface.terminal_id() == peer.handle.terminal_id() && surface.host_proof() == Some(proof.clone()))
        );
        assert_eq!(terminal_continuity(app.card_state(card)), Some(&continuity));
        assert!(
            ended_terminal(app.card_state(card)).is_none(),
            "a refusal is not an exit receipt"
        );
        assert_eq!(app.terminal_recovery_attempts, recovery_attempts);
        assert_eq!(app.uncertain_terminal_proofs, uncertain);
        assert!(
            app.notices
                .entries
                .iter()
                .any(|entry| entry.notice.severity == NoticeSeverity::Warning
                    && entry.notice.message.contains("this input was not sent"))
        );
        apply(
            &mut app,
            card,
            &mut peer,
            Some(WireTerminalEvent::Completed {
                exit_code: 23,
                signal: None,
                intentional: false,
            }),
        );
        assert!(matches!(
            &ended_terminal(app.card_state(card)).unwrap().continuity,
            TerminalContinuity::RelaunchAvailable { last_exit: Some(exit), .. }
                if exit.code == Some(23) && exit.signal.is_none()
        ));
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Terminal(surface))
            if surface.terminal_id() == peer.handle.terminal_id() && surface.host_proof() == Some(proof.clone()))
        );
        assert_eq!(
            app.card_state(card).unwrap().residency,
            Residency::Hibernated
        );
        assert!(
            app.notices
                .entries
                .iter()
                .any(|entry| entry.notice.severity == NoticeSeverity::Error
                    && entry.notice.message == "terminal exited with status 23")
        );
        observed.extend(peer.observed.try_iter());
        assert_eq!(
            observed
                .iter()
                .filter(|r| matches!(r, ObservedInput::Connect))
                .count(),
            1
        );
        assert_eq!(
            observed
                .iter()
                .filter(|r| matches!(r, ObservedInput::Key(actual) if *actual == key))
                .count(),
            1,
            "no reconnect or automatic replay of the refused physical key"
        );
        assert!(!peer.worker.as_ref().unwrap().is_finished());
    }
}

#[test]
fn worker_closed_wording_in_a_real_wire_error_remains_fatal() {
    let (mut app, card, mut peer) = fixture();
    apply(
        &mut app,
        card,
        &mut peer,
        Some(WireTerminalEvent::Error {
            operation: "send terminal key".into(),
            message: "the terminal worker has closed".into(),
        }),
    );
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
    assert!(ended_terminal(app.card_state(card)).is_none());
    assert!(app.notices.entries.iter().any(|entry| {
        entry.notice.severity == NoticeSeverity::Error
            && entry
                .notice
                .message
                .contains("the terminal worker has closed")
    }));
    assert!(
        !app.notices
            .entries
            .iter()
            .any(|entry| entry.notice.message.contains("Terminal input rejected"))
    );
}
