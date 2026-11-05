//! Real Unix framing at the hosted adapter boundary; no daemon/provider starts.
use super::*;
use crate::terminal::ipc::WireTerminalEvent;
use crate::terminal::{KeyAction, KeyModifiers, MouseAction, MouseButton, TerminalKey};

struct MockHost {
    _directory: tempfile::TempDir,
    listener: UnixListener,
    stream: UnixStream,
    handle: HostedTerminalHandle,
    proof: HostSessionProof,
    session: TerminalSession,
    attachment: u64,
    wakes: Arc<AtomicUsize>,
}

impl MockHost {
    fn begin(spawn: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("host.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let proof = proof("input-admission");
        let session = session();
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake = wakes.clone();
        let callback = move |_| {
            wake.fetch_add(1, Ordering::Relaxed);
        };
        let handle = if spawn {
            HostedTerminalHandle::attach_or_spawn_at(
                socket,
                session.clone(),
                TerminalSpawnOptions::default(),
                Some(proof.clone()),
                callback,
            )
        } else {
            HostedTerminalHandle::connect_at(socket, session.id, Some(proof.clone()), callback)
        }
        .unwrap();
        let mut stream = accept_connection(&listener);
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        assert!(
            matches!(&connect.request, ClientRequest::Connect { expected_host: Some(expected), .. } if expected == &proof)
        );
        write_frame(
            &mut stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(connect.request_id),
                proof: proof.clone(),
                message: ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: Vec::new(),
                    },
                },
            },
        )
        .unwrap();
        let attachment: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        assert_eq!(attachment.proof, Some(proof.clone()));
        assert!(match &attachment.request {
            ClientRequest::Spawn {
                session: source, ..
            } => spawn && source.id == session.id,
            ClientRequest::Attach { terminal_id, .. } => !spawn && *terminal_id == session.id,
            _ => false,
        });
        let visibility: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        assert!(matches!(
            visibility.request,
            ClientRequest::SetVisibility { .. }
        ));
        let mut this = Self {
            _directory: directory,
            listener,
            stream,
            handle,
            proof,
            session,
            attachment: attachment.request_id,
            wakes,
        };
        this.response(visibility.request_id, ServerResponse::Accepted);
        this
    }

    fn attach(&mut self) {
        self.response(
            self.attachment,
            ServerResponse::Attached {
                terminal: TerminalDescriptor {
                    session: self.session.clone(),
                    generation: 0,
                    event_sequence: 0,
                    oldest_event_sequence: 1,
                    attached: true,
                    transcript: None,
                    activity: None,
                    agent_activity: None,
                    screen_lost_after_crash: false,
                    upgrade_sources: Vec::new(),
                    upgrade_relaunches: Vec::new(),
                    title: None,
                },
            },
        );
        self.handle
            .wait_until_attached(Duration::from_secs(2))
            .unwrap();
        assert!(matches!(
            self.handle
                .events()
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            TerminalEvent::ResumeHintChanged(_)
        ));
        assert!(self.handle.events().try_iter().next().is_none());
        assert_eq!(self.handle.proof(), Some(self.proof.clone()));
    }

    fn send(&mut self, request_id: Option<u64>, message: ServerMessage) {
        write_frame(
            &mut self.stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id,
                proof: self.proof.clone(),
                message,
            },
        )
        .unwrap();
    }
    fn response(&mut self, id: u64, response: ServerResponse) {
        self.send(Some(id), ServerMessage::Response { response });
    }
    fn refuse(&mut self, id: Option<u64>, code: ProtocolErrorCode, retryable: bool) {
        self.send(
            id,
            ServerMessage::Error(ProtocolError::new(
                code,
                "fixture admission refusal",
                retryable,
            )),
        );
    }
    fn request(&mut self) -> ClientEnvelope {
        let request: ClientEnvelope = read_frame(&mut self.stream).unwrap().unwrap();
        assert_eq!(request.proof, Some(self.proof.clone()));
        request
    }
    fn no_reconnect(&self) {
        assert!(
            matches!(self.listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }
    fn reconnect(&self) -> UnixStream {
        let mut next = accept_connection(&self.listener);
        let request: ClientEnvelope = read_frame(&mut next).unwrap().unwrap();
        assert!(
            matches!(request.request, ClientRequest::Connect { expected_host: Some(expected), .. } if expected == self.proof)
        );
        next
    }
    fn event(&mut self, sequence: u64, event: WireTerminalEvent) {
        self.send(
            None,
            ServerMessage::Event {
                terminal_id: self.session.id,
                sequence,
                event,
            },
        );
    }
    fn stop(&self) {
        self.handle.stop_for_test();
        let worker = self.handle.inner.worker.lock().unwrap().take().unwrap();
        let (sent, received) = crossbeam_channel::bounded(1);
        let join = thread::spawn(move || {
            sent.send(worker.join().is_ok()).unwrap();
        });
        assert!(
            received
                .recv_timeout(Duration::from_secs(2))
                .expect("actual hosted worker must join after Stop")
        );
        join.join().unwrap();
    }
}

fn wait_for_rejection(mock: &MockHost) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while mock
        .handle
        .inner
        .published
        .input_rejection
        .lock()
        .unwrap()
        .is_none()
    {
        assert!(
            Instant::now() < deadline,
            "actual framed rejection was not retained"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn correlated_input_queue_full_keeps_attachment_and_never_replays_input() {
    let mut mock = MockHost::begin(false);
    mock.attach();
    let commands = [
        ClientCommand::Input(b"rejected input".to_vec()),
        ClientCommand::Paste("rejected paste".into(), PasteOrigin::Clipboard),
        ClientCommand::Key(KeyInput {
            action: KeyAction::Press,
            key: TerminalKey::Character('x'),
            modifiers: KeyModifiers::default(),
            text: Some("x".into()),
            unshifted_codepoint: Some('x'),
        }),
        ClientCommand::Mouse(MouseInput {
            action: MouseAction::Press,
            button: Some(MouseButton::Left),
            modifiers: KeyModifiers::default(),
            x: 1.0,
            y: 1.0,
            screen_width: 80,
            screen_height: 24,
            cell_width: 1,
            cell_height: 1,
            any_button_pressed: true,
        }),
    ];
    let mut previous = mock.attachment;
    for command in commands {
        let operation = command.operation();
        let expected = command.into_request(mock.session.id);
        // Use the real public input APIs, not a fabricated PendingRequest.
        match &expected {
            ClientRequest::Input { bytes, .. } => mock.handle.input(bytes.clone()).unwrap(),
            ClientRequest::Paste { text, origin, .. } => {
                mock.handle.paste(text.clone(), *origin).unwrap()
            }
            ClientRequest::Key { input, .. } => mock.handle.key(input.clone()).unwrap(),
            ClientRequest::Mouse { input, .. } => mock.handle.mouse(*input).unwrap(),
            _ => unreachable!(),
        }
        let actual = mock.request();
        assert!(actual.request_id > previous);
        previous = actual.request_id;
        assert_eq!(
            serde_json::to_value(&actual.request).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        mock.refuse(Some(actual.request_id), ProtocolErrorCode::QueueFull, true);
        let event = mock
            .handle
            .events()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(
            matches!(event, TerminalEvent::InputRejected { operation: actual, message } if actual == operation && message.contains("input was not accepted"))
        );
        assert_eq!(mock.handle.proof(), Some(mock.proof.clone()));
        mock.handle.wait_until_attached(Duration::ZERO).unwrap();
        mock.no_reconnect();
    }
    mock.handle.input(b"next accepted input".to_vec()).unwrap();
    let next = mock.request();
    assert!(next.request_id > previous);
    assert!(
        matches!(next.request, ClientRequest::Input { terminal_id, bytes } if terminal_id == mock.session.id && bytes == b"next accepted input")
    );
    mock.response(next.request_id, ServerResponse::Accepted);
    mock.event(
        1,
        WireTerminalEvent::TitleChanged {
            title: "same live owner".into(),
        },
    );
    assert_eq!(
        mock.handle
            .events()
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        TerminalEvent::TitleChanged("same live owner".into())
    );
    mock.no_reconnect();
    mock.stop();
}

#[test]
fn saturated_event_queue_retains_coalesced_rejection_without_blocking_shutdown_or_stop() {
    let mut mock = MockHost::begin(false);
    mock.attach();
    for sequence in 1..=EVENT_CAPACITY as u64 {
        mock.event(sequence, WireTerminalEvent::Bell);
    }
    mock.handle.input(b"not accepted".to_vec()).unwrap();
    let input = mock.request();
    mock.refuse(Some(input.request_id), ProtocolErrorCode::QueueFull, true);
    wait_for_rejection(&mock);
    mock.handle
        .paste("also not accepted", PasteOrigin::Clipboard)
        .unwrap();
    let paste = mock.request();
    assert!(matches!(paste.request, ClientRequest::Paste { .. }));
    mock.refuse(Some(paste.request_id), ProtocolErrorCode::QueueFull, true);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if mock
            .handle
            .inner
            .published
            .input_rejection
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(_, operation, _)| *operation == "send terminal input")
        {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    // This real control must round-trip BEFORE the UI drains its full inbox.
    let handle = mock.handle.clone();
    let (finished, result) = crossbeam_channel::bounded(1);
    let shutdown = thread::spawn(move || {
        finished.send(handle.shutdown()).unwrap();
    });
    let request = mock.request();
    assert!(
        matches!(request.request, ClientRequest::ShutdownTerminal { terminal_id } if terminal_id == mock.session.id)
    );
    mock.response(request.request_id, ServerResponse::Accepted);
    assert_eq!(result.recv_timeout(Duration::from_secs(2)).unwrap(), Ok(()));
    shutdown.join().unwrap();
    mock.no_reconnect();
    let before = mock.wakes.load(Ordering::Relaxed);
    let queued = mock.handle.events();
    assert!(
        mock.wakes.load(Ordering::Relaxed) > before,
        "full drain must enqueue another semantic turn"
    );
    assert_eq!(
        queued
            .try_iter()
            .filter(|event| matches!(event, TerminalEvent::Bell))
            .count(),
        EVENT_CAPACITY
    );
    assert!(matches!(
        mock.handle.events().try_recv().unwrap(),
        TerminalEvent::InputRejected {
            operation: "send terminal input",
            ..
        }
    ));
    assert!(
        mock.handle
            .inner
            .published
            .input_rejection
            .lock()
            .unwrap()
            .is_none()
    );
    assert!(mock.handle.events().try_recv().is_err());
    // A full channel with another retained rejection must not delay Stop/join.
    for sequence in EVENT_CAPACITY as u64 + 1..=2 * EVENT_CAPACITY as u64 {
        mock.event(sequence, WireTerminalEvent::Bell);
    }
    mock.handle.input(b"last rejected input".to_vec()).unwrap();
    let input = mock.request();
    mock.refuse(Some(input.request_id), ProtocolErrorCode::QueueFull, true);
    wait_for_rejection(&mock);
    mock.stop();
}

#[test]
fn queue_full_outside_correlated_input_preserves_existing_failure_paths() {
    for kind in [
        "attach",
        "spawn",
        "control",
        "unsolicited",
        "unknown",
        "wrong-proof",
    ] {
        let mut mock = MockHost::begin(kind == "spawn");
        let request_id = if matches!(kind, "attach" | "spawn") {
            Some(mock.attachment)
        } else {
            mock.attach();
            match kind {
                "control" => {
                    mock.handle.focus(true).unwrap();
                    Some(mock.request().request_id)
                }
                "unsolicited" => None,
                "unknown" => Some(u64::MAX),
                "wrong-proof" => {
                    mock.handle
                        .input(b"must not be reclassified".to_vec())
                        .unwrap();
                    Some(mock.request().request_id)
                }
                _ => unreachable!(),
            }
        };
        if kind == "wrong-proof" {
            write_frame(
                &mut mock.stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id,
                    proof: proof("other-incarnation"),
                    message: ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::QueueFull,
                        "must not trust this refusal",
                        true,
                    )),
                },
            )
            .unwrap();
        } else {
            mock.refuse(request_id, ProtocolErrorCode::QueueFull, true);
        }
        let _next = mock.reconnect();
        let events = mock.handle.events().try_iter().collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, TerminalEvent::InputRejected { .. }))
        );
        if !matches!(kind, "unknown" | "wrong-proof") {
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, TerminalEvent::Error { .. })),
                "old failure must remain visible for {kind}"
            );
        }
        mock.stop();
    }
}

