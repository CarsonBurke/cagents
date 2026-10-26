//! Actual private framing at the explicit caller boundary; no daemon/provider.
use super::*;
use crate::agent_execution::{AUTHORITY_ENV, Authority};
use std::{ffi::OsString, os::unix::ffi::OsStringExt};

const ROLE: &str = "CAGENTS_TEST_HOSTED_AUTHORITY_ROLE";
const TOKEN: &str = "fc927333-8e3f-4890-91b7-4a0c534c743f"; // inert fake peer only

fn reply(stream: &mut UnixStream, proof: &HostSessionProof, id: u64, response: ServerResponse) {
    write_frame(
        stream,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(id),
            proof: proof.clone(),
            message: ServerMessage::Response { response },
        },
    )
    .unwrap();
}
fn request(stream: &mut UnixStream, proof: &HostSessionProof) -> ClientEnvelope {
    let request: ClientEnvelope = read_frame(stream).unwrap().unwrap();
    assert_eq!(request.version, PROTOCOL_VERSION);
    assert_eq!(request.proof.as_ref(), Some(proof));
    request
}
fn descriptor(session: &TerminalSession) -> TerminalDescriptor {
    TerminalDescriptor {
        session: session.clone(),
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
    }
}
fn connect(listener: &UnixListener, proof: &HostSessionProof) -> UnixStream {
    let mut stream = accept_connection(listener);
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
    assert_eq!(connect.version, PROTOCOL_VERSION);
    assert_eq!(connect.proof, None);
    assert!(
        matches!(connect.request, ClientRequest::Connect { expected_host: Some(expected), resume } if expected == *proof && resume.is_empty())
    );
    reply(
        &mut stream,
        proof,
        connect.request_id,
        ServerResponse::Connected {
            proof: proof.clone(),
            resumed: Vec::new(),
        },
    );
    stream
}
fn authenticate(stream: &mut UnixStream, proof: &HostSessionProof, authority: &Authority) -> u64 {
    let auth = request(stream, proof);
    assert!(
        matches!(&auth.request, ClientRequest::Authenticate { authority: actual } if actual == authority)
    );
    auth.request_id
}
fn inventory(
    stream: &mut UnixStream,
    proof: &HostSessionProof,
    terminals: Vec<TerminalDescriptor>,
) {
    let list = request(stream, proof);
    assert!(matches!(list.request, ClientRequest::List));
    reply(
        stream,
        proof,
        list.request_id,
        ServerResponse::Listed { terminals },
    );
}
fn attach(
    stream: &mut UnixStream,
    proof: &HostSessionProof,
    session: &TerminalSession,
    spawned: bool,
    restored_epoch: Option<u64>,
) {
    let operation = request(stream, proof);
    assert!(if spawned {
        matches!(&operation.request, ClientRequest::Spawn { session: actual, .. } if actual == session)
    } else {
        matches!(operation.request, ClientRequest::Attach { terminal_id, .. } if terminal_id == session.id)
    });
    if restored_epoch.is_some() {
        // Match real subscription timing: the server has received Attach,
        // but its correlated descriptor response has not yet been queued.
        send_provisional(stream, proof, session);
    }
    reply(
        stream,
        proof,
        operation.request_id,
        if spawned {
            ServerResponse::Spawned {
                terminal: descriptor(session),
            }
        } else {
            ServerResponse::Attached {
                terminal: descriptor(session),
            }
        },
    );
    let visibility = request(stream, proof);
    assert!(
        matches!(visibility.request, ClientRequest::SetVisibility { terminal_id, .. } if terminal_id == session.id)
    );
    reply(
        stream,
        proof,
        visibility.request_id,
        ServerResponse::Accepted,
    );
    if let Some(expected) = restored_epoch {
        let acknowledgement = request(stream, proof);
        assert!(matches!(
            acknowledgement.request,
            ClientRequest::TerminalAcknowledge { terminal_id, epoch }
                if terminal_id == session.id && epoch == expected
        ));
        reply(
            stream,
            proof,
            acknowledgement.request_id,
            ServerResponse::Accepted,
        );
    }
}
fn provisional_messages(terminal_id: TerminalId) -> Vec<ServerMessage> {
    vec![
        ServerMessage::TerminalObserved {
            terminal_id,
            epoch: 7,
        },
        ServerMessage::Snapshot {
            terminal_id,
            snapshot: TerminalSnapshot::full(&RenderSnapshot {
                generation: 3,
                ..RenderSnapshot::empty()
            }),
        },
        ServerMessage::Event {
            terminal_id,
            sequence: 1,
            event: crate::terminal_ipc::WireTerminalEvent::Bell,
        },
        ServerMessage::Redraw {
            terminal_id,
            generation: 3,
        },
        ServerMessage::Synchronized {
            terminal_id,
            generation: 3,
            event_sequence: 1,
        },
    ]
}
fn send_provisional(stream: &mut UnixStream, proof: &HostSessionProof, session: &TerminalSession) {
    for message in provisional_messages(session.id) {
        write_frame(
            stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: None,
                proof: proof.clone(),
                message,
            },
        )
        .unwrap();
    }
}
fn await_stopped(handle: &HostedTerminalHandle) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !handle
        .inner
        .worker
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .is_finished()
    {
        assert!(
            Instant::now() < deadline,
            "permanent refusal did not stop the actual worker"
        );
        thread::sleep(Duration::from_millis(1));
    }
}
fn stop(handle: &HostedTerminalHandle) {
    handle.stop_for_test();
    let worker = handle.inner.worker.lock().unwrap().take().unwrap();
    let (sent, received) = crossbeam_channel::bounded(1);
    let join = thread::spawn(move || {
        sent.send(worker.join().is_ok()).unwrap();
    });
    assert!(received.recv_timeout(Duration::from_secs(2)).unwrap());
    join.join().unwrap();
}
fn begin(
    authority: Authority,
) -> (
    tempfile::TempDir,
    UnixListener,
    HostedTerminalHandle,
    HostSessionProof,
    TerminalSession,
) {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("host.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let proof = proof("explicit-caller");
    let session = session();
    let handle = HostedTerminalHandle::attach_or_spawn_at_with_authority(
        socket,
        session.clone(),
        TerminalSpawnOptions::default(),
        Some(proof.clone()),
        authority,
        |_| {},
    )
    .unwrap();
    (directory, listener, handle, proof, session)
}

#[test]
fn explicit_authority_precedes_spawn_input_paste_and_close_without_environment_fallback() {
    let selector = thread::current().name().unwrap().to_owned();
    let Some(role) = std::env::var_os(ROLE) else {
        for role in ["valid", "user", "empty", "invalid", "non-unicode"] {
            let private = tempfile::tempdir().unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            // Only these self-exec fake-peer children receive inert environments.
            // Never inspect, copy or strip the parent session's actual credential.
            command
                .env_clear()
                .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
                .env(ROLE, role)
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", private.path())
                .env("TMPDIR", private.path())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            match role {
                "valid" => {
                    command.env(AUTHORITY_ENV, TOKEN);
                }
                "empty" => {
                    command.env(AUTHORITY_ENV, "");
                }
                "invalid" => {
                    command.env(AUTHORITY_ENV, "not-a-credential");
                }
                "non-unicode" => {
                    command.env(AUTHORITY_ENV, OsString::from_vec(vec![0xff]));
                }
                "user" => {}
                _ => unreachable!(),
            }
            let mut child = command.spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("private authority fixture timed out");
                }
                thread::sleep(Duration::from_millis(5));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.stdout.len() <= 1024 * 1024 && output.stderr.len() <= 1024 * 1024);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{role}: {stdout}\n{stderr}");
            assert!(stdout.contains("1 passed; 0 failed;"));
            assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
        }
        return;
    };
    let authority = Authority::from_environment();
    let accepted = matches!(role.to_str().unwrap(), "valid" | "user");
    match role.to_str().unwrap() {
        "user" => assert_eq!(authority, Authority::default()),
        "non-unicode" | "empty" => assert_eq!(authority.token(), Some("")),
        "invalid" => assert_eq!(authority.token(), Some("not-a-credential")),
        "valid" => assert_eq!(authority.token(), Some(TOKEN)),
        _ => unreachable!(),
    }
    let (_directory, listener, handle, proof, session) = begin(authority.clone());
    handle.input(b"queued input".to_vec()).unwrap();
    handle
        .paste("queued paste", PasteOrigin::Clipboard)
        .unwrap();
    let mut stream = connect(&listener, &proof);
    assert!(handle.wait_until_attached(Duration::ZERO).is_err());
    let auth = authenticate(&mut stream, &proof, &authority);
    if !accepted {
        write_frame(
            &mut stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(auth),
                proof,
                message: ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::PermissionDenied,
                    "inert credential refused",
                    false,
                )),
            },
        )
        .unwrap();
        assert!(handle.wait_until_attached(Duration::from_secs(2)).is_err());
        await_stopped(&handle);
        assert_eq!(
            handle.input(b"no fallback".to_vec()),
            Err(TerminalSendError::Closed)
        );
        assert!(read_frame::<ClientEnvelope>(&mut stream).unwrap().is_none());
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        stop(&handle);
        return;
    }
    reply(&mut stream, &proof, auth, ServerResponse::Accepted);
    inventory(&mut stream, &proof, Vec::new());
    attach(&mut stream, &proof, &session, true, None);
    for paste in [false, true] {
        let operation = request(&mut stream, &proof);
        assert!(if paste {
            matches!(operation.request, ClientRequest::Paste { terminal_id, text, .. } if terminal_id == session.id && text == "queued paste")
        } else {
            matches!(operation.request, ClientRequest::Input { terminal_id, bytes } if terminal_id == session.id && bytes == b"queued input")
        });
        reply(
            &mut stream,
            &proof,
            operation.request_id,
            ServerResponse::Accepted,
        );
    }
    handle.wait_until_attached(Duration::from_secs(2)).unwrap();
    let close_handle = handle.clone();
    let closing = thread::spawn(move || close_handle.shutdown());
    let close = request(&mut stream, &proof);
    assert!(
        matches!(close.request, ClientRequest::ShutdownTerminal { terminal_id } if terminal_id == session.id)
    );
    reply(
        &mut stream,
        &proof,
        close.request_id,
        ServerResponse::Accepted,
    );
    closing.join().unwrap().unwrap();
    stop(&handle);
}

