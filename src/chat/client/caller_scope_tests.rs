fn native_scope_peer(
    serve: impl FnOnce(&mut UnixStream, &mut FrameCodec, &HostSessionProof) + Send + 'static,
) -> (UnixStream, JoinHandle<()>) {
    let (client, mut peer) = UnixStream::pair().unwrap();
    let worker = thread::spawn(move || {
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let proof = host_proof("caller-scope");
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let hello: ClientEnvelope = codec.read(&mut peer).unwrap().unwrap();
        assert_eq!(hello.request_id, 1);
        assert!(matches!(hello.request, ClientRequest::Connect { .. }));
        native_scope_reply(
            &mut peer,
            &mut codec,
            &proof,
            1,
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        serve(&mut peer, &mut codec, &proof);
        assert!(
            codec.read::<ClientEnvelope>(&mut peer).unwrap().is_none(),
            "a refused binding must end the stream without business or User fallback"
        );
    });
    (client, worker)
}

fn native_scope_reply(
    peer: &mut UnixStream,
    codec: &mut FrameCodec,
    proof: &HostSessionProof,
    id: u64,
    message: ServerMessage,
) {
    codec
        .write(
            peer,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(id),
                proof: proof.clone(),
                message,
            },
        )
        .unwrap();
}

fn native_scope_denied() -> ServerMessage {
    ServerMessage::Error(ipc::ProtocolError::new(
        ipc::ProtocolErrorCode::PermissionDenied,
        "Synthetic caller scope refused",
        false,
    ))
}

#[test]
fn native_scope_refusal_precedes_start_restore_and_account_recovery() {
    for token in ["", "synthetic-invalid-capability"] {
        for route in 0..5 {
            let caller = CallerContext::for_test(
                crate::agent_execution::Authority::session(token.into()),
                false,
                false,
            );
            let expected = caller.authentication().unwrap();
            let (stream, worker) = native_scope_peer(move |peer, codec, proof| {
                let auth: ClientEnvelope = codec.read(peer).unwrap().unwrap();
                assert_eq!(auth.request_id, 2);
                assert_eq!(auth.proof.as_ref(), Some(proof));
                assert!(
                    matches!(auth.request, ClientRequest::Authenticate { authority } if authority == expected)
                );
                native_scope_reply(peer, codec, proof, 2, native_scope_denied());
            });
            let session = omp_card(Some(CONVERSATION), None);
            let intent = match route {
                1 => Intent::Restore,
                2 => Intent::RetryStart,
                _ => Intent::Start,
            };
            let recovery = match route {
                3 => Some((session.clone(), None)),
                4 => Some((session.clone(), Some(7))),
                _ => None,
            };
            let result = ClientHandle::connect_on(
                &caller,
                stream,
                session,
                intent,
                None,
                false,
                None,
                recovery,
                None,
                |_| panic!("no client worker may be published after binding refusal"),
            );
            assert!(
                matches!(result, Err(error) if error.message == "Synthetic caller scope refused")
            );
            worker.join().unwrap();
        }
    }
}

#[test]
fn native_scope_binding_preserves_user_sequence_and_advances_scoped_business_id() {
    for scoped in [false, true] {
        let authority = if scoped {
            crate::agent_execution::Authority::session("synthetic-session".into())
        } else {
            crate::agent_execution::Authority::default()
        };
        let caller = CallerContext::for_test(authority.clone(), false, false);
        let (stream, worker) = native_scope_peer(move |peer, codec, proof| {
            if scoped {
                let auth: ClientEnvelope = codec.read(peer).unwrap().unwrap();
                assert_eq!(auth.request_id, 2);
                assert_eq!(auth.proof.as_ref(), Some(proof));
                assert!(
                    matches!(auth.request, ClientRequest::Authenticate { authority: got } if got == authority)
                );
                native_scope_reply(
                    peer,
                    codec,
                    proof,
                    2,
                    ServerMessage::Response {
                        response: ServerResponse::Accepted,
                    },
                );
            }
            let business: ClientEnvelope = codec.read(peer).unwrap().unwrap();
            let id = if scoped { 3 } else { 2 };
            assert_eq!(business.request_id, id);
            assert_eq!(business.proof.as_ref(), Some(proof));
            assert!(matches!(business.request, ClientRequest::ChatStart { .. }));
            native_scope_reply(peer, codec, proof, id, native_scope_denied());
        });
        let result = ClientHandle::connect_on(
            &caller,
            stream,
            omp_card(Some(CONVERSATION), None),
            Intent::Start,
            None,
            false,
            None,
            None,
            None,
            |_| panic!("the synthetic business refusal publishes no client"),
        );
        assert!(result.is_err());
        worker.join().unwrap();
    }
}