#[test]
fn proof_auth_and_wire_errors_cannot_be_reclassified_as_input_refusals() {
    for code in [
        ProtocolErrorCode::ProofMismatch,
        ProtocolErrorCode::PermissionDenied,
    ] {
        let mut mock = MockHost::begin(false);
        let request_id = if code == ProtocolErrorCode::ProofMismatch {
            mock.attach();
            mock.handle.input(b"original input".to_vec()).unwrap();
            mock.request().request_id
        } else {
            mock.attachment
        };
        mock.refuse(
            Some(request_id),
            code,
            code == ProtocolErrorCode::ProofMismatch,
        );
        assert!(matches!(
            mock.handle
                .events()
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            TerminalEvent::Error { .. }
        ));
        assert_eq!(
            mock.handle.events().recv_timeout(Duration::from_secs(2)),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        );
        assert!(mock.handle.wait_until_attached(Duration::ZERO).is_err());
        mock.no_reconnect();
        mock.stop();
    }
    let mut mock = MockHost::begin(false);
    mock.attach();
    mock.handle.input(b"nonretryable refusal".to_vec()).unwrap();
    let input = mock.request();
    mock.refuse(Some(input.request_id), ProtocolErrorCode::QueueFull, false);
    assert!(matches!(
        mock.handle
            .events()
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        TerminalEvent::Error { .. }
    ));
    mock.no_reconnect();
    mock.event(
        1,
        WireTerminalEvent::Error {
            operation: "write terminal input".into(),
            message: "The terminal input queue is full; input was not accepted.".into(),
        },
    );
    assert!(matches!(
        mock.handle
            .events()
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        TerminalEvent::Error { .. }
    ));
    mock.stop();
}