#[test]
fn explicit_authority_reauthenticates_on_reconnect_without_replaying_started_input() {
    // A lost Spawn acknowledgement must not bless its provisional broadcasts
    // or spawn again. The next authenticated inventory selects the same actor.
    {
        let authority = Authority::session(TOKEN.into());
        let (_directory, listener, handle, proof, session) = begin(authority.clone());
        handle
            .inner
            .published
            .output_observation
            .lock()
            .unwrap()
            .epoch = 7;
        handle.input(b"queued until exact source".to_vec()).unwrap();
        let mut first = connect(&listener, &proof);
        let auth = authenticate(&mut first, &proof, &authority);
        reply(&mut first, &proof, auth, ServerResponse::Accepted);
        inventory(&mut first, &proof, Vec::new());
        let spawn = request(&mut first, &proof);
        assert!(
            matches!(spawn.request, ClientRequest::Spawn { session: actual, .. } if actual == session)
        );
        send_provisional(&mut first, &proof, &session);
        drop(first); // Spawn may have happened; no descriptor was acknowledged.
        let mut second = connect(&listener, &proof);
        assert!(handle.wait_until_attached(Duration::ZERO).is_err());
        assert_eq!(handle.snapshot().generation, 0);
        assert_eq!(
            handle
                .inner
                .published
                .event_sequence
                .load(Ordering::Acquire),
            0
        );
        assert!(!handle.take_observed_output());
        assert!(
            !handle
                .events()
                .try_iter()
                .any(|event| matches!(event, TerminalEvent::Bell))
        );
        let auth = authenticate(&mut second, &proof, &authority);
        reply(&mut second, &proof, auth, ServerResponse::Accepted);
        inventory(&mut second, &proof, vec![descriptor(&session)]);
        attach(&mut second, &proof, &session, false, Some(7));
        let input = request(&mut second, &proof);
        assert!(
            matches!(input.request, ClientRequest::Input { terminal_id, bytes }
            if terminal_id == session.id && bytes == b"queued until exact source")
        );
        // Receiving input is a barrier: every provisional frame was drained
        // after the exact descriptor, before any queued user command.
        assert_eq!(handle.snapshot().generation, 3);
        assert_eq!(
            handle
                .inner
                .published
                .event_sequence
                .load(Ordering::Acquire),
            1
        );
        assert!(handle.take_observed_output());
        assert_eq!(
            handle
                .events()
                .try_iter()
                .filter(|event| matches!(event, TerminalEvent::Bell))
                .count(),
            1
        );
        reply(
            &mut second,
            &proof,
            input.request_id,
            ServerResponse::Accepted,
        );
        stop(&handle);
    }
    let authority = Authority::session(TOKEN.into());
    let (_directory, listener, handle, proof, session) = begin(authority.clone());
    let mut first = connect(&listener, &proof);
    let auth = authenticate(&mut first, &proof, &authority);
    reply(&mut first, &proof, auth, ServerResponse::Accepted);
    inventory(&mut first, &proof, Vec::new());
    attach(&mut first, &proof, &session, true, None);
    handle.wait_until_attached(Duration::from_secs(2)).unwrap();
    handle.input(b"already started input".to_vec()).unwrap();
    let input = request(&mut first, &proof);
    assert!(
        matches!(input.request, ClientRequest::Input { bytes, .. } if bytes == b"already started input")
    );
    drop(first); // No acknowledgement: started input must not be replayed.
    let mut second = connect(&listener, &proof);
    assert!(handle.wait_until_attached(Duration::ZERO).is_err());
    let auth = authenticate(&mut second, &proof, &authority);
    reply(&mut second, &proof, auth, ServerResponse::Accepted);
    inventory(&mut second, &proof, vec![descriptor(&session)]);
    attach(&mut second, &proof, &session, false, None);
    handle.wait_until_attached(Duration::from_secs(2)).unwrap();
    handle.input(b"fresh input".to_vec()).unwrap();
    let fresh = request(&mut second, &proof);
    assert!(matches!(fresh.request, ClientRequest::Input { bytes, .. } if bytes == b"fresh input"));
    reply(
        &mut second,
        &proof,
        fresh.request_id,
        ServerResponse::Accepted,
    );
    drop(second);
    let mut third = connect(&listener, &proof);
    let auth = authenticate(&mut third, &proof, &authority);
    write_frame(
        &mut third,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(auth),
            proof,
            message: ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                "authority revoked",
                false,
            )),
        },
    )
    .unwrap();
    assert!(handle.wait_until_attached(Duration::from_secs(2)).is_err());
    await_stopped(&handle);
    assert_eq!(
        handle.paste("no User retry", PasteOrigin::Clipboard),
        Err(TerminalSendError::Closed)
    );
    assert!(read_frame::<ClientEnvelope>(&mut third).unwrap().is_none());
    stop(&handle);
    assert!(matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock));
}

