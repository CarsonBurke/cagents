use crate::terminal_ipc::{read_frame, write_frame};

#[test]
fn first_start_waits_for_same_socket_epoch_checkpoint_and_preserves_write_uncertainty() {
    for name in [
        crate::agent_execution::AUTHORITY_ENV,
        "CAGENTS_AGENT_TOKEN",
        "CAGENTS_SESSION_TOKEN",
    ] {
        assert!(
            std::env::var_os(name).is_none(),
            "User transport fixture cannot run under inherited authority"
        );
    }
    for permit in [false, true] {
        let original = host_proof("original-start-destination");
        let mut source = omp_card(None, None);
        source.launch.provider = crate::agent::Provider::Claude;
        source.launch.recipe.program = "claude".into();
        source.launch.recipe.arguments.clear();
        assert!(source.is_unstarted());
        let raw = source.clone();
        let operation = uuid::Uuid::new_v4();
        let (client, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (events, received) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            ClientHandle::fixture_guarded_start(client, source, operation, move |wake| {
                events.send(wake).unwrap();
            })
        });
        let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        assert!(matches!(
            request.request,
            ClientRequest::Connect {
                expected_host: None,
                ..
            }
        ));
        write_frame(
            &mut peer,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                proof: original.clone(),
                request_id: Some(request.request_id),
                message: ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: original.clone(),
                        resumed: Vec::new(),
                    },
                },
            },
        )
        .unwrap();
        let Wake::StartPrepared(actual_operation, actual_proof, response) =
            received.recv_timeout(Duration::from_secs(5)).unwrap()
        else {
            panic!("actual prewrite checkpoint required")
        };
        assert_eq!(actual_operation, operation);
        assert_eq!(actual_proof, original);
        // The owned socket has completed Connect, but cannot send Start while
        // its durable proof is unacknowledged.
        peer.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        assert!(read_frame::<ClientEnvelope>(&mut peer).is_err());
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        response.answer(if permit {
            Ok(())
        } else {
            Err("writer refused checkpoint".into())
        });
        if permit {
            let start: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(start.proof.as_ref(), Some(&original));
            assert!(
                matches!(&start.request, ClientRequest::ChatStart { session, source: None, terminate_source: false, .. } if session == &raw && session.proof.is_none())
            );
            write_frame(
                &mut peer,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    proof: original,
                    request_id: Some(start.request_id),
                    message: ServerMessage::Error(ipc::ProtocolError::new(
                        ipc::ProtocolErrorCode::PermissionDenied,
                        "Ready refused after possible start",
                        false,
                    )),
                },
            )
            .unwrap();
        }
        let failure = worker
            .join()
            .unwrap()
            .err()
            .expect("actual Ready error/refused checkpoint");
        assert_eq!(failure.no_start_write, !permit);
        assert!(read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none());
    }
    let mut source = omp_card(None, None);
    source.launch.provider = crate::agent::Provider::Claude;
    let failure = ClientHandle::guarded_start_on(
        source,
        uuid::Uuid::new_v4(),
        |_| {},
        || Err(StartFailure::retry("connect refused before write")),
    )
    .err()
    .unwrap();
    assert!(
        failure.no_start_write,
        "actual before-write connection failure is removable"
    );
}