#[test]
fn closed_input_admission_keeps_the_same_connection_for_actual_completion() {
    for (message, retryable) in [
        ("the terminal worker has closed", true),
        ("terminal output has completed", false),
    ] {
        let mut mock = MockHost::begin(false);
        mock.attach();
        let key = KeyInput {
            action: KeyAction::Release,
            key: TerminalKey::Enter,
            modifiers: KeyModifiers::default(),
            text: None,
            unshifted_codepoint: None,
        };
        mock.handle.key(key.clone()).unwrap();
        let request = mock.request();
        assert!(
            matches!(request.request, ClientRequest::Key { terminal_id, input }
            if terminal_id == mock.session.id && input == key)
        );
        mock.send(
            Some(request.request_id),
            ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                message,
                retryable,
            )),
        );
        assert!(matches!(
            mock.handle.events().recv_timeout(Duration::from_secs(2)).unwrap(),
            TerminalEvent::InputRejected { operation: "send terminal key", message }
                if message.contains("this input was not sent") && !message.contains("queue is full")
        ));
        mock.handle.wait_until_attached(Duration::ZERO).unwrap();
        assert_eq!(mock.handle.proof(), Some(mock.proof.clone()));
        mock.no_reconnect();
        mock.event(
            1,
            WireTerminalEvent::Completed {
                exit_code: 17,
                signal: None,
                intentional: false,
            },
        );
        assert!(matches!(
            mock.handle
                .events()
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            TerminalEvent::Completed {
                exit_code: 17,
                signal: None,
                intentional: false
            }
        ));
        // A distinct observational control is the next request: the refused
        // physical key is neither replayed nor silently acknowledged.
        mock.handle.focus(false).unwrap();
        let next = mock.request();
        assert!(next.request_id > request.request_id);
        assert!(
            matches!(next.request, ClientRequest::Focus { terminal_id, focused: false }
            if terminal_id == mock.session.id)
        );
        mock.response(next.request_id, ServerResponse::Accepted);
        mock.no_reconnect();
        mock.stop();
    }
}

