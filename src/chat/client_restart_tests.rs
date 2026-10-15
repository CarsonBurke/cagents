// Framed matched-protocol client contracts. The actual Host owns metadata
// issuance separately; these tests start no provider, daemon or native UI.
#[derive(Clone, Copy)]
enum RestartCase {
    Exact,
    FirstThread,
    Upgrade,
    Source,
    Snapshot,
    MissingBinding,
    NilActor,
    WrongActor,
    WrongPair,
    NilBackend,
    WrongRequest,
    ForeignProof,
    Start,
    ArchiveSource,
    ArchiveReplacement,
}
impl RestartCase {
    fn accepted(self) -> bool {
        matches!(self, Self::Exact | Self::FirstThread | Self::Upgrade)
    }
}

fn restarted_client_case(case: RestartCase) {
    let proof = handoff_proof();
    let source = omp_card(
        if matches!(case, RestartCase::FirstThread) {
            None
        } else {
            Some("confirmed")
        },
        Some(proof.clone()),
    );
    let mut saved = source.clone();
    if matches!(case, RestartCase::Upgrade) {
        saved.proof = Some(host_proof("previous-daemon"));
        saved.launch.session_id = Some("previous-thread".into());
        assert!(crate::chat::is_upgrade_recovery(&saved, &source));
    }
    let mut replacement = source.clone();
    replacement.launch.session_id = Some("confirmed".into());
    let recipe = crate::model::LaunchRecipe {
        program: "/matched/cagents".into(),
        arguments: vec![
            "agent".into(),
            "console".into(),
            crate::model::AgentId::new().to_string(),
        ],
        working_directory: source.launch.recipe.working_directory.clone(),
        environment: BTreeMap::new(),
        unset_environment: Default::default(),
    };
    let mut pair = ipc::TerminalDescriptor::relaunchable(source.id, recipe, None).session;
    pair.continuity = crate::model::TerminalContinuity::Live {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    replacement.terminal = Some(pair);
    let receipt =
        crate::chat::RestartReceipt::confirm(source.clone(), replacement.clone()).unwrap();
    let instance = uuid::Uuid::new_v4();
    let binding = ipc::ChatSourceBinding {
        source: replacement.clone(),
        execution_instance: instance,
        backend: uuid::Uuid::new_v4(),
    };
    assert!(valid_restart_attachment(
        &receipt,
        &source,
        &Snapshot {
            session_id: replacement.launch.session_id.clone(),
            ..Default::default()
        },
        instance,
        Some(&binding),
        &proof
    ));

    let (client, mut server) = UnixStream::pair().unwrap();
    let socket = server.try_clone().unwrap();
    let (release, released) = crossbeam_channel::bounded(1);
    let (done, completed) = crossbeam_channel::bounded(1);
    let (wake, wakes) = crossbeam_channel::bounded(16);
    let sent_source = source.clone();
    let sent_saved = saved.clone();
    let sent_proof = proof.clone();
    let sent_receipt = receipt.clone();
    let sent_binding = binding.clone();
    let worker = thread::spawn(move || {
        struct Done {
            sender: crossbeam_channel::Sender<bool>,
            success: bool,
        }
        impl Drop for Done {
            fn drop(&mut self) {
                let _ = self.sender.try_send(self.success);
            }
        }
        let mut finished = Done {
            sender: done,
            success: false,
        };
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(
            matches!(&connect.request, ClientRequest::Connect { resume, expected_host }
            if resume.is_empty() && expected_host.as_ref() == (!matches!(case, RestartCase::Upgrade)).then_some(&sent_proof))
        );
        handoff_reply(
            &mut server,
            &mut codec,
            Some(connect.request_id),
            &sent_proof,
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: sent_proof.clone(),
                    resumed: vec![],
                },
            },
        );
        if matches!(case, RestartCase::Upgrade) {
            let resolve: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(
                matches!(resolve.request, ClientRequest::ResolveUpgradeChat {source} if source==sent_saved)
            );
            handoff_reply(
                &mut server,
                &mut codec,
                Some(resolve.request_id),
                &sent_proof,
                ServerMessage::Response {
                    response: ServerResponse::UpgradeChatResolved {
                        session: sent_source.clone(),
                    },
                },
            );
        }
        let attach: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert_eq!(attach.proof.as_ref(), Some(&sent_proof));
        if matches!(case, RestartCase::Start) {
            assert!(
                matches!(&attach.request, ClientRequest::ChatStart {session, ..} if session==&sent_source)
            );
        } else {
            assert!(
                matches!(&attach.request, ClientRequest::ChatAttach {session} if session==&sent_source)
            );
        }
        let mut receipt_json = serde_json::to_value(&sent_receipt).unwrap();
        match case {
            RestartCase::Source => {
                receipt_json["source"]["launch"]["recipe"]["program"] = "different".into();
                receipt_json["replacement"]["launch"]["recipe"]["program"] = "different".into();
            }
            RestartCase::ArchiveSource | RestartCase::ArchiveReplacement => {
                let report = crate::chat::ArchiveReportReference {
                    format: 1,
                    registry: uuid::Uuid::new_v4(),
                    agent: crate::model::AgentId::new(),
                    source_operation: uuid::Uuid::new_v4(),
                };
                let side = if matches!(case, RestartCase::ArchiveSource) {
                    "source"
                } else {
                    "replacement"
                };
                receipt_json[side]["archive_report"] = serde_json::to_value(report).unwrap();
            }
            _ => {}
        }
        let wire_receipt: crate::chat::RestartReceipt =
            serde_json::from_value(receipt_json).unwrap();
        let mut wire_binding = sent_binding;
        wire_binding.source = wire_receipt.replacement().clone();
        match case {
            RestartCase::WrongActor => wire_binding.execution_instance = uuid::Uuid::new_v4(),
            RestartCase::WrongPair => {
                wire_binding
                    .source
                    .terminal
                    .as_mut()
                    .unwrap()
                    .transcript_path = Some("/different-pair.log".into())
            }
            RestartCase::NilBackend => wire_binding.backend = uuid::Uuid::nil(),
            _ => {}
        }
        let request = if matches!(case, RestartCase::WrongRequest) {
            attach.request_id + 1
        } else {
            attach.request_id
        };
        let mut envelope_proof = sent_proof.clone();
        if matches!(case, RestartCase::ForeignProof) {
            envelope_proof.host_incarnation = crate::model::HostIncarnation::new("foreign");
        }
        handoff_reply(
            &mut server,
            &mut codec,
            Some(request),
            &envelope_proof,
            ServerMessage::Response {
                response: ServerResponse::ChatRestarted {
                    receipt: wire_receipt,
                    snapshot: Snapshot {
                        generation: 1,
                        session_id: Some(
                            (if matches!(case, RestartCase::Snapshot) {
                                "different"
                            } else {
                                "confirmed"
                            })
                            .into(),
                        ),
                        ..Default::default()
                    },
                    execution_instance: if matches!(case, RestartCase::NilActor) {
                        uuid::Uuid::nil()
                    } else {
                        instance
                    },
                    binding: (!matches!(case, RestartCase::MissingBinding)).then_some(wire_binding),
                },
            },
        );
        if case.accepted() {
            // A real request after the handshake proves a usable original wire,
            // rather than only testing a pure receipt predicate/callback.
            loop {
                let command: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                let compact = matches!(command.request, ClientRequest::ChatCommand {chat_id, command:Command::Compact} if chat_id==sent_source.id);
                handoff_reply(
                    &mut server,
                    &mut codec,
                    Some(command.request_id),
                    &sent_proof,
                    ServerMessage::Response {
                        response: ServerResponse::Accepted,
                    },
                );
                if compact {
                    break;
                }
            }
            let mut later = binding_for_later_restart_test(&sent_source, instance);
            later.source.terminal = sent_receipt.replacement().terminal.clone();
            handoff_reply(
                &mut server,
                &mut codec,
                None,
                &sent_proof,
                ServerMessage::Response {
                    response: ServerResponse::ChatSnapshot {
                        chat_id: sent_source.id,
                        execution_instance: instance,
                        binding: Some(later),
                        snapshot: Some(Snapshot {
                            generation: 2,
                            session_id: Some("later-thread".into()),
                            ..Default::default()
                        }),
                    },
                },
            );
            released.recv_timeout(Duration::from_secs(5)).unwrap();
        } else {
            let next: Option<ClientEnvelope> = codec.read(&mut server).unwrap();
            assert!(
                next.is_none(),
                "invalid restart metadata must not publish an Inner or admit later controls"
            );
        }
        finished.success = true;
    });
    // Armed before connect_on, including invalid/early-return assertion paths.
    let mut peer = ReaderPeer {
        socket,
        release,
        done: completed,
        settled: false,
        workers: vec![worker],
    };
    let result = ClientHandle::connect_on(
        client,
        saved.clone(),
        if matches!(case, RestartCase::Start) {
            Intent::Start
        } else {
            Intent::Restore
        },
        (!matches!(case, RestartCase::Upgrade)).then_some(proof.clone()),
        false,
        None,
        None,
        move |event| {
            let _ = wake.try_send(event);
        },
    );
    if case.accepted() {
        let handle = result.unwrap();
        assert_eq!(handle.restart_receipt(), Some(&receipt));
        assert_eq!(handle.session(), replacement);
        assert_eq!(handle.inner.execution_instance, instance);
        assert_eq!(handle.source_binding(), Some(binding));
        if matches!(case, RestartCase::Upgrade) {
            assert_eq!(handle.upgrade_recovery(), Some(&(saved, source.clone())));
            assert_ne!(
                receipt.replacement(),
                &source,
                "new pair is a separate restart, never rewritten into upgrade provenance"
            );
        } else {
            assert!(handle.upgrade_recovery().is_none());
        }
        handle.set_visible(true).unwrap();
        handle.send(Command::Compact).unwrap();
        while handle.snapshot().generation != 2 {
            wakes
                .recv_timeout(Duration::from_secs(5))
                .expect("actual later framed state update");
        }
        assert_eq!(
            handle.session().launch.session_id.as_deref(),
            Some("later-thread")
        );
        assert_eq!(
            handle.restart_receipt(),
            Some(&receipt),
            "later live metadata never rewrites captured restart evidence"
        );
        assert_eq!(
            handle
                .source_binding()
                .unwrap()
                .source
                .launch
                .session_id
                .as_deref(),
            Some("later-thread")
        );
        peer.release.try_send(()).unwrap();
        assert!(peer.done.recv_timeout(Duration::from_secs(6)).unwrap());
        peer.settled = true;
        drop(handle);
    } else {
        assert!(
            result.is_err(),
            "malformed or uncorrelated restart metadata must refuse before publishing any handle"
        );
        assert!(
            wakes.try_recv().is_err(),
            "initial payload cannot wake a not-admitted surface"
        );
        assert!(peer.done.recv_timeout(Duration::from_secs(6)).unwrap());
        peer.settled = true;
    }
}

