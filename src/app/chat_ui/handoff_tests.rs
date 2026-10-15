//! Reducer ownership and real framed client fixtures. Production daemon/provider
//! futures returned by App::update are deliberately left unpolled; the private
//! peer exercises the exact descriptor-first client API without a real PTY.
use super::*;
use crate::terminal::ipc::{
    ClientEnvelope, ClientRequest, FrameCodec, FrameError, MAX_FRAME_BYTES,
    PROTOCOL_VERSION, ServerEnvelope, ServerMessage, ServerResponse,
    TerminalDescriptor, TerminalHandoffReceipt,
};
use std::{os::unix::net::UnixListener, thread};

struct Peer {
    _directory: tempfile::TempDir,
    handle: HostedTerminalHandle,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Peer {
    fn attach(descriptor: TerminalDescriptor, proof: HostSessionProof) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("handoff.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = HostedTerminalHandle::connect_descriptor_proved_at(
            socket, descriptor.clone(), proof.clone(), false, |_| {},
        ).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
            let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
            let mut phase = 0;
            while !stopping.load(Ordering::Acquire) {
                let request: ClientEnvelope = match codec.read(&mut stream) {
                    Ok(Some(request)) => request,
                    Ok(None) => break,
                    Err(FrameError::Io(error)) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                    Err(error) => panic!("private handoff frame: {error}"),
                };
                let response = match phase {
                    0 => {
                        assert!(matches!(&request.request, ClientRequest::Connect { expected_host: Some(expected), resume }
                            if expected == &proof && resume.is_empty()));
                        phase = 1;
                        ServerResponse::Connected { proof: proof.clone(), resumed: Vec::new() }
                    }
                    1 => {
                        assert!(matches!(&request.request, ClientRequest::AttachTerminalSource { source, execution_instance, .. }
                            if source == &descriptor.session && *execution_instance == descriptor.execution_instance));
                        phase = 2;
                        let mut attached = descriptor.clone();
                        attached.attached = true;
                        ServerResponse::Attached { terminal: attached }
                    }
                    _ => {
                        assert!(!matches!(request.request, ClientRequest::List | ClientRequest::Spawn { .. }));
                        ServerResponse::Accepted
                    }
                };
                if crate::terminal::ipc::write_frame(&mut stream, &ServerEnvelope {
                    version: PROTOCOL_VERSION, request_id: Some(request.request_id), proof: proof.clone(),
                    message: ServerMessage::Response { response },
                }).is_err() { break; }
            }
        });
        handle.wait_until_attached(Duration::from_secs(3)).unwrap();
        Self { _directory: directory, handle, stop, worker: Some(worker) }
    }
    fn started(&self, receipt: &TerminalHandoffReceipt) -> StartedTerminal {
        StartedTerminal { handle: self.handle.clone().into(), session: receipt.terminal.session.clone(), spawned: false }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() { worker.join().unwrap(); }
    }
}

fn returning() -> (Cagents, CardId, Target, TerminalHandoffReceipt) {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = super::tests::live_chat(&mut app, Some("0199a2b4-5c6d-7e8f-9a0b-1c2d3e4f5a6b"));
    drop(app.update(Message::ChatUi(Action::Switch(card))));
    let target = app.chat_ui.phases[&card].target();
    let expected = app.chat_ui.handoff_bindings[&target].binding.clone();
    let proof = expected.source.proof.clone().unwrap();
    let mut terminal = return_terminal(&expected.source).unwrap();
    terminal.continuity = TerminalContinuity::Live {
        host_session_id: proof.host_session_id, boot_id: proof.boot_id, host_incarnation: proof.host_incarnation,
    };
    let mut descriptor = TerminalDescriptor::relaunchable(terminal.id, terminal.launch.clone(), terminal.transcript_path.clone());
    descriptor.session = terminal;
    descriptor.execution_instance = uuid::Uuid::new_v4();
    let receipt = TerminalHandoffReceipt {
        operation: uuid::Uuid::new_v4(), source: expected.source.clone(),
        source_execution_instance: expected.execution_instance, source_binding: expected,
        retained_binding: None, terminal_native_epoch: uuid::Uuid::new_v4(), terminal: descriptor,
    };
    (app, card, target, receipt)
}
fn queued(task: Task<Message>) -> Message {
    use iced::futures::StreamExt;
    let mut stream = iced::runtime::task::into_stream(task).expect("captured continuation");
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(cosmic::Action::App(message @ Message::CloseQueuedCard(_))) = action { return message; }
        }
        panic!("missing captured close continuation");
    })
}