#[test]
fn unavailable_refusals_require_the_exact_pre_effect_input_contract() {
    for (kind, message, retryable) in [
        ("input", "unknown partial input delivery", true),
        ("input", "the terminal worker has closed", false),
        ("input", "terminal output has completed", true),
        ("attach", "the terminal worker has closed", true),
        ("control", "the terminal worker has closed", true),
        ("unsolicited", "the terminal worker has closed", true),
        ("unknown", "the terminal worker has closed", true),
        ("wrong-proof", "the terminal worker has closed", true),
        ("wrong-version", "the terminal worker has closed", true),
    ] {
        let mut mock = MockHost::begin(false);
        let request_id = if kind == "attach" {
            Some(mock.attachment)
        } else {
            mock.attach();
            match kind {
                "input" | "wrong-proof" | "wrong-version" => {
                    mock.handle.input(b"never replay".to_vec()).unwrap();
                    Some(mock.request().request_id)
                }
                "control" => {
                    mock.handle.focus(true).unwrap();
                    Some(mock.request().request_id)
                }
                "unsolicited" => None,
                "unknown" => Some(u64::MAX),
                _ => unreachable!(),
            }
        };
        write_frame(
            &mut mock.stream,
            &ServerEnvelope {
                version: if kind == "wrong-version" {
                    PROTOCOL_VERSION - 1
                } else {
                    PROTOCOL_VERSION
                },
                request_id,
                proof: if kind == "wrong-proof" {
                    proof("other-incarnation")
                } else {
                    mock.proof.clone()
                },
                message: ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::TerminalUnavailable,
                    message,
                    retryable,
                )),
            },
        )
        .unwrap();
        if retryable {
            let _next = mock.reconnect();
        }
        if matches!(kind, "unknown" | "wrong-proof" | "wrong-version") {
            assert!(mock.handle.events().try_recv().is_err());
        } else {
            let first = mock.handle.events().recv_timeout(Duration::from_secs(2));
            assert!(
                matches!(first, Ok(TerminalEvent::Error { message: actual, .. }) if actual == message)
            );
        }
        assert!(
            mock.handle
                .events()
                .try_iter()
                .all(|event| !matches!(event, TerminalEvent::InputRejected { .. }))
        );
        mock.stop();
    }
}