fn binding_for_later_restart_test(
    source: &Session,
    instance: uuid::Uuid,
) -> ipc::ChatSourceBinding {
    let mut source = source.clone();
    source.launch.session_id = Some("later-thread".into());
    ipc::ChatSourceBinding {
        source,
        execution_instance: instance,
        backend: uuid::Uuid::new_v4(),
    }
}

#[test]
fn restart_handshake_retains_exact_receipt_actor_and_usable_wire_across_live_updates() {
    restarted_client_case(RestartCase::Exact);
    restarted_client_case(RestartCase::FirstThread);
}
#[test]
fn restart_handshake_keeps_upgrade_and_paired_restart_as_distinct_immutable_receipts() {
    restarted_client_case(RestartCase::Upgrade);
}
#[test]
fn restart_handshake_refuses_uncorrelated_or_incoherent_authority_before_handle_publication() {
    for case in [
        RestartCase::Source,
        RestartCase::Snapshot,
        RestartCase::MissingBinding,
        RestartCase::NilActor,
        RestartCase::WrongActor,
        RestartCase::WrongPair,
        RestartCase::NilBackend,
        RestartCase::WrongRequest,
        RestartCase::ForeignProof,
        RestartCase::Start,
    ] {
        restarted_client_case(case);
    }
}
#[test]
fn restart_handshake_never_makes_archived_reports_executable() {
    restarted_client_case(RestartCase::ArchiveSource);
    restarted_client_case(RestartCase::ArchiveReplacement);
}