#[test]
fn explicit_authority_rejects_changed_source_and_uncorrelated_or_stale_proof_before_restore() {
    let frame = ServerEnvelope {
        version: PROTOCOL_VERSION,
        request_id: None,
        proof: proof("capacity-only"),
        message: ServerMessage::TerminalObserved {
            terminal_id: session().id,
            epoch: 7,
        },
    };
    let exact_bytes = serde_json::to_vec(&frame).unwrap().len();
    assert_eq!(
        provisional_frame_bytes(&frame, exact_bytes),
        Some(exact_bytes)
    );
    assert_eq!(provisional_frame_bytes(&frame, exact_bytes - 1), None);
    {
        // A real framed overflow must reconnect without accepting provisional
        // publication or advancing either replay cursor. Tiny frames exercise
        // the count limit without constructing a large snapshot allocation.
        let authority = Authority::session(TOKEN.into());
        let (_directory, listener, handle, proof, session) = begin(authority.clone());
        handle
            .inner
            .published
            .output_observation
            .lock()
            .unwrap()
            .epoch = 7;
        handle
            .input(b"queued across capacity retry".to_vec())
            .unwrap();
        let mut first = connect(&listener, &proof);
        let auth = authenticate(&mut first, &proof, &authority);
        reply(&mut first, &proof, auth, ServerResponse::Accepted);
        inventory(&mut first, &proof, Vec::new());
        let spawn = request(&mut first, &proof);
        assert!(matches!(spawn.request,
            ClientRequest::Spawn { session: actual, .. } if actual == session));
        for index in 0..=EVENT_CAPACITY {
            let message = match index {
                0 => ServerMessage::Snapshot {
                    terminal_id: session.id,
                    snapshot: TerminalSnapshot::full(&RenderSnapshot {
                        generation: 3,
                        ..RenderSnapshot::empty()
                    }),
                },
                1 => ServerMessage::Event {
                    terminal_id: session.id,
                    sequence: 1,
                    event: crate::terminal_ipc::WireTerminalEvent::Bell,
                },
                _ => ServerMessage::TerminalObserved {
                    terminal_id: session.id,
                    epoch: 7,
                },
            };
            write_frame(
                &mut first,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: None,
                    proof: proof.clone(),
                    message,
                },
            )
            .unwrap();
        }
        assert!(
            read_frame::<ClientEnvelope>(&mut first).unwrap().is_none(),
            "overflow must close without presentation, output ACK, snapshot or input requests"
        );
        drop(first);
        // This is the actual worker's bounded retry, not a new test handle.
        // connect checks the original host proof and an empty resume list.
        let mut second = connect(&listener, &proof);
        assert!(handle.wait_until_attached(Duration::ZERO).is_err());
        assert_eq!(handle.snapshot().generation, 0);
        assert_eq!(
            handle
                .inner
                .published
                .event_sequence
                .load(Ordering::Acquire),
            0
        );
        assert!(!handle.take_observed_output());
        assert!(
            !handle
                .events()
                .try_iter()
                .any(|event| matches!(event, TerminalEvent::Bell))
        );
        let auth = authenticate(&mut second, &proof, &authority);
        reply(&mut second, &proof, auth, ServerResponse::Accepted);
        inventory(&mut second, &proof, vec![descriptor(&session)]);
        let attach = request(&mut second, &proof);
        assert!(matches!(attach.request, ClientRequest::Attach {
            terminal_id, after_generation: 0, after_event_sequence: 0
        } if terminal_id == session.id));
        reply(
            &mut second,
            &proof,
            attach.request_id,
            ServerResponse::Attached {
                terminal: descriptor(&session),
            },
        );
        let visibility = request(&mut second, &proof);
        assert!(matches!(visibility.request,
            ClientRequest::SetVisibility { terminal_id, .. } if terminal_id == session.id));
        reply(
            &mut second,
            &proof,
            visibility.request_id,
            ServerResponse::Accepted,
        );
        let acknowledgement = request(&mut second, &proof);
        assert!(matches!(acknowledgement.request,
            ClientRequest::TerminalAcknowledge { terminal_id, epoch: 7 }
            if terminal_id == session.id));
        reply(
            &mut second,
            &proof,
            acknowledgement.request_id,
            ServerResponse::Accepted,
        );
        let input = request(&mut second, &proof);
        assert!(
            matches!(input.request, ClientRequest::Input { terminal_id, bytes }
            if terminal_id == session.id && bytes == b"queued across capacity retry")
        );
        // Consuming the queued input is a barrier after restoration: none of
        // the discarded connection's provisional frames may leak through.
        assert_eq!(handle.snapshot().generation, 0);
        assert_eq!(
            handle
                .inner
                .published
                .event_sequence
                .load(Ordering::Acquire),
            0
        );
        assert!(!handle.take_observed_output());
        assert!(
            !handle
                .events()
                .try_iter()
                .any(|event| matches!(event, TerminalEvent::Bell))
        );
        reply(
            &mut second,
            &proof,
            input.request_id,
            ServerResponse::Accepted,
        );
        stop(&handle);
    }
    for case in [
        "source",
        "proof",
        "request-id",
        "unexpected",
        "list-proof",
        "list-request-id",
        "attached-source",
        "spawned-source",
    ] {
        let authority = Authority::session(TOKEN.into());
        let (_directory, listener, handle, proof, session) = begin(authority.clone());
        let mut stream = connect(&listener, &proof);
        let auth = authenticate(&mut stream, &proof, &authority);
        if case.starts_with("list-") {
            reply(&mut stream, &proof, auth, ServerResponse::Accepted);
            let list = request(&mut stream, &proof);
            assert!(matches!(list.request, ClientRequest::List));
            let mut response = ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(list.request_id),
                proof: proof.clone(),
                message: ServerMessage::Response {
                    response: ServerResponse::Listed {
                        terminals: Vec::new(),
                    },
                },
            };
            if case == "list-proof" {
                response.proof = super::proof("stale-list");
            } else {
                response.request_id = Some(list.request_id + 1);
            }
            write_frame(&mut stream, &response).unwrap();
        } else if matches!(case, "attached-source" | "spawned-source") {
            handle
                .inner
                .published
                .output_observation
                .lock()
                .unwrap()
                .epoch = 7;
            handle
                .input(b"must wait for exact source".to_vec())
                .unwrap();
            reply(&mut stream, &proof, auth, ServerResponse::Accepted);
            let spawned = case == "spawned-source";
            inventory(
                &mut stream,
                &proof,
                if spawned {
                    Vec::new()
                } else {
                    vec![descriptor(&session)]
                },
            );
            let operation = request(&mut stream, &proof);
            assert!(if spawned {
                matches!(operation.request, ClientRequest::Spawn { .. })
            } else {
                matches!(operation.request, ClientRequest::Attach { .. })
            });
            // Genuine broadcasts may precede a descriptor. Even their output
            // acknowledgement and redraw requests must wait for its identity.
            send_provisional(&mut stream, &proof, &session);
            let mut changed = descriptor(&session);
            changed
                .session
                .launch
                .arguments
                .push("changed-after-list".into());
            reply(
                &mut stream,
                &proof,
                operation.request_id,
                if spawned {
                    ServerResponse::Spawned { terminal: changed }
                } else {
                    ServerResponse::Attached { terminal: changed }
                },
            );
        } else if case == "source" {
            reply(&mut stream, &proof, auth, ServerResponse::Accepted);
            let mut changed = descriptor(&session);
            changed
                .session
                .launch
                .arguments
                .push("different-original".into());
            inventory(&mut stream, &proof, vec![changed]);
        } else {
            let mut response = ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(auth),
                proof: proof.clone(),
                message: ServerMessage::Response {
                    response: ServerResponse::Accepted,
                },
            };
            match case {
                "proof" => response.proof = super::proof("stale-host"),
                "request-id" => response.request_id = Some(auth + 1),
                "unexpected" => {
                    response.message = ServerMessage::Response {
                        response: ServerResponse::Listed {
                            terminals: Vec::new(),
                        },
                    }
                }
                _ => unreachable!(),
            }
            write_frame(&mut stream, &response).unwrap();
        }
        assert!(handle.wait_until_attached(Duration::from_secs(2)).is_err());
        assert!(
            read_frame::<ClientEnvelope>(&mut stream).unwrap().is_none(),
            "no visibility, acknowledgement, snapshot or queued input may precede a valid descriptor"
        );
        await_stopped(&handle);
        if matches!(case, "attached-source" | "spawned-source") {
            assert_eq!(handle.snapshot().generation, 0);
            assert_eq!(
                handle
                    .inner
                    .published
                    .event_sequence
                    .load(Ordering::Acquire),
                0
            );
            assert!(!handle.take_observed_output());
            assert!(
                !handle
                    .events()
                    .try_iter()
                    .any(|event| matches!(event, TerminalEvent::Bell))
            );
        }
        stop(&handle);
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }
}