pub(super) fn end_waits_for_original_attachment_then_closes_only_its_worker() {
    let (mut app, card, target, receipt) = returning();
    super::tests::request_end_chat(&mut app, card);
    drop(app.update(Message::ChatUi(Action::HandoffFinished(target, app.chat_ui.handoff_bindings[&target].operation, Ok(receipt.clone())))));
    assert!(app.terminal_start_in_flight(card));
    assert!(app.chat_ui.phases.is_empty(), "actor cleanup cannot race the outstanding attachment");
    assert!(!app.chat_ui.handoff_closes[&card].attached);
    let operation = app.terminal_start_operation(card).unwrap();
    let peer = Peer::attach(receipt.terminal.clone(), receipt.source.proof.clone().unwrap());
    drop(app.update(Message::ChatUi(Action::HandoffAttached(card, operation, Arc::new(receipt.clone()), Ok(peer.started(&receipt))))));
    assert!(!app.terminal_start_in_flight(card));
    let closing = app.chat_ui.phases[&card].target();
    assert!(matches!(app.chat_ui.phases[&card], Phase::Closing { restart: None, .. }));
    let delivery = queued(app.update(Message::ChatUi(Action::CloseFinished(closing, Ok(())))));
    assert!(!app.chat_has_parked_runtime(card));
    drop(app.update(delivery));
    let intent = app.terminal_close.clone().expect("exact returned worker confirmation");
    assert_eq!(app.terminal_worker_for_shared_removal(card), Some(receipt.terminal.execution_instance));
    drop(app.update(Message::TerminalCloseFinished {
        card, terminal: intent.terminal, proof: intent.proof, generation: intent.generation,
        outcome: terminal_close::CloseOutcome::Closed,
    }));
    if let Some(removal) = app.pending_card_removal_target(card) { drop(app.update(Message::CommitCloseTarget(removal))); }
    assert!(!app.card_state(card).unwrap().existence.is_open());
    assert!(!app.workspace.order().any(|id| id == card));
    assert!(app.chat_ui.handoff_closes.is_empty());
    assert!(app.chat_ui.phases.is_empty());
    assert_eq!(app.durable.terminal_handoff_receipts[&receipt.operation].receipt, receipt);
}

#[test]
fn post_retirement_unknown_binding_keeps_the_exact_completed_return() {
    for completed in [false, true] {
        let (mut app, card, target, mut receipt) = returning();
        app.chat_ui.handoff_bindings[&target].handle.as_ref().unwrap().clear_source_binding_for_test();
        if completed {
            receipt.terminal.session.continuity = TerminalContinuity::Exited {
                last_exit: crate::model::TerminalExit { code: Some(0), signal: None, occurred_at_unix_ms: 1 },
            };
        }
        drop(app.update(Message::ChatUi(Action::HandoffFinished(target, app.chat_ui.handoff_bindings[&target].operation, Ok(receipt.clone())))));
        assert!(matches!(&app.card_state(card).unwrap().kind, CardKind::Terminal(source) if source == &receipt.terminal.session));
        let saved = &app.durable.terminal_handoff_receipts[&receipt.operation];
        assert!(saved.converted);
        assert_eq!(saved.receipt, receipt);
        let operation = app.terminal_start_operation(card).unwrap();
        let peer = Peer::attach(receipt.terminal.clone(), receipt.source.proof.clone().unwrap());
        drop(app.update(Message::ChatUi(Action::HandoffAttached(card, operation, Arc::new(receipt.clone()), Ok(peer.started(&receipt))))));
        assert!(!app.terminal_start_in_flight(card));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Terminal(_))));
        assert_eq!(app.durable.terminal_handoff_receipts[&receipt.operation].receipt.terminal.session.continuity, receipt.terminal.session.continuity);
    }
}

