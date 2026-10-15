//! Real hosted-client delivery into Application::update; no daemon or PTY.
use super::*;
use crate::terminal::{RenderSnapshot, ipc::{ClientEnvelope, ClientRequest, FrameCodec,
    FrameError, MAX_FRAME_BYTES, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
    ServerResponse, TerminalDescriptor, TerminalSnapshot, WireTerminalEvent, write_frame}};
use std::{cell::Cell, os::unix::net::UnixListener, sync::atomic::{AtomicBool, Ordering}, thread};

thread_local! {
    static PROJECTIONS: Cell<usize> = const { Cell::new(0) };
    static PROTECTIONS: Cell<usize> = const { Cell::new(0) };
}
pub(super) fn record_projection() { PROJECTIONS.set(PROJECTIONS.get() + 1); }
pub(super) fn record_protection() { PROTECTIONS.set(PROTECTIONS.get() + 1); }
fn counts() -> (usize, usize) { (PROJECTIONS.get(), PROTECTIONS.get()) }
fn reset() { PROJECTIONS.set(0); PROTECTIONS.set(0); }

struct Peer {
    _directory: tempfile::TempDir,
    handle: HostedTerminalHandle,
    send: crossbeam_channel::Sender<ServerMessage>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    sequence: u64,
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
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                if stopping.load(Ordering::Relaxed) { return; }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline);
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("private accept: {error}"),
                }
            };
            stream.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
            let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
            let mut connected = false;
            while !stopping.load(Ordering::Relaxed) {
                for message in messages.try_iter() {
                    if write_frame(&mut stream, &ServerEnvelope {
                        version: PROTOCOL_VERSION, request_id: None, proof: proof.clone(), message,
                    }).is_err() { return; }
                }
                let request: ClientEnvelope = match codec.read(&mut stream) {
                    Ok(Some(request)) => request,
                    Ok(None) => return,
                    Err(FrameError::Io(error)) if matches!(error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                    Err(error) => panic!("private decode: {error}"),
                };
                let response = if !connected {
                    assert!(matches!(request.request, ClientRequest::Connect { .. }));
                    connected = true;
                    ServerResponse::Connected { proof: proof.clone(), resumed: vec![descriptor.clone()] }
                } else { ServerResponse::Accepted };
                if write_frame(&mut stream, &ServerEnvelope {
                    version: PROTOCOL_VERSION, request_id: Some(request.request_id),
                    proof: proof.clone(), message: ServerMessage::Response { response },
                }).is_err() { return; }
            }
        });
        let handle = HostedTerminalHandle::connect_at(socket, id, Some(expected_proof), |_| {}).unwrap();
        let peer = Self { _directory: directory, handle, send, stop, worker: Some(worker), sequence: 0 };
        peer.handle.wait_until_attached(Duration::from_secs(3)).unwrap();
        peer
    }
    fn publish(&mut self, event: Option<WireTerminalEvent>) -> u64 {
        self.sequence += 1;
        let id = self.handle.terminal_id();
        if let Some(event) = event {
            self.send.send(ServerMessage::Event { terminal_id: id, sequence: self.sequence, event }).unwrap();
        }
        let mut snapshot = RenderSnapshot::empty();
        snapshot.generation = self.sequence;
        self.send.send(ServerMessage::Snapshot {
            terminal_id: id, snapshot: TerminalSnapshot::full(&snapshot),
        }).unwrap();
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
        if let Some(worker) = self.worker.take() { worker.join().unwrap(); }
    }
}

fn fixture() -> (Cagents, CardId, Peer) {
    let mut app = super::tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let CardKind::Terminal(mut session) = app.card_state(card).unwrap().kind.clone() else { panic!() };
    let proof = HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("output-test"),
        boot_id: crate::model::BootId::new("output-test"),
        host_incarnation: crate::model::HostIncarnation::new("output-test"),
    };
    session.continuity = TerminalContinuity::Live {
        host_session_id: proof.host_session_id.clone(), boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    app.card_state_mut(card).unwrap().kind = CardKind::Terminal(session.clone());
    let peer = Peer::new(session, proof);
    app.cards.insert(card, CardRuntime::Terminal(peer.handle.clone().into()));
    drop(app.update(Message::TerminalWake { card, generation: 0 }));
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
        app.known_worktree_roots.insert(PathBuf::from(format!("/output-fixture/root-{index}")));
    }
    let durable = Arc::clone(&app.durable);
    app.status = Some(TransientNotice::warning("output test notice"));
    for _ in 0..32 { apply(&mut app, card, &mut peer, None); }
    assert_eq!(counts(), (0, 0));
    assert!(Arc::ptr_eq(&durable, &app.durable));
    assert_eq!(peer.handle.snapshot().generation, 32);
    assert!(app.notices.entries.iter().any(|notice| notice.notice.message == "output test notice"));
}

#[test]
fn semantic_terminal_changes_invalidate_but_identical_reports_do_not() {
    let (mut app, card, mut peer) = fixture();
    let events = [
        WireTerminalEvent::TitleChanged { title: "output agent".into() },
        WireTerminalEvent::WorkingDirectoryChanged { path: "/output-fixture/new".into() },
        WireTerminalEvent::ResumeHintChanged { hint: Some("resume-output".into()) },
        WireTerminalEvent::AgentActivityChanged { activity: Some(crate::agent::Activity {
            provider: crate::agent::Provider::Claude, state: crate::agent::ActivityState::Working,
        }) },
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
    apply(&mut app, card, &mut peer, Some(WireTerminalEvent::Completed {
        exit_code: 0, signal: None, intentional: false,
    }));
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
        request, text: "copied output".into(),
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