#[test]
fn malformed_native_scope_acknowledgements_cannot_enter_restore_fallback() {
    for invalid in 0..5 {
        let caller = CallerContext::for_test(
            crate::agent_execution::Authority::session("synthetic-session".into()),
            false,
            false,
        );
        let (stream, worker) = native_scope_peer(move |peer, codec, proof| {
            let auth: ClientEnvelope = codec.read(peer).unwrap().unwrap();
            assert!(matches!(auth.request, ClientRequest::Authenticate { .. }));
            let mut reply = ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(2),
                proof: proof.clone(),
                message: ServerMessage::Response {
                    response: ServerResponse::Accepted,
                },
            };
            match invalid {
                0 => reply.request_id = Some(3),
                1 => reply.version = PROTOCOL_VERSION.saturating_sub(1),
                2 => reply.proof = host_proof("different-caller-host"),
                3 => reply.proof.host_incarnation = crate::model::HostIncarnation::new(""),
                4 => {
                    reply.message = ServerMessage::Response {
                        response: ServerResponse::ChatListed { sessions: vec![] },
                    }
                }
                _ => unreachable!(),
            }
            codec.write(peer, &reply).unwrap();
        });
        let result = ClientHandle::connect_on(
            &caller,
            stream,
            omp_card(Some(CONVERSATION), Some(host_proof("saved-before-upgrade"))),
            Intent::Restore,
            None,
            false,
            None,
            None,
            None,
            |_| panic!("a malformed scope reply cannot publish a client"),
        );
        assert!(result.is_err());
        worker.join().unwrap();
    }
}

#[derive(Clone, Copy)]
enum NativeScopeLookup {
    Preview,
    Candidates,
}

fn native_scope_preview() -> crate::chat::BindingPreview {
    crate::chat::BindingPreview {
        token: "synthetic-preview-only".into(),
        session_id: CONVERSATION.into(),
        title: "Synthetic caller preview".into(),
        working_directory: "/synthetic/caller-preview".into(),
    }
}

fn native_scope_candidates() -> Vec<crate::chat::BindingCandidate> {
    vec![crate::chat::BindingCandidate {
        session_id: CONVERSATION.into(),
        title: "Synthetic caller candidate".into(),
        working_directory: "/synthetic/caller-candidate".into(),
    }]
}

fn native_scope_lookup(
    route: NativeScopeLookup,
    caller: &CallerContext,
    stream: UnixStream,
    session: Session,
) -> Result<(), String> {
    let source = host_proof("caller-scope");
    match route {
        NativeScopeLookup::Preview => {
            let preview = ClientHandle::preview_on(caller, stream, session, source)?;
            (preview == native_scope_preview())
                .then_some(())
                .ok_or_else(|| "Synthetic preview payload differed".into())
        }
        NativeScopeLookup::Candidates => {
            let candidates = ClientHandle::candidates_on(caller, stream, session, source)?;
            (candidates == native_scope_candidates())
                .then_some(())
                .ok_or_else(|| "Synthetic candidate payload differed".into())
        }
    }
}