#[test]
fn exact_dead_reader_replay_keeps_end_until_attachment_without_parking_the_dead_chat() {
    let (mut app, card, target, receipt) = returning();
    let captured = app.chat_ui.handoff_bindings[&target].clone();
    let original = captured.handle.as_ref().unwrap().clone();
    super::tests::request_end_chat(&mut app, card);
    drop(app.update(Message::ChatUi(Action::HandoffFinished(target, captured.operation, Err("lost reply".into())))));
    original.disconnect_reader_for_test();
    drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Disconnected("original EOF".into())))));
    drop(app.update(Message::ChatUi(Action::Switch(card))));
    let retry = app.chat_ui.handoff_bindings[&target].operation;
    assert_ne!(retry, captured.operation);
    assert!(matches!(app.chat_ui.phases.get(&card), Some(Phase::Returning { close_requested: true, .. })));
    // Inject a typed producer result only. Actual dead-client/real-host replay
    // validation is covered in the owning producer's separate framed fixtures.
    drop(app.update(Message::ChatUi(Action::HandoffFinished(target, retry, Ok(receipt.clone())))));
    assert!(!original.is_connected(), "replay never revives the old reader");
    assert!(!app.chat_has_parked_runtime(card), "stale cached non-exited metadata cannot park a dead UI attachment");
    assert!(app.terminal_start_in_flight(card));
    assert!(app.chat_ui.handoff_closes.contains_key(&card));
    assert!(app.card_state(card).unwrap().existence.is_open());
    let operation = app.terminal_start_operation(card).unwrap();
    let peer = Peer::attach(receipt.terminal.clone(), receipt.source.proof.clone().unwrap());
    let next = app.update(Message::ChatUi(Action::HandoffAttached(card, operation, Arc::new(receipt.clone()), Ok(peer.started(&receipt)))));
    assert!(!app.terminal_start_in_flight(card));
    let delivery = queued(next);
    drop(app.update(delivery));
    assert_eq!(app.terminal_worker_for_shared_removal(card), Some(receipt.terminal.execution_instance));
    assert!(app.card_state(card).unwrap().existence.is_open(), "only exact worker cleanup may now finish End");
    assert!(app.chat_ui.phases.is_empty(), "no dead UI actor cleanup is substituted");
}

#[test]
fn dead_reader_cached_pair_requires_the_exact_original_return_receipt() {
    for corroborated in [false, true] {
        let (mut app, card, target, mut receipt) = returning();
        let captured = app.chat_ui.handoff_bindings[&target].clone();
        let original = captured.handle.as_ref().unwrap().clone();
        super::tests::request_end_chat(&mut app, card);
        drop(app.update(Message::ChatUi(Action::HandoffFinished(target, captured.operation, Err("lost final reply".into())))));
        let mut cached = captured.binding.clone();
        cached.backend = uuid::Uuid::new_v4();
        cached.source.terminal = Some(receipt.terminal.session.clone());
        original.publish_source_binding_for_test(cached.clone());
        // Live positive replacement is still a refusal before any request.
        drop(app.update(Message::ChatUi(Action::Switch(card))));
        assert!(app.chat_ui.phases.is_empty());
        assert_eq!(app.chat_ui.handoff_bindings[&target].operation, captured.operation);
        original.disconnect_reader_for_test();
        drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Disconnected("old EOF".into())))));
        drop(app.update(Message::ChatUi(Action::Switch(card))));
        let retry = app.chat_ui.handoff_bindings[&target].clone();
        assert_ne!(retry.operation, captured.operation);
        assert_eq!(retry.binding, captured.binding, "cached metadata never refreshes the submitted original expected");
        if corroborated { receipt.retained_binding = Some(cached); }
        // These injected typed results test the UI's final corroboration gate;
        // the producer separately exercises the actual fresh control exchange.
        drop(app.update(Message::ChatUi(Action::HandoffFinished(target, retry.operation, Ok(receipt.clone())))));
        assert_eq!(matches!(&app.card_state(card).unwrap().kind, CardKind::Terminal(_)), corroborated);
        assert_eq!(app.terminal_start_in_flight(card), corroborated);
        assert_eq!(app.chat_ui.handoff_closes.contains_key(&card), corroborated);
        assert!(!app.chat_has_parked_runtime(card));
        assert_eq!(app.durable.terminal_handoff_receipts[&receipt.operation].converted, corroborated);
        assert!(app.card_state(card).unwrap().existence.is_open());
    }
}