#[test]
fn full_event_inbox_preserves_mixed_pre_effect_refusals_and_completion() {
    let mut mock = MockHost::begin(false);
    mock.attach();
    for sequence in 1..=EVENT_CAPACITY as u64 {
        mock.event(sequence, WireTerminalEvent::Bell);
    }
    mock.handle.input(b"queue refused".to_vec()).unwrap();
    let input = mock.request();
    mock.refuse(Some(input.request_id), ProtocolErrorCode::QueueFull, true);
    wait_for_rejection(&mock);
    mock.handle.input(b"closed refused".to_vec()).unwrap();
    let input = mock.request();
    mock.send(
        Some(input.request_id),
        ServerMessage::Error(ProtocolError::new(
            ProtocolErrorCode::TerminalUnavailable,
            "the terminal worker has closed",
            true,
        )),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if mock
            .handle
            .inner
            .published
            .input_rejection
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(_, _, reason)| *reason == InputRejectionReason::Coalesced)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "closed refusal must not block on the UI inbox"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let queued = mock.handle.events();
    assert_eq!(
        queued
            .try_iter()
            .filter(|event| matches!(event, TerminalEvent::Bell))
            .count(),
        EVENT_CAPACITY
    );
    assert!(matches!(mock.handle.events().try_recv().unwrap(),
        TerminalEvent::InputRejected { operation: "send terminal input", message }
        if message == "Terminal input was refused before it was sent."));
    mock.event(
        EVENT_CAPACITY as u64 + 1,
        WireTerminalEvent::Completed {
            exit_code: 19,
            signal: None,
            intentional: false,
        },
    );
    assert!(matches!(
        mock.handle
            .events()
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        TerminalEvent::Completed { exit_code: 19, .. }
    ));
    mock.no_reconnect();
    mock.stop();
}
