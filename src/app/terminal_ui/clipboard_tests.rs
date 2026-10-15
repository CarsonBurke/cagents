//! Actual CopySelection request and held hosted-client reply, without a daemon or PTY.
use super::*;
use crate::terminal::{RenderSnapshot, ipc::{ClientEnvelope, ClientRequest, FrameCodec, FrameError, MAX_FRAME_BYTES, PROTOCOL_VERSION, ServerEnvelope, ServerMessage, ServerResponse, TerminalDescriptor, TerminalSnapshot, WireTerminalEvent, write_frame}};
use std::{os::unix::net::UnixListener, sync::atomic::{AtomicBool, Ordering}, thread};
struct Peer {
    _directory: tempfile::TempDir,
    handle: HostedTerminalHandle,
    send: crossbeam_channel::Sender<ServerMessage>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    sequence: u64,
    copies: crossbeam_channel::Receiver<(uuid::Uuid, crate::terminal::SearchMatch)>,
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
        let (copy_tx, copies) = crossbeam_channel::bounded(2);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let expected_proof = proof.clone();
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
                let response = if !connected {
                    assert!(matches!(request.request, ClientRequest::Connect { .. }));
                    connected = true;
                    ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![descriptor.clone()],
                    }
                } else {
                    if let ClientRequest::CopyTerminalSelection { request, selection, .. } = request.request {
                        copy_tx.send((request, selection)).unwrap();
                    }
                    ServerResponse::Accepted
                };
                if write_frame(
                    &mut stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request.request_id),
                        proof: proof.clone(),
                        message: ServerMessage::Response { response },
                    },
                )
                .is_err()
                {
                    return;
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
            copies,
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
    let mut app = crate::app::tests::context_inspector_test_app(true);
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
    (app, card, peer)
}
#[test]
fn terminal_selection_copy_reply_obeys_latest_clipboard_intent() {
    use iced::futures::{FutureExt as _, StreamExt as _};
    let (mut app, card, mut peer) = fixture();
    let selection = crate::terminal::SearchMatch { start_row: 0, start_column: 0, end_row: 0, end_column: 4 };
    for newer_copy in [false, true] {
        drop(app.update(Message::TerminalFind(Action::CopySelection(card, selection.clone()))));
        let (request, actual_selection) = peer.copies.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(actual_selection, selection);
        assert_eq!(app.terminal_find[&card].pending_copy.as_ref().unwrap().request, request);
        if newer_copy {
            drop(app.update(Message::ChromeUi(crate::app::chrome_ui::Action::CopyPath(card))));
        }
        let wrong_generation = peer.publish(Some(WireTerminalEvent::ClipboardText { request: uuid::Uuid::new_v4(), text: "unrelated clipboard reply".into() }));
        drop(app.update(Message::TerminalWake { card, generation: wrong_generation }));
        assert_eq!(app.terminal_find[&card].pending_copy.as_ref().unwrap().request, request, "unrelated reply cannot consume a current slot");
        let generation = peer.publish(Some(WireTerminalEvent::ClipboardText { request, text: "held terminal selection".into() }));
        let task = app.update(Message::TerminalWake { card, generation });
        assert!(app.terminal_find[&card].pending_copy.is_none(), "matching reply always retires the slot");
        let mut writes = Vec::new();
        if let Some(mut stream) = iced::runtime::task::into_stream(task) {
            while let Some(action) = stream.next().now_or_never().expect("reply effects are immediate") {
                if let iced::runtime::Action::Clipboard(iced::runtime::clipboard::Action::Write { contents, target }) = action {
                    assert_eq!(target, iced::advanced::clipboard::Kind::Standard);
                    writes.push(contents);
                }
            }
        }
        assert_eq!(writes, if newer_copy { Vec::<String>::new() } else { vec!["held terminal selection".to_owned()] });
    }
}