#[test]
fn a_new_handle_with_unknown_binding_does_not_receive_the_earlier_return() {
    let (mut app, card, target, receipt) = returning();
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else { panic!("chat"); };
    let current = runtime.session.clone();
    runtime.connection = Connection::Live(Arc::new(ClientHandle::fixture(current.clone(), Snapshot { ready: true, session_id: current.launch.session_id.clone(), ..Snapshot::default() })));
    drop(app.update(Message::ChatUi(Action::HandoffFinished(target, app.chat_ui.handoff_bindings[&target].operation, Ok(receipt.clone())))));
    assert!(matches!(app.card_state(card).unwrap().kind, CardKind::Chat(_)));
    assert!(!app.terminal_start_in_flight(card));
    assert!(!app.durable.terminal_handoff_receipts[&receipt.operation].converted);
    drop(app.update(Message::TerminalHandoffsCommitted { result: Ok(vec![(TerminalHandoffAck::Receipt(receipt.clone()), Ok(()))]) }));
    assert_eq!(app.durable.terminal_handoff_receipts[&receipt.operation].receipt, receipt, "an unapplied receipt cannot be consumed by an ACK result");
}

#[test]
fn exact_receipt_ack_cannot_remove_a_new_operation_or_replacement_payload() {
    let (mut app, card, target, receipt) = returning();
    drop(app.update(Message::ChatUi(Action::HandoffFinished(target, app.chat_ui.handoff_bindings[&target].operation, Ok(receipt.clone())))));
    let mut next = receipt.clone();
    next.operation = uuid::Uuid::new_v4();
    next.terminal.execution_instance = uuid::Uuid::new_v4();
    Arc::make_mut(&mut app.durable).terminal_handoff_receipts.insert(next.operation, crate::model::SavedTerminalHandoff { card, receipt: next.clone(), converted: true });
    drop(app.update(Message::TerminalHandoffsCommitted { result: Ok(vec![(TerminalHandoffAck::Receipt(receipt.clone()), Ok(()))]) }));
    assert!(!app.durable.terminal_handoff_receipts.contains_key(&receipt.operation));
    assert_eq!(app.durable.terminal_handoff_receipts[&next.operation].receipt, next);
    let mut wrong = next.clone(); wrong.terminal_native_epoch = uuid::Uuid::new_v4();
    drop(app.update(Message::TerminalHandoffsCommitted { result: Ok(vec![(TerminalHandoffAck::Receipt(wrong), Ok(()))]) }));
    assert_eq!(app.durable.terminal_handoff_receipts[&next.operation].receipt, next);
}

#[test]
fn escaped_receipt_metadata_uses_the_promised_json_budget_not_wire_size() {
    let (mut app, card, _target, mut receipt) = returning();
    let escaped = "\u{0001}".repeat(60 * 1024);
    receipt.source.launch.recipe.environment.insert("HANDOFF_TEST_ESCAPED".into(), escaped.clone());
    receipt.source_binding.source = receipt.source.clone();
    receipt.terminal.session.launch.environment.insert("HANDOFF_TEST_ESCAPED".into(), escaped);
    let mut wire = Vec::new();
    FrameCodec::new(MAX_FRAME_BYTES).write(&mut wire, &receipt).unwrap();
    let json_bytes = crate::terminal::ipc::terminal_handoff_json_bytes(&receipt).unwrap();
    assert!(json_bytes > wire.len() * 4);
    assert!(app.retain_terminal_handoff_receipt(card, &receipt));
    assert_eq!(crate::model::validate_terminal_handoffs(&app.durable.terminal_handoff_receipts).unwrap(), json_bytes + 256);
    assert_eq!(app.durable.terminal_handoff_receipts[&receipt.operation].receipt, receipt);
}