fn native_scope_lookup_peer(
    serve: impl FnOnce(&mut UnixStream, &mut FrameCodec, &HostSessionProof) + Send + 'static,
) -> (UnixStream, JoinHandle<()>) {
    let (client, mut peer) = UnixStream::pair().unwrap();
    let worker = thread::spawn(move || {
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let proof = host_proof("caller-scope");
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let hello: ClientEnvelope = codec.read(&mut peer).unwrap().unwrap();
        assert_eq!(hello.version, PROTOCOL_VERSION);
        assert_eq!(hello.request_id, 1);
        assert!(hello.proof.is_none());
        assert!(
            matches!(hello.request, ClientRequest::Connect { expected_host, resume }
            if expected_host.as_ref() == Some(&proof) && resume.is_empty())
        );
        native_scope_reply(
            &mut peer,
            &mut codec,
            &proof,
            1,
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        serve(&mut peer, &mut codec, &proof);
        assert!(
            codec.read::<ClientEnvelope>(&mut peer).unwrap().is_none(),
            "a read-only lookup must end without a fallback or another business request"
        );
    });
    (client, worker)
}

#[test]
fn native_scope_refusal_precedes_preview_and_candidates() {
    for route in [NativeScopeLookup::Preview, NativeScopeLookup::Candidates] {
        for token in ["", "synthetic-invalid-capability"] {
            let caller = CallerContext::for_test(
                crate::agent_execution::Authority::session(token.into()),
                false,
                false,
            );
            let expected = caller.authentication().unwrap();
            let (stream, worker) = native_scope_lookup_peer(move |peer, codec, proof| {
                let auth: ClientEnvelope = codec.read(peer).unwrap().unwrap();
                assert_eq!(auth.version, PROTOCOL_VERSION);
                assert_eq!(auth.request_id, 2);
                assert_eq!(auth.proof.as_ref(), Some(proof));
                assert!(
                    matches!(auth.request, ClientRequest::Authenticate { authority }
                    if authority == expected)
                );
                native_scope_reply(peer, codec, proof, 2, native_scope_denied());
            });
            let result =
                native_scope_lookup(route, &caller, stream, omp_card(Some(CONVERSATION), None));
            worker.join().unwrap();
            assert_eq!(result, Err("Synthetic caller scope refused".into()));
        }
    }
}

#[test]
fn native_scope_preview_and_candidates_preserve_user_and_scoped_sequences() {
    for route in [NativeScopeLookup::Preview, NativeScopeLookup::Candidates] {
        for scoped in [false, true] {
            let authority = if scoped {
                crate::agent_execution::Authority::session("synthetic-lookup-session".into())
            } else {
                crate::agent_execution::Authority::default()
            };
            let caller = CallerContext::for_test(authority.clone(), false, false);
            let session = omp_card(Some(CONVERSATION), None);
            let expected_session = session.clone();
            let (stream, worker) = native_scope_lookup_peer(move |peer, codec, proof| {
                if scoped {
                    let auth: ClientEnvelope = codec.read(peer).unwrap().unwrap();
                    assert_eq!(auth.version, PROTOCOL_VERSION);
                    assert_eq!(auth.request_id, 2);
                    assert_eq!(auth.proof.as_ref(), Some(proof));
                    assert!(
                        matches!(auth.request, ClientRequest::Authenticate { authority: got }
                        if got == authority)
                    );
                    native_scope_reply(
                        peer,
                        codec,
                        proof,
                        2,
                        ServerMessage::Response {
                            response: ServerResponse::Accepted,
                        },
                    );
                }
                let business: ClientEnvelope = codec.read(peer).unwrap().unwrap();
                let id = if scoped { 3 } else { 2 };
                assert_eq!(business.version, PROTOCOL_VERSION);
                assert_eq!(business.request_id, id);
                assert_eq!(business.proof.as_ref(), Some(proof));
                let (session, source, response) = match (route, business.request) {
                    (
                        NativeScopeLookup::Preview,
                        ClientRequest::ChatPreview { session, source },
                    ) => (
                        session,
                        source,
                        ServerResponse::ChatPreviewed {
                            preview: native_scope_preview(),
                        },
                    ),
                    (
                        NativeScopeLookup::Candidates,
                        ClientRequest::ChatCandidates { session, source },
                    ) => (
                        session,
                        source,
                        ServerResponse::ChatCandidates {
                            candidates: native_scope_candidates(),
                        },
                    ),
                    _ => panic!("read-only lookup used the wrong business request"),
                };
                assert_eq!(session, expected_session);
                assert_eq!(source, *proof);
                native_scope_reply(peer, codec, proof, id, ServerMessage::Response { response });
            });
            let result = native_scope_lookup(route, &caller, stream, session);
            worker.join().unwrap();
            assert_eq!(result, Ok(()));
        }
    }
}

#[test]
fn malformed_native_scope_acknowledgements_cannot_enter_preview_or_candidates() {
    for route in [NativeScopeLookup::Preview, NativeScopeLookup::Candidates] {
        for invalid in 0..5 {
            let caller = CallerContext::for_test(
                crate::agent_execution::Authority::session("synthetic-lookup-session".into()),
                false,
                false,
            );
            let expected = caller.authentication().unwrap();
            let (stream, worker) = native_scope_lookup_peer(move |peer, codec, proof| {
                let auth: ClientEnvelope = codec.read(peer).unwrap().unwrap();
                assert_eq!(auth.version, PROTOCOL_VERSION);
                assert_eq!(auth.request_id, 2);
                assert_eq!(auth.proof.as_ref(), Some(proof));
                assert!(
                    matches!(auth.request, ClientRequest::Authenticate { authority }
                    if authority == expected)
                );
                let mut reply = ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(2),
                    proof: proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Accepted,
                    },
                };
                match invalid {
                    0 => reply.request_id = Some(3),
                    1 => reply.version = PROTOCOL_VERSION.saturating_sub(1),
                    2 => reply.proof = host_proof("different-lookup-host"),
                    3 => reply.proof.host_incarnation = crate::model::HostIncarnation::new(""),
                    4 => {
                        reply.message = ServerMessage::Response {
                            response: ServerResponse::ChatListed { sessions: vec![] },
                        }
                    }
                    _ => unreachable!(),
                }
                codec.write(peer, &reply).unwrap();
            });
            let result =
                native_scope_lookup(route, &caller, stream, omp_card(Some(CONVERSATION), None));
            worker.join().unwrap();
            assert!(result.is_err());
        }
    }
}