#[test]
fn return_admission_refuses_full_receipt_capacity_before_creating_a_request() {
    let (mut app, card, original, receipt) = returning();
    // Retire this unpolled request to exercise a fresh actual Switch intake.
    app.chat_ui.phases.remove(&card);
    app.chat_ui.handoff_bindings.remove(&original);
    for _ in 0..crate::model::MAX_TERMINAL_HANDOFFS {
        let mut saved = receipt.clone();
        saved.operation = uuid::Uuid::new_v4();
        Arc::make_mut(&mut app.durable).terminal_handoff_receipts.insert(saved.operation,
            crate::model::SavedTerminalHandoff { card, receipt: saved, converted: false });
    }
    let source = app.card_state(card).unwrap().kind.clone();
    drop(app.update(Message::ChatUi(Action::Switch(card))));
    assert!(app.chat_ui.phases.is_empty());
    assert!(app.chat_ui.handoff_bindings.is_empty());
    assert!(!app.terminal_start_in_flight(card));
    assert_eq!(app.card_state(card).unwrap().kind, source);
}

#[test]
fn receipt_owned_issued_thread_learning_keeps_the_original_end_and_pins_known_threads() {
    for known_other in [false, true] {
        let (mut app, card, target, receipt) = returning();
        let CardKind::Chat(issued) = &mut app.card_state_mut(card).unwrap().kind else { panic!("issued chat"); };
        issued.launch.session_id = known_other.then(|| "0199a2b4-5c6d-7e8f-9a0b-1c2d3e4f5a6c".into());
        super::tests::request_end_chat(&mut app, card);
        assert!(app.chat_ui.end_requests.contains_key(&target));
        if !known_other {
            // The ordinary descriptor adopts already-confirmed thread metadata
            // while the exact typed Return answer is still in flight.
            app.card_state_mut(card).unwrap().kind = CardKind::Chat(receipt.source.clone());
        }
        drop(app.update(Message::ChatUi(Action::HandoffFinished(target, app.chat_ui.handoff_bindings[&target].operation, Ok(receipt.clone())))));
        assert_eq!(app.durable.terminal_handoff_receipts[&receipt.operation].converted, !known_other);
        assert_eq!(app.chat_ui.handoff_closes.contains_key(&card), !known_other);
        assert!(app.chat_ui.phases.is_empty(), "accepted End still waits for its actual attachment");
        if known_other { assert!(matches!(app.card_state(card).unwrap().kind, CardKind::Chat(_))); }
    }
}

#[test]
fn held_old_attachment_cannot_retire_a_new_attachment_of_the_same_receipt() {
    let (mut app, card, target, receipt) = returning();
    super::tests::request_end_chat(&mut app, card);
    drop(app.update(Message::ChatUi(Action::HandoffFinished(target, app.chat_ui.handoff_bindings[&target].operation, Ok(receipt.clone())))));
    let first = app.terminal_start_operation(card).unwrap();
    app.begin_terminal_start(card);
    let second = app.terminal_start_operation(card).unwrap();
    assert_ne!(first, second);
    app.chat_ui.handoff_closes.get_mut(&card).unwrap().attachment_operation = second;
    app.chat_ui.handoff_attachments.insert(second, (card, Arc::new(receipt.clone())));
    let peer = Peer::attach(receipt.terminal.clone(), receipt.source.proof.clone().unwrap());
    drop(app.update(Message::ChatUi(Action::HandoffAttached(card, first, Arc::new(receipt.clone()), Ok(peer.started(&receipt))))));
    assert_eq!(app.terminal_start_operation(card), Some(second));
    assert_eq!(app.chat_ui.handoff_closes[&card].attachment_operation, second);
    assert!(!app.chat_ui.handoff_closes[&card].attached);
    assert!(app.chat_ui.phases.is_empty());
    // Deliver the old result again after its owner was settled. The remaining
    // count and nonce still belong exclusively to the second attachment.
    drop(app.update(Message::ChatUi(Action::HandoffAttached(card, first, Arc::new(receipt.clone()), Ok(peer.started(&receipt))))));
    assert_eq!(app.terminal_start_operation(card), Some(second));
    assert!(app.terminal_start_in_flight(card));
    assert!(app.chat_ui.handoff_attachments.contains_key(&second));
    drop(app.update(Message::ChatUi(Action::HandoffAttached(card, second, Arc::new(receipt.clone()), Ok(peer.started(&receipt))))));
    assert!(!app.terminal_start_in_flight(card));
    assert!(app.chat_ui.handoff_closes.is_empty());
    assert!(matches!(app.chat_ui.phases[&card], Phase::Closing { restart: None, .. }));
}
