use super::fixture::{host_proof, restore_with_lineage};
use super::*;

#[test]
fn active_model_sidechannel_survives_snapshots_patches_and_hidden_publications() {
    let publications = Publications::new(Snapshot {
        generation: 4,
        ..Snapshot::default()
    });
    let body = AtomicBool::new(true);
    let model = super::super::ActiveModel {
        id: "actual".into(),
        label: "Actual model".into(),
        reasoning: Some("high".into()),
        default_reasoning: Some("medium".into()),
        aliases: Vec::new(),
    };
    assert!(publications.active_model(2, Some(model.clone()), &body));
    assert!(
        !publications.active_model(1, None, &body),
        "late attach status cannot rewind a live update"
    );
    assert_eq!(
        publications.replace(
            Snapshot {
                generation: 5,
                ..Snapshot::default()
            },
            &body,
            false,
            false
        ),
        Some(true)
    );
    let base = publications.current.load_full();
    assert_eq!(base.snapshot.active_model.as_ref(), Some(&model));
    assert_eq!(base.snapshot.active_model_revision, 2);
    let patch = Publication::next(
        &base,
        Snapshot {
            generation: 6,
            ..Snapshot::default()
        },
        true,
        append::Lineage::default(),
    );
    assert_eq!(patch.snapshot.active_model.as_ref(), Some(&model));
    body.store(false, Ordering::Release);
    assert!(publications.active_model(3, None, &body));
    assert!(!publications.current.load().resident);
    assert!(publications.current.load().snapshot.active_model.is_none());
    assert!(publications.active_model(4, Some(model.clone()), &body));
    assert_eq!(
        publications.current.load().snapshot.active_model.as_ref(),
        Some(&model)
    );
    assert_eq!(
        publications.replace(
            Snapshot {
                generation: 7,
                ..Snapshot::default()
            },
            &body,
            false,
            false
        ),
        Some(false)
    );
    assert_eq!(
        publications.current.load().snapshot.active_model.as_ref(),
        Some(&model)
    );
}

/// The daemon tells a surface every address it knows, as it learns one
/// and once on attaching, in whichever order those arrive.
#[test]
fn agent_addresses_accumulate_in_any_order_they_arrive() {
    let address = |address: &str, subagent: &str| super::super::AgentAddress {
        address: address.into(),
        subagent: subagent.into(),
    };
    let mut known = AgentAddresses::default();
    let shared = known.clone();
    assert!(learn_addresses(
        &mut known,
        vec![
            address("a1892b93e88999a7d", "toolu_relocate"),
            address("a77f00d1e2b3c4d5a", "toolu_review"),
        ]
    ));
    // The copy a frame holds is not changed under it.
    assert!(shared.is_empty());
    let learned = known.clone();
    // An older list, sent on attaching after a newer one, adds nothing,
    // and an address keeps the agent it first named.
    assert!(!learn_addresses(
        &mut known,
        vec![address("a1892b93e88999a7d", "toolu_other")]
    ));
    assert!(Arc::ptr_eq(&known, &learned), "nothing new is not a change");
    assert_eq!(known["a1892b93e88999a7d"], "toolu_relocate");
    assert_eq!(known.len(), 2);
}

static REPLACEMENT_GATES: Mutex<
    BTreeMap<usize, (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
> = Mutex::new(BTreeMap::new());

pub(super) fn pause_before_replacement(owner: usize) {
    let gate = REPLACEMENT_GATES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&owner);
    if let Some((paused, resume)) = gate {
        paused.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(10)).unwrap();
    }
}

static PUBLICATION_GATES: Mutex<
    BTreeMap<TerminalId, (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
> = Mutex::new(BTreeMap::new());
pub(super) fn pause_before_publication(chat: TerminalId) {
    let gate = PUBLICATION_GATES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&chat);
    if let Some((paused, resume)) = gate {
        paused.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(10)).unwrap();
    }
}

#[test]
fn admission_capacity_is_typed_and_keeps_daemon_detail() {
    for (code, recovery) in [
        (
            ipc::ProtocolErrorCode::ChatCapacity,
            super::super::Recovery::Capacity,
        ),
        (
            ipc::ProtocolErrorCode::AccountRemoved,
            super::super::Recovery::Account,
        ),
        (
            ipc::ProtocolErrorCode::TerminalUnavailable,
            super::super::Recovery::Retry,
        ),
    ] {
        let failure = start_failure(ipc::ProtocolError::new(
            code,
            "128 resident; limit 128",
            true,
        ));
        assert_eq!(failure.recovery, recovery);
        assert_eq!(failure.message, "128 resident; limit 128");
    }
    // Text alone must never turn a provider failure into capacity recovery.
    assert_eq!(
        start_failure(ipc::ProtocolError::new(
            ipc::ProtocolErrorCode::TerminalUnavailable,
            "Daemon native chat capacity reached",
            true,
        ))
        .recovery,
        super::super::Recovery::Retry,
    );
}

#[test]
fn start_recovery_uses_retryability_without_losing_typed_recovery() {
    use super::super::Recovery;
    use ipc::ProtocolErrorCode;

    for retryable in [false, true] {
        for (code, recovery) in [
            (ProtocolErrorCode::AccountRemoved, Recovery::Account),
            (ProtocolErrorCode::ChatCapacity, Recovery::Capacity),
            (
                ProtocolErrorCode::InvalidRequest,
                if retryable {
                    Recovery::Retry
                } else {
                    Recovery::Blocked
                },
            ),
            (
                ProtocolErrorCode::TerminalUnavailable,
                if retryable {
                    Recovery::Retry
                } else {
                    Recovery::Blocked
                },
            ),
            (
                ProtocolErrorCode::Internal,
                if retryable {
                    Recovery::Retry
                } else {
                    Recovery::Blocked
                },
            ),
        ] {
            let failure = start_failure(ipc::ProtocolError::new(
                code,
                "the daemon's exact reason",
                retryable,
            ));
            assert_eq!(
                failure.recovery, recovery,
                "{code:?}, retryable={retryable}"
            );
            assert_eq!(failure.message, "the daemon's exact reason");
        }
    }
}

/// The daemon's busy refusal of an exclusive start, such as an account
/// switch, is typed apart from other retryable failures, because it alone
/// proves nothing was admitted. Only its own whole retryable sentence.
#[test]
fn only_the_daemons_busy_refusal_is_typed_busy() {
    use super::super::Recovery;
    use ipc::ProtocolErrorCode;

    let recovery = |message: &str, retryable| {
        start_failure(ipc::ProtocolError::new(
            ProtocolErrorCode::Internal,
            message,
            retryable,
        ))
        .recovery
    };
    // As `host::control_gate::busy` refuses; its own tests pin that.
    let busy = start_failure(ipc::ProtocolError::new(
        ProtocolErrorCode::Internal,
        ipc::DAEMON_BUSY,
        true,
    ));
    assert_eq!(busy.recovery, Recovery::Busy);
    assert_eq!(busy.message, ipc::DAEMON_BUSY);
    assert!(!Recovery::Busy.is_blocked());
    assert_eq!(recovery(ipc::DAEMON_BUSY, false), Recovery::Blocked);
    assert_eq!(
        recovery(&format!("Provider said: {}", ipc::DAEMON_BUSY), true),
        Recovery::Retry
    );
}

/// A daemon too busy to finish a switch's handshake refuses it as busy;
/// one that can't be reached at all does not claim to be busy, and an
/// update in progress is.
#[test]
fn a_switch_whose_handshake_timed_out_is_unanswered_not_busy() {
    use super::super::Recovery;
    use crate::terminal::HostedTerminalStartError;

    let timed_out = switch_unadmitted(HostedTerminalStartError::HostProtocol(
        crate::control::ControlError::ReplyTimeout {
            timeout: Duration::from_secs(30),
            error: ipc::FrameError::Io(io::ErrorKind::WouldBlock.into()),
        },
    ));
    // Nothing says another operation holds the daemon; it may be hung.
    assert_eq!(timed_out.recovery, Recovery::Unresponsive);
    assert!(!Recovery::Unresponsive.is_blocked());
    assert!(timed_out.message.contains("did not answer within 30 s"));
    for unreachable in [
        HostedTerminalStartError::HostProtocol(crate::control::ControlError::ConnectionClosed),
        HostedTerminalStartError::HostSocket {
            path: "/run/cagents.sock".into(),
            source: io::ErrorKind::ConnectionRefused.into(),
        },
        HostedTerminalStartError::SocketPath(io::ErrorKind::NotFound.into()),
        HostedTerminalStartError::HostIncompatible { actual: 1 },
    ] {
        // Nothing was asked of a daemon, so nothing marks the account
        // removed.
        assert_eq!(switch_unadmitted(unreachable).recovery, Recovery::Unreached);
    }
    assert!(!Recovery::Unreached.is_blocked());
    assert_eq!(
        switch_unadmitted(HostedTerminalStartError::UpdateInProgress(
            crate::instance::UpdateInProgress
        ))
        .recovery,
        Recovery::Busy
    );
}

#[test]
fn only_the_daemons_whole_unproven_owner_refusal_waits_for_a_restart() {
    use super::super::{Recovery, UNPROVEN_OWNER_REFUSAL};
    use ipc::ProtocolErrorCode;

    let recovery = |message: &str, retryable| {
        start_failure(ipc::ProtocolError::new(
            ProtocolErrorCode::InvalidRequest,
            message,
            retryable,
        ))
        .recovery
    };
    assert_eq!(
        recovery(UNPROVEN_OWNER_REFUSAL, false),
        Recovery::AfterRestart
    );
    assert!(Recovery::AfterRestart.is_blocked());
    // A daemon still running from before the sentence was reworded.
    assert_eq!(
        recovery(super::EARLIER_UNPROVEN_OWNER_REFUSAL, false),
        Recovery::AfterRestart
    );
    // A provider or wrapper quoting the sentence names no restart.
    assert_eq!(
        recovery(&format!("Provider said: {UNPROVEN_OWNER_REFUSAL}"), false),
        Recovery::Blocked
    );
    // Retryability still wins: a retry is a nearer way back.
    assert_eq!(recovery(UNPROVEN_OWNER_REFUSAL, true), Recovery::Retry);
}

#[test]
fn source_clock_advances_in_daemon_time_despite_delayed_hydration() {
    let received = Instant::now();
    // The daemon can be arbitrarily ahead of or behind the display
    // machine. Neither its epoch nor receipt time comes from UI refresh.
    for sampled_at in [1_000, u64::MAX - 60_000] {
        let clock = SourceClock::from_sample(Some(sampled_at), received).unwrap();
        assert_eq!(clock.at(received), sampled_at);
        assert_eq!(
            clock.at(received + Duration::from_secs(12)),
            sampled_at + 12_000
        );
    }
    assert!(SourceClock::from_sample(None, received).is_none());
}

#[test]
fn source_clock_never_rewinds_when_new_publications_arrive() {
    let received = Instant::now();
    let mut clock = SourceClock::from_sample(Some(10_000), received);
    SourceClock::observe(&mut clock, Some(9_000), received + Duration::from_secs(2));
    assert_eq!(clock.unwrap().at(received + Duration::from_secs(3)), 13_000);
    SourceClock::observe(&mut clock, Some(14_000), received + Duration::from_secs(3));
    assert_eq!(clock.unwrap().at(received + Duration::from_secs(4)), 15_000);
}

fn handoff_terminal() -> crate::model::TerminalSession {
    use crate::model::{LaunchRecipe, TerminalContinuity, TerminalSession};
    let recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "unused".into(),
        arguments: Vec::new(),
        working_directory: "/".into(),
        environment: BTreeMap::new(),
    };
    TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
        resume_hint: None,
        transcript_path: None,
    }
}

fn handoff_proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("session"),
        boot_id: crate::model::BootId::new("boot"),
        host_incarnation: crate::model::HostIncarnation::new("incarnation"),
    }
}

fn handoff_reply(
    server: &mut UnixStream,
    codec: &mut FrameCodec,
    request_id: Option<u64>,
    proof: &HostSessionProof,
    message: ServerMessage,
) {
    codec
        .write(
            server,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id,
                proof: proof.clone(),
                message,
            },
        )
        .unwrap();
}

fn handoff_handshake(server: &mut UnixStream, codec: &mut FrameCodec) {
    server
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let connect: ClientEnvelope = codec.read(server).unwrap().unwrap();
    assert!(matches!(
        connect.request,
        ClientRequest::Connect {
            expected_host: None,
            ..
        }
    ));
    let proof = handoff_proof();
    handoff_reply(
        server,
        codec,
        Some(connect.request_id),
        &proof,
        ServerMessage::Response {
            response: ServerResponse::Connected {
                proof: proof.clone(),
                resumed: Vec::new(),
            },
        },
    );
}

#[test]
fn handoff_batch_continues_after_a_refusal_and_an_incomplete_saved_proof() {
    use crate::terminal_ipc::{ProtocolError, ProtocolErrorCode};
    let mut terminals = vec![
        handoff_terminal(),
        handoff_terminal(),
        handoff_terminal(),
        handoff_terminal(),
    ];
    terminals[1].continuity = crate::model::TerminalContinuity::DetachedSameBoot {
        host_session_id: crate::model::HostSessionId::new(""),
        boot_id: handoff_proof().boot_id,
        host_incarnation: handoff_proof().host_incarnation,
    };
    let expected = [terminals[0].id, terminals[2].id, terminals[3].id];
    let (client, mut server) = UnixStream::pair().unwrap();
    let worker = thread::spawn(move || {
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        handoff_handshake(&mut server, &mut codec);
        for (index, terminal_id) in expected.into_iter().enumerate() {
            let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert_eq!(request.request_id, index as u64 + 2);
            assert!(
                matches!(request.request, ClientRequest::ChatCommitTerminal { terminal_id: actual } if actual == terminal_id)
            );
            assert_eq!(request.proof, Some(handoff_proof()));
            let message = if index == 0 {
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    "receipt refused",
                    false,
                ))
            } else {
                ServerMessage::Response {
                    response: ServerResponse::Accepted,
                }
            };
            handoff_reply(
                &mut server,
                &mut codec,
                Some(request.request_id),
                &handoff_proof(),
                message,
            );
        }
        assert!(codec.read::<ClientEnvelope>(&mut server).unwrap().is_none());
    });
    let results = acknowledge_terminal_handoffs_on(
        crate::control::ControlClient::from_stream(client),
        &terminals,
    );
    worker.join().unwrap();
    assert!(results[0].as_ref().unwrap_err().contains("receipt refused"));
    assert!(
        results[1]
            .as_ref()
            .unwrap_err()
            .contains("incomplete daemon proof")
    );
    assert_eq!(&results[2..], &[Ok(()), Ok(())]);
}

#[test]
fn handoff_batch_preserves_success_but_stops_using_a_failed_connection() {
    use crate::terminal_ipc::{ProtocolError, ProtocolErrorCode};
    // EOF, proof changes, uncorrelated drop notices, and mismatched request
    // IDs or unexpected response types must not cause any more requests
    // on a compromised connection.
    for failure in 0..5 {
        let mut terminals = vec![
            handoff_terminal(),
            handoff_terminal(),
            handoff_terminal(),
            handoff_terminal(),
        ];
        terminals[3].continuity = crate::model::TerminalContinuity::DetachedSameBoot {
            host_session_id: handoff_proof().host_session_id,
            boot_id: handoff_proof().boot_id,
            host_incarnation: crate::model::HostIncarnation::new("previous"),
        };
        let (client, mut server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            handoff_handshake(&mut server, &mut codec);
            let first: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            handoff_reply(
                &mut server,
                &mut codec,
                Some(first.request_id),
                &handoff_proof(),
                ServerMessage::Response {
                    response: ServerResponse::Accepted,
                },
            );
            let second: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            if failure == 0 {
                return;
            }
            let mut proof = handoff_proof();
            if failure == 1 {
                proof.host_incarnation = crate::model::HostIncarnation::new("changed");
            }
            let (id, message) = if failure == 2 {
                (
                    None,
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::QueueFull,
                        "dropping connection",
                        true,
                    )),
                )
            } else {
                (
                    Some(second.request_id + u64::from(failure == 3)),
                    ServerMessage::Response {
                        response: if failure == 4 {
                            ServerResponse::Listed {
                                terminals: Vec::new(),
                            }
                        } else {
                            ServerResponse::Accepted
                        },
                    },
                )
            };
            handoff_reply(&mut server, &mut codec, id, &proof, message);
            assert!(codec.read::<ClientEnvelope>(&mut server).unwrap().is_none());
        });
        let results = acknowledge_terminal_handoffs_on(
            crate::control::ControlClient::from_stream(client),
            &terminals,
        );
        worker.join().unwrap();
        assert_eq!(results[0], Ok(()));
        assert!(results[1].is_err());
        assert_eq!(results[1], results[2]);
        assert_eq!(results[3], Ok(()));
    }
}

#[test]
fn only_missing_or_refused_socket_connections_retire_the_whole_handoff_batch() {
    use crate::control::{ControlClient, ControlError};
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("daemon.sock");
    let terminals = [handoff_terminal(), handoff_terminal()];
    assert_eq!(
        acknowledge_terminal_handoffs_on(ControlClient::connect(&socket), &terminals),
        vec![Ok(()), Ok(())]
    );
    // Inject refusal explicitly: parallel subprocesses can briefly inherit a
    // listener before exec, so dropping it does not guarantee immediate refusal.
    assert_eq!(
        acknowledge_terminal_handoffs_on(
            Err(ControlError::Connect(io::Error::from(
                io::ErrorKind::ConnectionRefused
            ))),
            &terminals,
        ),
        vec![Ok(()), Ok(())]
    );
    for error in [
        ControlError::Connect(io::Error::from(io::ErrorKind::PermissionDenied)),
        ControlError::Connect(io::Error::from(io::ErrorKind::ConnectionReset)),
        ControlError::Configure(io::Error::from(io::ErrorKind::NotFound)),
        ControlError::ProofMismatch,
        ControlError::ConnectionClosed,
    ] {
        assert!(
            acknowledge_terminal_handoffs_on(Err(error), &terminals)
                .iter()
                .all(Result::is_err)
        );
    }
}

/// The conversation the live installation's card was showing: a UUIDv7,
/// which every identifier gate on the restore path has to accept.
const CONVERSATION: &str = "01a0ab66-21d6-7065-9fe5-95f302b669e1";

#[test]
fn exited_terminal_commit_discovers_host_but_pins_the_acknowledgement() {
    use crate::model::{
        BootId, HostIncarnation, HostSessionId, LaunchRecipe, TerminalContinuity, TerminalSession,
    };
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("session"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    let recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "unused".into(),
        arguments: Vec::new(),
        working_directory: "/".into(),
        environment: BTreeMap::new(),
    };
    let terminal = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
        resume_hint: None,
        transcript_path: None,
    };
    // A persisted descriptor with no live proof still commits on the
    // discovered host, as one naming that host does. An incarnation change
    // after discovery must fail.
    for (live, changed) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut terminal = terminal.clone();
        if live {
            terminal.continuity = TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            };
        }
        let (client, mut server) = UnixStream::pair().unwrap();
        let proof = proof.clone();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            codec
                .write(
                    &mut server,
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
            let commit: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                commit.request,
                ClientRequest::ChatCommitTerminal { .. }
            ));
            let response_proof = if changed {
                HostSessionProof {
                    host_incarnation: HostIncarnation::new("replacement"),
                    ..proof
                }
            } else {
                proof
            };
            codec
                .write(
                    &mut server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(commit.request_id),
                        proof: response_proof,
                        message: ServerMessage::Response {
                            response: ServerResponse::Accepted,
                        },
                    },
                )
                .unwrap();
        });
        let result = acknowledge_terminal_handoffs_on(
            crate::control::ControlClient::from_stream(client),
            &[terminal],
        );
        worker.join().unwrap();
        assert_eq!(result[0].is_err(), changed);
    }
}

#[test]
fn a_replacement_daemon_retires_the_previous_daemons_handoff() {
    use crate::model::{
        BootId, HostIncarnation, HostSessionId, LaunchRecipe, TerminalContinuity, TerminalSession,
    };
    let previous = HostSessionProof {
        host_session_id: HostSessionId::new("session"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("previous"),
    };
    let replacement = HostSessionProof {
        host_incarnation: HostIncarnation::new("replacement"),
        ..previous.clone()
    };
    // A closed card saved before its daemon was replaced still carries the
    // previous daemon's identity; its receipt went away with that daemon.
    let terminal = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: LaunchRecipe {
            unset_environment: Default::default(),
            program: "unused".into(),
            arguments: Vec::new(),
            working_directory: "/".into(),
            environment: BTreeMap::new(),
        },
        continuity: TerminalContinuity::DetachedSameBoot {
            host_session_id: previous.host_session_id.clone(),
            boot_id: previous.boot_id.clone(),
            host_incarnation: previous.host_incarnation.clone(),
        },
        resume_hint: None,
        transcript_path: None,
    };
    let (client, mut server) = UnixStream::pair().unwrap();
    let worker = thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        // Pinning the previous identity would only draw a refusal.
        assert!(matches!(
            connect.request,
            ClientRequest::Connect {
                expected_host: None,
                ..
            }
        ));
        codec
            .write(
                &mut server,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(connect.request_id),
                    proof: replacement.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: replacement,
                            resumed: Vec::new(),
                        },
                    },
                },
            )
            .unwrap();
        // Nothing more is asked of a daemon that never held the receipt.
        codec.read::<ClientEnvelope>(&mut server).unwrap()
    });
    assert_eq!(
        acknowledge_terminal_handoffs_on(
            crate::control::ControlClient::from_stream(client),
            &[terminal],
        ),
        vec![Ok(())]
    );
    assert!(worker.join().unwrap().is_none());
}

#[test]
fn selecting_then_leaving_fences_queued_wakes_and_old_wire_observations() {
    let mut observed = OutputObservation::default();
    let (wake, queued) = crossbeam_channel::unbounded();
    // Hidden output queues a wake, but selection acknowledges before it runs.
    assert!(observed.observe(0));
    wake.send(Wake::Observed).unwrap();
    let epoch = observed.begin_acknowledgement();
    assert_eq!(epoch, 1);
    // The worktree is hidden again when the pre-selection wake executes.
    assert!(matches!(queued.recv().unwrap(), Wake::Observed));
    assert!(
        !observed.take(),
        "selection must consume the old unread edge"
    );
    // An already-written daemon frame can arrive after the queued wake.
    assert!(!observed.observe(0));
    assert!(
        !observed.take(),
        "old wire output must not resurrect unread"
    );
    assert!(observed.observe(epoch));
    assert!(
        !observed.observe(epoch),
        "fresh output coalesces until drained"
    );
    assert!(!observed.observe(0));
    assert!(
        observed.take(),
        "rejecting old output must preserve fresh output"
    );
    assert!(!observed.take());
    // A stale queued wake may consume fresh output; its matching wake must
    // then be harmless, rather than produce a second unread transition.
    assert!(observed.observe(epoch));
    wake.send(Wake::Observed).unwrap();
    let next_epoch = observed.begin_acknowledgement();
    assert!(next_epoch > epoch);
    assert!(observed.observe(next_epoch));
    wake.send(Wake::Observed).unwrap();
    assert!(matches!(queued.recv().unwrap(), Wake::Observed));
    assert!(observed.take());
    assert!(matches!(queued.recv().unwrap(), Wake::Observed));
    assert!(!observed.take());
}

#[test]
fn activity_wake_coalesces_until_consumed_and_fences_stale_attach_edges() {
    let mut activity = ActivityObservation {
        generation: 5,
        state: ActivityState::Working,
        pending: false,
    };
    assert!(!activity.observe(4, ActivityState::Idle));
    assert_eq!(activity.take(), ActivityState::Working);
    assert!(activity.observe(6, ActivityState::Waiting));
    assert!(!activity.observe(7, ActivityState::Idle));
    assert_eq!(activity.take(), ActivityState::Idle);
    assert!(!activity.observe(8, ActivityState::Idle));
    assert!(activity.observe(9, ActivityState::Working));
    assert_eq!(activity.take(), ActivityState::Working);
}

#[test]
fn pane_first_send_keeps_its_basis_and_candidate_while_publication_is_pending() {
    use ipc::{
        ClientEnvelope, ProtocolError, ProtocolErrorCode, ServerEnvelope, ServerMessage,
        ServerResponse, read_frame, write_frame,
    };
    let source = omp_card(None, None);
    let mut candidate = source.clone();
    candidate.id = TerminalId::new();
    let pane = ipc::PaneChatStart {
        registry: crate::workspace_model::NO_REGISTRY,
        card: crate::model::CardId::new(),
        source,
        allow_local: false,
    };
    let expected_pane = pane.clone();
    let expected_candidate = candidate.clone();
    let (stream, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let server = thread::spawn(move || {
        let proof = host_proof("pane-start");
        let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        write_frame(
            &mut peer,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(hello.request_id),
                proof: proof.clone(),
                message: ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                },
            },
        )
        .unwrap();
        for (code, retryable, message) in [
            (ProtocolErrorCode::QueueFull, true, "Pane not yet published"),
            (
                ProtocolErrorCode::InvalidRequest,
                false,
                "Pane claimed elsewhere",
            ),
        ] {
            let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert!(
                matches!(request.request, ClientRequest::ChatStartInPane { pane, session, .. }
                if pane == expected_pane && session == expected_candidate)
            );
            write_frame(
                &mut peer,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Error(ProtocolError::new(code, message, retryable)),
                },
            )
            .unwrap();
        }
        assert!(
            read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
            "a definitive lost claim must not be retried"
        );
    });
    let failure = ClientHandle::connect_on_with_pane(
        stream,
        candidate,
        Intent::Start,
        None,
        false,
        None,
        None,
        None,
        Some(pane),
        |_| {},
    )
    .err()
    .expect("the fixture refuses admission");
    assert_eq!(failure.message, "Pane claimed elsewhere");
    server.join().unwrap();
}

/// The live installation's own card: a native OMP conversation whose
/// persisted launch names its session directory and nothing else.
fn omp_card(conversation: Option<&str>, proof: Option<HostSessionProof>) -> Session {
    Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: TerminalId::new(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Omp,
            recipe: crate::model::LaunchRecipe {
                unset_environment: Default::default(),
                program: "omp".into(),
                arguments: vec![
                    "--session-dir".into(),
                    "/home/user/.omp/agent/sessions/native".into(),
                ],
                working_directory: "/".into(),
                environment: BTreeMap::new(),
            },
            session_id: conversation.map(str::to_owned),
        },
        proof,
        terminal: None,
    }
}

/// Runs the restore negotiation against a daemon that answers the upgrade
/// lineage question with `answer`, and returns what it was asked.
fn negotiate(
    live: &HostSessionProof,
    session: Session,
    answer: ServerMessage,
) -> (Result<ClientRequest, StartFailure>, ClientRequest) {
    let (outcome, request, _) = negotiate_with_recovery(live, session, answer);
    (outcome, request)
}

#[test]
fn proofless_restore_attaches_the_exact_hosted_writer() {
    let live = host_proof("live");
    for durable in [false, true] {
        for named in [false, true] {
            let mut saved = omp_card(named.then_some(CONVERSATION), None);
            if durable {
                saved.native_continuity = super::super::NativeContinuity::Durable {
                    provider_version: "test".into(),
                };
            }
            let mut hosted = saved.clone();
            hosted.proof = Some(live.clone());
            hosted.launch.session_id = Some(CONVERSATION.into());
            let (outcome, asked) = negotiate(
                &live,
                saved,
                ServerMessage::Response {
                    response: ServerResponse::ChatListed {
                        sessions: vec![hosted.clone()],
                    },
                },
            );
            assert!(matches!(asked, ClientRequest::ChatList));
            assert!(
                matches!(outcome, Ok(ClientRequest::ChatAttach { session }) if session == hosted)
            );
        }
    }
}

#[test]
fn a_lost_codex_resume_reply_attaches_its_allocated_listener() {
    let live = host_proof("live");
    let mut saved = omp_card(Some(CONVERSATION), None);
    saved.launch.provider = crate::agent::Provider::Codex;
    saved.launch.recipe.program = "codex".into();
    saved.launch.recipe.arguments = ["resume", CONVERSATION, "--remote", "unix://"]
        .map(String::from)
        .into();
    let mut hosted = saved.clone();
    assert!(hosted.claim_owned_codex_listener_for_unserved_thread(
        std::path::Path::new("/run/test-native"),
        &live,
    ));
    hosted.proof = Some(live.clone());
    for conflict in 0..5 {
        let mut advertised = hosted.clone();
        match conflict {
            0 => {}
            1 => advertised.launch.session_id = Some("other-thread".into()),
            2 => advertised.launch.recipe.arguments.push("--search".into()),
            3 => advertised.launch.recipe.working_directory = "/other".into(),
            4 => advertised.proof = Some(host_proof("stale")),
            _ => unreachable!(),
        }
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let proof = live.clone();
        let worker = thread::spawn(move || {
            let envelope: ClientEnvelope = ipc::read_frame(&mut server).unwrap().unwrap();
            assert!(matches!(envelope.request, ClientRequest::ChatList));
            ipc::write_frame(
                &mut server,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(envelope.request_id),
                    proof,
                    message: ServerMessage::Response {
                        response: ServerResponse::ChatListed {
                            sessions: vec![advertised],
                        },
                    },
                },
            )
            .unwrap();
        });
        let result = restore(
            &mut client,
            &mut FrameCodec::new(ipc::MAX_FRAME_BYTES),
            &mut 1,
            &live,
            saved.clone(),
            &mut None,
            &mut None,
            Intent::RetryStart,
        );
        worker.join().unwrap();
        if conflict == 0 {
            assert!(
                matches!(result, Ok(ClientRequest::ChatAttach { session }) if session == hosted)
            );
        } else {
            assert!(
                result.is_err(),
                "a changed identity must not attach or restart: {conflict}"
            );
        }
    }
}

#[test]
fn proofless_restore_refuses_a_conflicting_hosted_identity() {
    let live = host_proof("live");
    let saved = omp_card(Some(CONVERSATION), None);
    let mut hosted = saved.clone();
    hosted.proof = Some(live.clone());
    for conflict in 0..4 {
        let mut other = hosted.clone();
        match conflict {
            0 => other.proof = Some(host_proof("stale")),
            1 => other.launch.provider = crate::agent::Provider::Claude,
            2 => other.launch.recipe.working_directory = "/other".into(),
            _ => other.launch.session_id = Some("other-conversation".into()),
        }
        let (outcome, _) = negotiate(
            &live,
            saved.clone(),
            ServerMessage::Response {
                response: ServerResponse::ChatListed {
                    sessions: vec![other],
                },
            },
        );
        assert!(
            outcome.is_err(),
            "identity conflict must not start or attach"
        );
    }
}

#[test]
fn proofless_restore_without_a_hosted_writer_preserves_restart_policy() {
    let live = host_proof("live");
    for durable in [false, true] {
        let mut saved = omp_card(Some(CONVERSATION), None);
        if durable {
            saved.native_continuity = super::super::NativeContinuity::Durable {
                provider_version: "test".into(),
            };
        }
        let (outcome, _) = negotiate(
            &live,
            saved,
            ServerMessage::Response {
                response: ServerResponse::ChatListed { sessions: vec![] },
            },
        );
        if durable {
            assert!(outcome.is_err(), "uncertain durable work must not restart");
        } else {
            assert!(matches!(outcome, Ok(ClientRequest::ChatStart { .. })));
        }
    }
}

#[test]
fn failed_saved_claude_restore_sends_no_start_request() {
    let (stream, mut peer) = UnixStream::pair().unwrap();
    let live = host_proof("current");
    let mut source = omp_card(Some(CONVERSATION), Some(host_proof("old")));
    source.launch.provider = crate::agent::Provider::Claude;
    source.launch.recipe.program = "must-not-run".into();
    source.launch.recipe.arguments.clear();
    let expected = source.clone();
    let worker = thread::spawn(move || {
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        for step in 0..2 {
            let request: ClientEnvelope = ipc::read_frame(&mut peer).unwrap().unwrap();
            let message = match (step, request.request) {
                (0, ClientRequest::Connect { .. }) => ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: live.clone(),
                        resumed: Vec::new(),
                    },
                },
                (1, ClientRequest::ResolveUpgradeChat { source }) if source == expected => {
                    ServerMessage::Error(ipc::ProtocolError::new(
                        ipc::ProtocolErrorCode::TerminalUnavailable,
                        "Exact history is unavailable",
                        false,
                    ))
                }
                other => panic!("Unexpected saved history request: {other:?}"),
            };
            ipc::write_frame(
                &mut peer,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: live.clone(),
                    message,
                },
            )
            .unwrap();
        }
        let extra: Option<ClientEnvelope> = ipc::read_frame(&mut peer).unwrap();
        assert!(extra.is_none(), "a failed restore must not send ChatStart");
    });
    let result = ClientHandle::connect_on(
        stream,
        source,
        Intent::Restore,
        None,
        false,
        None,
        None,
        None,
        |_| {},
    );
    assert!(result.is_err());
    worker.join().unwrap();
}

/// A retained Claude card whose helper no daemon hosts asks the daemon for
/// a proved successor, and the answer decides what the card does.
fn resume_stranded(
    source: Session,
    answer: impl FnOnce(&Session) -> ServerMessage + Send + 'static,
) -> (Result<ClientRequest, StartFailure>, Option<Session>, usize) {
    let live = host_proof("current");
    let (mut client, mut server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let server_proof = live.clone();
    let expected = source.clone();
    let worker = thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let mut answer = Some(answer);
        let mut asked = 0;
        while let Ok(Some(request)) = codec.read::<ClientEnvelope>(&mut server) {
            asked += 1;
            let message = match request.request {
                ClientRequest::ChatList => ServerMessage::Response {
                    response: ServerResponse::ChatListed { sessions: vec![] },
                },
                ClientRequest::ResolveUpgradeChat { source } if source == expected => {
                    ServerMessage::Error(ipc::ProtocolError::new(
                        ipc::ProtocolErrorCode::TerminalUnavailable,
                        "The durable native holder is unavailable",
                        false,
                    ))
                }
                ClientRequest::ChatPrepareResume { source } if source == expected => {
                    (answer.take().unwrap())(&source)
                }
                other => panic!("unexpected stranded-card request: {other:?}"),
            };
            codec
                .write(
                    &mut server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request.request_id),
                        proof: server_proof.clone(),
                        message,
                    },
                )
                .unwrap();
        }
        asked
    });
    let mut resumed = None;
    let result = restore(
        &mut client,
        &mut FrameCodec::new(ipc::MAX_FRAME_BYTES),
        &mut 2,
        &live,
        source,
        &mut None,
        &mut resumed,
        Intent::Restore,
    );
    drop(client);
    (result, resumed, worker.join().unwrap())
}

fn stranded_claude(session_id: Option<&str>) -> Session {
    let mut source = omp_card(session_id, Some(host_proof("previous")));
    source.launch.provider = crate::agent::Provider::Claude;
    source.launch.recipe.program = "claude".into();
    source.launch.recipe.arguments = vec!["--model".into(), "haiku".into()];
    source.native_continuity = super::super::NativeContinuity::RetainedClaudeActor { protocol: 2 };
    assert!(source.has_valid_native_continuity());
    source
}

#[test]
fn stranded_retained_claude_card_starts_the_daemon_prepared_successor() {
    let source = stranded_claude(Some("6f1f6f1e-4a4b-4c4d-8e8f-909192939495"));
    let (result, resumed, asked) = resume_stranded(source.clone(), |source| {
        let mut successor = source.clone();
        successor.id = TerminalId::new();
        successor.proof = None;
        successor.native_continuity = Default::default();
        ServerMessage::Response {
            response: ServerResponse::ChatResumePrepared { session: successor },
        }
    });
    let Ok(ClientRequest::ChatStart {
        session,
        source: None,
        terminate_source: false,
        ..
    }) = result
    else {
        panic!("a proved successor must be started: {result:?}");
    };
    assert_eq!(asked, 2, "upgrade lineage, then resume preparation");
    assert_ne!(session.id, source.id);
    assert_eq!(session.launch, source.launch);
    assert!(session.proof.is_none());
    assert!(super::super::is_retained_resume_successor(
        &source, &session
    ));
    assert_eq!(resumed.as_ref(), Some(&source));
    // The daemon's start proves the successor; only that form commits.
    let mut started = session.clone();
    assert!(!super::super::is_retained_resume_commit(&source, &started));
    started.proof = Some(host_proof("current"));
    started.native_continuity = super::super::NativeContinuity::RetainedClaudeActor { protocol: 2 };
    assert!(super::super::is_retained_resume_commit(&source, &started));
    started.launch.recipe.arguments.push("--other".into());
    assert!(!super::super::is_retained_resume_commit(&source, &started));
}

#[test]
fn retained_resume_preparation_honors_typed_refusals() {
    for (retryable, expected) in [
        (true, super::super::Recovery::Retry),
        (false, super::super::Recovery::Blocked),
    ] {
        let (result, resumed, _) = resume_stranded(
            stranded_claude(Some("6f1f6f1e-4a4b-4c4d-8e8f-909192939495")),
            move |_| {
                ServerMessage::Error(ipc::ProtocolError::new(
                    ipc::ProtocolErrorCode::TerminalUnavailable,
                    "Exact preparation refusal",
                    retryable,
                ))
            },
        );
        let failure = result.unwrap_err();
        assert_eq!(failure.recovery, expected);
        assert!(failure.message.contains("Exact preparation refusal"));
        assert!(resumed.is_none());
    }
}

#[test]
fn stranded_retained_claude_card_never_replaces_a_live_or_unproved_owner() {
    let source = stranded_claude(Some("6f1f6f1e-4a4b-4c4d-8e8f-909192939495"));
    // A still-present owner is returned unchanged: attach, never replace.
    let (result, resumed, _) = resume_stranded(source.clone(), |source| ServerMessage::Response {
        response: ServerResponse::ChatResumePrepared {
            session: source.clone(),
        },
    });
    let failure = result.unwrap_err();
    assert_eq!(failure.recovery, crate::chat::Recovery::Retry);
    assert!(failure.message.contains("still holds this conversation"));
    assert!(
        failure.message.contains("close this pane"),
        "a refusal that retrying cannot clear names the way out"
    );
    assert!(resumed.is_none());
    // An unproved exit is refused by the daemon, and the reason is shown.
    let (result, resumed, _) = resume_stranded(source.clone(), |_| {
        ServerMessage::Error(ipc::ProtocolError::new(
            ipc::ProtocolErrorCode::TerminalUnavailable,
            "The old provider owner has no confirmed close receipt.",
            true,
        ))
    });
    let failure = result.unwrap_err();
    assert_eq!(failure.recovery, crate::chat::Recovery::Retry);
    assert!(failure.message.contains("no confirmed close receipt"));
    assert!(failure.message.contains("nothing was restarted or resent"));
    assert!(resumed.is_none());
    // A successor that changed the conversation is never started.
    let (result, resumed, _) = resume_stranded(source, |source| {
        let mut successor = source.clone();
        successor.id = TerminalId::new();
        successor.proof = None;
        successor.native_continuity = Default::default();
        successor.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        ServerMessage::Response {
            response: ServerResponse::ChatResumePrepared { session: successor },
        }
    });
    assert!(result.is_err());
    assert!(resumed.is_none());
}

#[test]
fn stranded_retained_claude_card_without_a_conversation_id_asks_the_daemon() {
    // The daemon names what the ended actor confirmed, or proves it
    // confirmed nothing; the card starts that successor either way.
    for learned in [Some("6f1f6f1e-4a4b-4c4d-8e8f-909192939495"), None] {
        let source = stranded_claude(None);
        let (result, resumed, asked) = resume_stranded(source.clone(), move |source| {
            let mut successor = source.clone();
            successor.id = TerminalId::new();
            successor.proof = None;
            successor.native_continuity = Default::default();
            successor.launch.session_id = learned.map(str::to_owned);
            ServerMessage::Response {
                response: ServerResponse::ChatResumePrepared { session: successor },
            }
        });
        let Ok(ClientRequest::ChatStart { session, .. }) = result else {
            panic!("an unnamed card must start the daemon's successor");
        };
        assert_eq!(session.launch.session_id.as_deref(), learned);
        assert_eq!(resumed, Some(source));
        assert_eq!(asked, 2, "upgrade lineage, then resume preparation");
    }
    // A successor that changes anything but the learned conversation is
    // refused, as is one naming something that is not a conversation.
    for changed in [true, false] {
        let (result, resumed, _) = resume_stranded(stranded_claude(None), move |source| {
            let mut successor = source.clone();
            successor.id = TerminalId::new();
            successor.proof = None;
            successor.native_continuity = Default::default();
            if changed {
                successor.launch.recipe.arguments.push("--other".into());
            } else {
                successor.launch.session_id = Some("not-a-conversation".into());
            }
            ServerMessage::Response {
                response: ServerResponse::ChatResumePrepared { session: successor },
            }
        });
        assert!(result.is_err());
        assert!(resumed.is_none());
    }
    // Without a retirement that records the conversation, the daemon
    // refuses permanently for this card; Resume can locate a new descriptor.
    let (result, resumed, _) = resume_stranded(stranded_claude(None), |_| {
        ServerMessage::Error(ipc::ProtocolError::new(
            ipc::ProtocolErrorCode::TerminalUnavailable,
            "The saved Claude actor ended without a retirement that records which conversation it confirmed",
            false,
        ))
    });
    assert_eq!(result.unwrap_err().recovery, crate::chat::Recovery::Blocked);
    assert!(resumed.is_none());
}

#[test]
fn explicit_retry_classifies_exact_history_reader_and_live_successor() {
    for state in 0..7 {
        let proof = host_proof("current");
        let mut source = omp_card(Some(CONVERSATION), Some(proof.clone()));
        source.launch.provider = crate::agent::Provider::Claude;
        source.launch.recipe.program = "claude".into();
        source.launch.recipe.arguments.clear();
        let mut hosted = source.clone();
        if state == 1 {
            hosted.native_continuity =
                super::super::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        } else if state == 2 || state == 4 {
            source.proof = Some(host_proof("previous"));
        }
        let expected = hosted.clone();
        let expected_source = source.clone();
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let server_proof = proof.clone();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            for step in 0..2 {
                let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                let response = match (step, request.request) {
                    (0, ClientRequest::ChatList) => ServerResponse::ChatListed {
                        sessions: if state < 3 || state >= 5 {
                            vec![expected.clone()]
                        } else {
                            vec![]
                        },
                    },
                    (1, ClientRequest::ChatPrepareResume { source })
                        if (state < 3 || state >= 5) && source == expected =>
                    {
                        ServerResponse::ChatResumePrepared {
                            session: expected.clone(),
                        }
                    }
                    (1, ClientRequest::ResolveUpgradeChat { source })
                        if state >= 3 && source == expected_source =>
                    {
                        ServerResponse::UpgradeChatResolved {
                            session: expected.clone(),
                        }
                    }
                    other => panic!("unexpected explicit retry request: {other:?}"),
                };
                codec
                    .write(
                        &mut server,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(request.request_id),
                            proof: server_proof.clone(),
                            message: if step == 1 && state >= 5 {
                                ServerMessage::Error(ipc::ProtocolError::new(
                                    ipc::ProtocolErrorCode::InvalidRequest,
                                    "Exact resume preparation refusal",
                                    state == 5,
                                ))
                            } else {
                                ServerMessage::Response { response }
                            },
                        },
                    )
                    .unwrap();
            }
        });
        let result = restore(
            &mut client,
            &mut FrameCodec::new(ipc::MAX_FRAME_BYTES),
            &mut 1,
            &proof,
            source,
            &mut None,
            &mut None,
            Intent::RetryStart,
        );
        worker.join().unwrap();
        match state {
            0 | 3 => assert!(
                matches!(result, Ok(ClientRequest::ChatStart { session, source: None, terminate_source: false, .. }) if session == hosted)
            ),
            1 => assert!(
                matches!(result, Ok(ClientRequest::ChatAttach { session }) if session == hosted)
            ),
            2 | 4 => assert!(
                result
                    .unwrap_err()
                    .message
                    .contains("changed the explicit retry request")
            ),
            5 | 6 => {
                let failure = result.unwrap_err();
                assert_eq!(
                    failure.recovery,
                    if state == 5 {
                        crate::chat::Recovery::Retry
                    } else {
                        crate::chat::Recovery::Blocked
                    }
                );
                assert_eq!(failure.message, "Exact resume preparation refusal");
            }
            _ => unreachable!(),
        }
    }
}

fn negotiate_with_recovery(
    live: &HostSessionProof,
    session: Session,
    answer: ServerMessage,
) -> (
    Result<ClientRequest, StartFailure>,
    ClientRequest,
    Option<(Session, Session)>,
) {
    negotiate_with_restore_policy(live, session, answer, false)
}

fn negotiate_with_restore_policy(
    live: &HostSessionProof,
    session: Session,
    answer: ServerMessage,
    existing_only: bool,
) -> (
    Result<ClientRequest, StartFailure>,
    ClientRequest,
    Option<(Session, Session)>,
) {
    let (mut client, mut server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let proof = live.clone();
    let worker = thread::spawn(move || {
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let envelope: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        codec
            .write(
                &mut server,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(envelope.request_id),
                    proof,
                    message: answer,
                },
            )
            .unwrap();
        envelope.request
    });
    let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
    let mut request_id = 2;
    let mut recovery = None;
    let outcome = restore(
        &mut client,
        &mut codec,
        &mut request_id,
        live,
        session,
        &mut recovery,
        &mut None,
        if existing_only {
            Intent::RestoreExisting
        } else {
            Intent::Restore
        },
    );
    (outcome, worker.join().unwrap(), recovery)
}

#[test]
fn recovery_restore_policy_leaves_explicit_provider_actions_unchanged() {
    use crate::instance::WindowRestorePolicy::{ExistingOnly, Ordinary};
    for intent in [Intent::Start, Intent::Resume, Intent::RetryStart] {
        assert_eq!(restore_intent(intent, ExistingOnly), intent);
        assert_eq!(restore_intent(intent, Ordinary), intent);
    }
    assert_eq!(
        restore_intent(Intent::Restore, ExistingOnly),
        Intent::RestoreExisting
    );
    assert_eq!(restore_intent(Intent::Restore, Ordinary), Intent::Restore);
}

#[test]
fn recovery_restore_does_not_autostart_a_missing_daemon() {
    const CHILD: &str = "CAGENTS_TEST_RECOVERY_ATTACH_WITHOUT_DAEMON";
    if std::env::var_os(CHILD).is_some() {
        crate::instance::initialize_window_restore_policy(
            crate::instance::WindowRestorePolicy::ExistingOnly,
        )
        .unwrap();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        assert!(!paths.lock.exists());
        // Eager background publishers and device/pane readers share this
        // entrypoint; they must not create a daemon admission either.
        let assert_socket_only = |result| match result {
            Err(crate::terminal::HostedTerminalStartError::HostProtocol(
                crate::control::ControlError::Connect(error),
            )) => assert!(matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            )),
            other => panic!(
                "recovery must fail at the existing socket, before binary lookup or daemon admission: {other:?}"
            ),
        };
        assert_socket_only(crate::terminal::ensure_daemon_ready());
        assert!(!paths.lock.exists());
        assert_socket_only(crate::terminal::ensure_daemon_ready_for(
            crate::notifications::Application::Cagents,
        ));
        assert!(!paths.lock.exists());
        let result = ClientHandle::connect(
            stranded_claude(Some(CONVERSATION)),
            Intent::RestoreExisting,
            None,
            false,
            None,
            |_| {},
        );
        let failure = result.err().expect("missing daemon must refuse attachment");
        assert!(
            failure
                .message
                .starts_with("Cannot connect to the proven chat daemon:")
        );
        assert!(
            !paths.lock.exists(),
            "a read-only recovery attachment tried daemon admission"
        );
        assert!(!paths.socket.exists());
        crate::instance::finish_window_restore();
        assert_eq!(
            crate::instance::window_restore_policy(),
            crate::instance::WindowRestorePolicy::Ordinary
        );
        // The ordinary terminal/new-work path now reaches autostart's
        // executable lookup. This lib-test executable has no daemon
        // companion, so it refuses there without launching anything.
        assert!(matches!(
            crate::terminal::ensure_daemon_ready(),
            Err(crate::terminal::HostedTerminalStartError::HostBinaryMissing(_))
        ));
        // A queued initial attachment keeps its explicit intent even
        // when the worker does not run until after the startup verdict.
        let delayed = ClientHandle::connect(
            stranded_claude(Some(CONVERSATION)),
            Intent::RestoreExisting,
            None,
            false,
            None,
            |_| {},
        )
        .err()
        .unwrap();
        assert!(
            delayed
                .message
                .starts_with("Cannot connect to the proven chat daemon:")
        );
        let ordinary = ClientHandle::connect(
            stranded_claude(Some(CONVERSATION)),
            Intent::Restore,
            None,
            false,
            None,
            |_| {},
        )
        .err()
        .unwrap();
        assert!(
            !ordinary
                .message
                .starts_with("Cannot connect to the proven chat daemon:")
        );
        assert!(!paths.lock.exists());
        return;
    }
    let root = tempfile::tempdir().unwrap();
    for directory in ["home", "runtime", "config", "data", "cache"] {
        std::fs::create_dir(root.path().join(directory)).unwrap();
    }
    let name = std::thread::current().name().unwrap().to_owned();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", &name, "--nocapture"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.path().join("home"))
        .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env(CHILD, "1");
    // Keep hosted authority: the exact socket-only error below must prove
    // policy selection without relying on a separate lifecycle refusal.
    if let Some(authority) = std::env::var_os(crate::agent_execution::AUTHORITY_ENV) {
        command.env(crate::agent_execution::AUTHORITY_ENV, authority);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn recovery_restore_missing_claude_and_codex_never_prepare_or_start_a_provider() {
    for provider in [
        crate::agent::Provider::Claude,
        crate::agent::Provider::Codex,
    ] {
        for named in [false, true] {
            for proofless in [false, true] {
                let (stream, mut server) = UnixStream::pair().unwrap();
                let live = host_proof("recovery");
                let mut saved = if provider == crate::agent::Provider::Claude {
                    stranded_claude(named.then_some(CONVERSATION))
                } else {
                    let mut saved =
                        omp_card(named.then_some(CONVERSATION), Some(host_proof("old")));
                    saved.launch.provider = provider;
                    saved.launch.recipe.program = "codex".into();
                    saved
                };
                if proofless {
                    saved.proof = None;
                }
                let expected = saved.clone();
                let worker = thread::spawn(move || {
                    server
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    for step in 0..2 {
                        let envelope: ClientEnvelope =
                            ipc::read_frame(&mut server).unwrap().unwrap();
                        let message = match (step, envelope.request) {
                            (0, ClientRequest::Connect { .. }) => ServerMessage::Response {
                                response: ServerResponse::Connected {
                                    proof: live.clone(),
                                    resumed: vec![],
                                },
                            },
                            (1, ClientRequest::ChatList) if proofless => ServerMessage::Response {
                                response: ServerResponse::ChatListed { sessions: vec![] },
                            },
                            (1, ClientRequest::ResolveUpgradeChat { source })
                                if !proofless && source == expected =>
                            {
                                ServerMessage::Error(ipc::ProtocolError::new(
                                    ipc::ProtocolErrorCode::TerminalUnavailable,
                                    "The exact retained holder is unavailable",
                                    false,
                                ))
                            }
                            other => {
                                panic!("recovery must not prepare or start a provider: {other:?}")
                            }
                        };
                        ipc::write_frame(
                            &mut server,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id: Some(envelope.request_id),
                                proof: live.clone(),
                                message,
                            },
                        )
                        .unwrap();
                    }
                    let extra: Option<ClientEnvelope> = ipc::read_frame(&mut server).unwrap();
                    assert!(
                        extra.is_none(),
                        "a missing recovery attachment sent an extra provider request"
                    );
                });
                let result = ClientHandle::connect_on(
                    stream,
                    saved,
                    Intent::RestoreExisting,
                    None,
                    false,
                    None,
                    None,
                    None,
                    |_| {},
                );
                let failure = result.err().expect("missing exact lineage must refuse");
                assert_eq!(
                    failure.recovery,
                    if proofless {
                        super::super::Recovery::Retry
                    } else {
                        super::super::Recovery::Blocked
                    }
                );
                assert!(failure.message.contains(if proofless {
                    "Start or Resume"
                } else {
                    "The exact retained holder is unavailable"
                }));
                worker.join().unwrap();
            }
        }
    }
}

#[test]
fn recovery_restore_preserves_typed_refusals_without_provider_requests() {
    for (code, retryable, recovery) in [
        (
            ipc::ProtocolErrorCode::TerminalUnavailable,
            false,
            super::super::Recovery::Blocked,
        ),
        (
            ipc::ProtocolErrorCode::TerminalUnavailable,
            true,
            super::super::Recovery::Retry,
        ),
        (
            ipc::ProtocolErrorCode::AccountRemoved,
            false,
            super::super::Recovery::Account,
        ),
        (
            ipc::ProtocolErrorCode::ChatCapacity,
            false,
            super::super::Recovery::Capacity,
        ),
    ] {
        let source = stranded_claude(Some(CONVERSATION));
        let expected = source.clone();
        let live = host_proof("recovery");
        let (stream, mut server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            for step in 0..2 {
                let envelope: ClientEnvelope = ipc::read_frame(&mut server).unwrap().unwrap();
                let message = match (step, envelope.request) {
                    (0, ClientRequest::Connect { .. }) => ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: live.clone(),
                            resumed: vec![],
                        },
                    },
                    (1, ClientRequest::ResolveUpgradeChat { source }) if source == expected => {
                        ServerMessage::Error(ipc::ProtocolError::new(
                            code,
                            "Exact saved source refusal",
                            retryable,
                        ))
                    }
                    other => {
                        panic!("read-only restore must not prepare or start a provider: {other:?}")
                    }
                };
                ipc::write_frame(
                    &mut server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(envelope.request_id),
                        proof: live.clone(),
                        message,
                    },
                )
                .unwrap();
            }
            let extra: Option<ClientEnvelope> = ipc::read_frame(&mut server).unwrap();
            assert!(
                extra.is_none(),
                "typed refusal must not trigger Attach, PrepareResume, or Start"
            );
        });
        let failure = ClientHandle::connect_on(
            stream,
            source,
            Intent::RestoreExisting,
            None,
            false,
            None,
            None,
            None,
            |_| {},
        )
        .err()
        .expect("daemon refused attachment");
        assert_eq!(failure.recovery, recovery);
        assert_eq!(failure.message, "Exact saved source refusal");
        worker.join().unwrap();
    }
}

/// A recovery pane only attaches, so a refused attachment it could act
/// on becomes Retry. A permanent refusal, including one a restart lifts,
/// keeps its kind and the daemon's exact words.
#[test]
fn recovery_restore_keeps_permanent_attach_refusals_as_they_are() {
    use super::super::{Recovery, UNPROVEN_OWNER_REFUSAL};
    for (code, message, retryable, recovery) in [
        (
            ipc::ProtocolErrorCode::TerminalUnavailable,
            "Exact attachment refusal",
            false,
            Recovery::Blocked,
        ),
        (
            ipc::ProtocolErrorCode::InvalidRequest,
            UNPROVEN_OWNER_REFUSAL,
            false,
            Recovery::AfterRestart,
        ),
        (
            ipc::ProtocolErrorCode::AccountRemoved,
            "Exact attachment refusal",
            false,
            Recovery::Retry,
        ),
    ] {
        let live = host_proof("recovery");
        let saved = stranded_claude(Some(CONVERSATION));
        let mut current = saved.clone();
        current.proof = Some(live.clone());
        let expected = saved.clone();
        let (stream, mut server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            for step in 0..3 {
                let envelope: ClientEnvelope = ipc::read_frame(&mut server).unwrap().unwrap();
                let reply = match (step, envelope.request) {
                    (0, ClientRequest::Connect { .. }) => ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: live.clone(),
                            resumed: vec![],
                        },
                    },
                    (1, ClientRequest::ResolveUpgradeChat { source }) if source == expected => {
                        ServerMessage::Response {
                            response: ServerResponse::UpgradeChatResolved {
                                session: current.clone(),
                            },
                        }
                    }
                    (2, ClientRequest::ChatAttach { session }) if session == current => {
                        ServerMessage::Error(ipc::ProtocolError::new(code, message, retryable))
                    }
                    other => panic!("read-only restore must only attach: {other:?}"),
                };
                ipc::write_frame(
                    &mut server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(envelope.request_id),
                        proof: live.clone(),
                        message: reply,
                    },
                )
                .unwrap();
            }
            let extra: Option<ClientEnvelope> = ipc::read_frame(&mut server).unwrap();
            assert!(extra.is_none(), "a refused attachment starts nothing");
        });
        let failure = ClientHandle::connect_on(
            stream,
            saved,
            Intent::RestoreExisting,
            None,
            false,
            None,
            None,
            None,
            |_| {},
        )
        .err()
        .expect("daemon refused attachment");
        worker.join().unwrap();
        assert_eq!(failure.recovery, recovery, "{code:?}");
        if recovery == Recovery::Retry {
            assert!(failure.message.starts_with(message), "{}", failure.message);
            assert!(
                failure.message.contains("only attaches"),
                "{}",
                failure.message
            );
        } else {
            assert_eq!(failure.message, message, "{code:?}");
        }
    }
}

#[test]
fn recovery_restore_keeps_exact_live_and_issued_history_attachments() {
    let live = host_proof("recovery");
    let retained = stranded_claude(Some("6f1f6f1e-4a4b-4c4d-8e8f-909192939495"));
    for history in [false, true] {
        let mut saved = retained.clone();
        if history {
            saved.native_continuity = super::super::NativeContinuity::ReconnectOnly;
        }
        let mut current = saved.clone();
        current.proof = Some(live.clone());
        let (outcome, asked, _) = negotiate_with_restore_policy(
            &live,
            saved.clone(),
            ServerMessage::Response {
                response: ServerResponse::UpgradeChatResolved {
                    session: current.clone(),
                },
            },
            true,
        );
        assert!(matches!(asked, ClientRequest::ResolveUpgradeChat { source } if source == saved));
        assert!(matches!(outcome, Ok(ClientRequest::ChatAttach { session }) if session == current));
    }
    let mut same_owner = retained;
    same_owner.proof = Some(live.clone());
    let (mut client, mut server) = UnixStream::pair().unwrap();
    let request = restore(
        &mut client,
        &mut FrameCodec::new(ipc::MAX_FRAME_BYTES),
        &mut 2,
        &live,
        same_owner.clone(),
        &mut None,
        &mut None,
        Intent::RestoreExisting,
    )
    .unwrap();
    assert!(matches!(request, ClientRequest::ChatAttach { session } if session == same_owner));
    drop(client);
    assert!(
        ipc::read_frame::<ClientEnvelope>(&mut server)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_changed_midstream_proof_retries_a_new_handshake_instead_of_binding() {
    let expected = host_proof("expected");
    let foreign = host_proof("foreign");
    let (mut client, mut server) = UnixStream::pair().unwrap();
    FrameCodec::new(ipc::MAX_FRAME_BYTES)
        .write(
            &mut server,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(2),
                proof: foreign,
                message: ServerMessage::Error(ipc::ProtocolError::new(
                    ipc::ProtocolErrorCode::ProofMismatch,
                    "The former connection changed its daemon proof",
                    false,
                )),
            },
        )
        .unwrap();
    let failure = receive_start(
        &mut client,
        &mut FrameCodec::new(ipc::MAX_FRAME_BYTES),
        2,
        Some(&expected),
    )
    .unwrap_err();
    assert_eq!(failure.recovery, crate::chat::Recovery::Retry);
    assert!(failure.message.contains("former connection"));
}

#[test]
fn initial_hard_target_rejects_a_foreign_proof_response_without_attaching() {
    let expected = host_proof("expected");
    let foreign = host_proof("foreign");
    let (mut client, mut server) = UnixStream::pair().unwrap();
    let worker = thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(request.request, ClientRequest::Connect { .. }));
        codec
            .write(
                &mut server,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: foreign.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: foreign,
                            resumed: vec![],
                        },
                    },
                },
            )
            .unwrap();
        assert!(
            codec.read::<ClientEnvelope>(&mut server).unwrap().is_none(),
            "a mismatched response cannot lead to an Attach or Start"
        );
    });
    let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
    codec
        .write(
            &mut client,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id: 1,
                proof: None,
                request: ClientRequest::Connect {
                    expected_host: Some(expected.clone()),
                    resume: vec![],
                },
            },
        )
        .unwrap();
    let failure = receive_connect(&mut client, &mut codec, 1, Some(&expected)).unwrap_err();
    assert_eq!(failure.recovery, crate::chat::Recovery::Blocked);
    drop(client);
    worker.join().unwrap();
}

#[test]
fn cold_retained_codex_resolve_refusal_keeps_the_daemons_recovery() {
    let live = host_proof("current");
    for retryable in [false, true] {
        let mut source = omp_card(Some(CONVERSATION), Some(host_proof("previous")));
        source.launch.provider = crate::agent::Provider::Codex;
        source.launch.recipe.program = "codex".into();
        source.native_continuity = super::super::NativeContinuity::Durable {
            provider_version: "0.100.0".into(),
        };
        let (result, request, recovery) = negotiate_with_recovery(
            &live,
            source.clone(),
            ServerMessage::Error(ipc::ProtocolError::new(
                ipc::ProtocolErrorCode::InvalidRequest,
                "Exact retained source refusal",
                retryable,
            )),
        );
        assert!(
            matches!(request, ClientRequest::ResolveUpgradeChat { source: requested } if requested == source)
        );
        let failure = result.unwrap_err();
        assert_eq!(
            failure.recovery,
            if retryable {
                crate::chat::Recovery::Retry
            } else {
                crate::chat::Recovery::Blocked
            }
        );
        assert!(failure.message.contains("Exact retained source refusal"));
        assert!(recovery.is_none());
    }
}

#[test]
fn same_thread_codex_resume_normalization_requires_explicit_rebind() {
    let previous = host_proof("previous");
    let live = host_proof("live");
    let mut source = omp_card(None, Some(previous.clone()));
    source.launch.provider = crate::agent::Provider::Codex;
    source.launch.recipe.program = "codex".into();
    source.launch.recipe.arguments = vec!["--remote".into(), "unix://".into()];
    assert!(source.claim_owned_codex_listener(std::path::Path::new("/run/test-native"), &previous));
    source.launch.session_id = Some(CONVERSATION.into());
    source.native_continuity = super::super::NativeContinuity::Durable {
        provider_version: "0.100.0".into(),
    };
    let mut replacement = source.clone();
    replacement.proof = Some(live.clone());
    replacement
        .launch
        .recipe
        .arguments
        .splice(0..0, ["resume".into(), CONVERSATION.into()]);
    let (outcome, _, recovery) = negotiate_with_recovery(
        &live,
        source.clone(),
        ServerMessage::Response {
            response: ServerResponse::UpgradeChatResolved {
                session: replacement.clone(),
            },
        },
    );
    assert!(matches!(outcome, Ok(ClientRequest::ChatAttach { session }) if session == replacement));
    assert_eq!(recovery, Some((source.clone(), replacement.clone())));
    for alteration in 0..5 {
        let mut invalid = replacement.clone();
        match alteration {
            0 => invalid.launch.recipe.arguments[1] = "other-thread".into(),
            1 => invalid.launch.recipe.arguments.push("--different".into()),
            2 => {
                invalid
                    .launch
                    .recipe
                    .environment
                    .insert("ACCOUNT_HOME".into(), "/other".into());
            }
            3 => invalid.launch.recipe.program = "other-provider".into(),
            _ => invalid.owned_codex_listener = None,
        }
        let (outcome, _, recovery) = negotiate_with_recovery(
            &live,
            source.clone(),
            ServerMessage::Response {
                response: ServerResponse::UpgradeChatResolved { session: invalid },
            },
        );
        assert!(outcome.is_err(), "alteration {alteration}");
        assert!(recovery.is_none());
    }
}

#[test]
fn authenticated_upgrade_rotation_preserves_provenance_and_known_thread_identity() {
    let live = host_proof("live");
    for previous in [None, Some(CONVERSATION), Some("previous-conversation")] {
        let source = omp_card(previous, Some(host_proof("previous")));
        let mut resolved = source.clone();
        resolved.proof = Some(live.clone());
        resolved.launch.session_id = Some(CONVERSATION.into());
        let (outcome, _, recovery) = negotiate_with_recovery(
            &live,
            source.clone(),
            ServerMessage::Response {
                response: ServerResponse::UpgradeChatResolved {
                    session: resolved.clone(),
                },
            },
        );
        if previous.is_some_and(|id| id != CONVERSATION) {
            assert!(outcome.is_err(), "a known provider thread cannot rotate");
            assert!(recovery.is_none());
        } else {
            assert!(
                matches!(outcome, Ok(ClientRequest::ChatAttach { session }) if session == resolved)
            );
            assert_eq!(recovery, Some((source, resolved)));
        }
    }
    let source = omp_card(Some(CONVERSATION), Some(host_proof("previous")));
    let mut resolved = source.clone();
    resolved.proof = Some(live.clone());
    resolved.launch.session_id = Some(CONVERSATION.into());
    resolved
        .launch
        .recipe
        .environment
        .insert("ACCOUNT_HOME".into(), "/different".into());
    let (outcome, _, recovery) = negotiate_with_recovery(
        &live,
        source,
        ServerMessage::Response {
            response: ServerResponse::UpgradeChatResolved { session: resolved },
        },
    );
    assert!(outcome.is_err());
    assert!(recovery.is_none());
}

/// An application update replaces the executables and restarts the daemon,
/// so every saved chat proof is stale afterwards and the daemon holds no
/// upgrade checkpoint for a restart it did not perform itself. A card that
/// treated that as an exact-attach failure never started the provider that
/// would have read its conversation back.
#[test]
fn a_restarted_daemon_still_resumes_the_conversation_a_card_was_showing() {
    let live = host_proof("live");
    let stale = host_proof("before-the-update");

    // The hosting daemon is still the live one, so its writer is there to
    // attach to and nothing needs to be asked first.
    let (mut client, _server) = UnixStream::pair().unwrap();
    let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
    let mut request_id = 2;
    let attach = restore(
        &mut client,
        &mut codec,
        &mut request_id,
        &live,
        omp_card(Some(CONVERSATION), Some(live.clone())),
        &mut None,
        &mut None,
        Intent::Restore,
    )
    .expect("a live proof attaches");
    assert!(matches!(attach, ClientRequest::ChatAttach { .. }));
    assert_eq!(request_id, 2, "an attach asks the daemon nothing first");

    // The saved proof is stale and the restart left no lineage, which is
    // every card after an installer-driven update.
    let (outcome, asked) = negotiate(
        &live,
        omp_card(Some(CONVERSATION), Some(stale.clone())),
        ServerMessage::Error(crate::terminal_ipc::ProtocolError::new(
            crate::terminal_ipc::ProtocolErrorCode::TerminalUnavailable,
            "No durable upgrade replacement exists for this native chat",
            false,
        )),
    );
    assert!(
        matches!(asked, ClientRequest::ResolveUpgradeChat { .. }),
        "a stale proof asks the daemon for its upgrade lineage before giving up on attaching"
    );
    let ClientRequest::ChatStart {
        session,
        source,
        terminate_source,
        ..
    } = outcome.expect("a refused lineage resumes rather than failing")
    else {
        panic!("a card with a saved conversation must start the provider on it");
    };
    assert_eq!(
        session.launch.session_id.as_deref(),
        Some(CONVERSATION),
        "the exact saved conversation is what the provider is started on"
    );
    assert_eq!(
        session.proof.as_ref(),
        Some(&live),
        "the start carries the proof the live daemon just gave, not the saved one"
    );
    assert!(source.is_none() && !terminate_source);

    // A conversation converted from a terminal resumes the same way, but
    // must carry no client-chosen execution environment: the daemon
    // refuses one outright for any chat that still names a source
    // terminal, and a refused start is a card that does not come back.
    let mut converted = omp_card(Some(CONVERSATION), Some(stale));
    converted.terminal = Some(crate::model::TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: converted.id,
        launch: converted.launch.recipe.clone(),
        continuity: crate::model::TerminalContinuity::RelaunchAvailable {
            recipe: converted.launch.recipe.clone(),
            last_exit: None,
        },
        resume_hint: None,
        transcript_path: None,
    });
    let (outcome, _) = negotiate(
        &live,
        converted,
        ServerMessage::Error(crate::terminal_ipc::ProtocolError::new(
            crate::terminal_ipc::ProtocolErrorCode::TerminalUnavailable,
            "No durable upgrade replacement exists for this native chat",
            false,
        )),
    );
    let ClientRequest::ChatStart {
        execution_environment,
        ..
    } = outcome.expect("a terminal-derived card resumes too")
    else {
        panic!("a card with a saved conversation must start the provider on it");
    };
    assert!(
        execution_environment.0.is_empty(),
        "a chat naming a source terminal takes the daemon's environment, never a client's"
    );
}

/// A daemon that did upgrade itself can still map the saved descriptor onto
/// the conversation it restarted under, and that attachment wins over
/// starting a second provider on the same conversation.
#[test]
fn a_proven_upgrade_lineage_attaches_instead_of_starting_a_second_provider() {
    let live = host_proof("live");
    let saved = omp_card(Some(CONVERSATION), Some(host_proof("before-the-upgrade")));
    let mut restored = saved.clone();
    restored.proof = Some(live.clone());
    let (outcome, _) = negotiate(
        &live,
        saved.clone(),
        ServerMessage::Response {
            response: ServerResponse::UpgradeChatResolved {
                session: restored.clone(),
            },
        },
    );
    let ClientRequest::ChatAttach { session } = outcome.expect("a proven lineage attaches") else {
        panic!("a proven lineage must attach");
    };
    assert_eq!(session, restored);

    let mut wrong_thread = restored.clone();
    wrong_thread.launch.session_id = Some("different-known-thread".into());
    let (outcome, _) = negotiate(
        &live,
        saved.clone(),
        ServerMessage::Response {
            response: ServerResponse::UpgradeChatResolved {
                session: wrong_thread,
            },
        },
    );
    assert!(
        outcome.is_err(),
        "a confirmed replacement cannot change a saved known thread"
    );

    // A replacement for a different chat proves nothing about this one.
    // A known conversation identifier remains part of its exact source.
    let mut other = restored.clone();
    other.id = TerminalId::new();
    let (outcome, _) = negotiate(
        &live,
        saved,
        ServerMessage::Response {
            response: ServerResponse::UpgradeChatResolved { session: other },
        },
    );
    assert!(
        matches!(outcome, Ok(ClientRequest::ChatStart { .. })),
        "a mismatched lineage falls back to resuming, never to attaching"
    );
}

/// A conversation that cannot be resumed and cannot be named is the one
/// case where retrying is not a real answer, so the card is told to offer
/// the only thing left instead of being handed a retry that cannot work.
#[test]
fn a_card_with_nothing_to_resume_asks_for_a_new_conversation_in_the_daemons_words() {
    let live = host_proof("live");
    let (outcome, _) = negotiate(
        &live,
        omp_card(None, Some(host_proof("before-the-update"))),
        ServerMessage::Error(crate::terminal_ipc::ProtocolError::new(
            crate::terminal_ipc::ProtocolErrorCode::TerminalUnavailable,
            "No durable upgrade replacement exists for this native chat",
            false,
        )),
    );
    let failure = outcome.expect_err("nothing can be resumed here");
    assert_eq!(failure.recovery, crate::chat::Recovery::Fresh);
    assert!(
        failure
            .message
            .contains("No durable upgrade replacement exists for this native chat"),
        "the daemon's own refusal has to reach the card: {}",
        failure.message
    );
}

/// Drives a whole restore against a daemon that has just restarted: it
/// proves a new identity, holds no upgrade checkpoint for a restart it did
/// not perform, and then answers the resume with `answer`.
fn restore_against(
    session: Session,
    answer: impl FnOnce(ClientRequest) -> ServerMessage + Send + 'static,
) -> Result<ClientHandle, StartFailure> {
    restore_with_lineage(session, None, answer)
}

#[test]
fn canonical_upgrade_requires_exact_lineage_before_attach_and_never_starts() {
    // 0: exact success; 1: no lineage; 2: different resolved descriptor;
    // 3: changed live daemon; 4: changed attached descriptor;
    // 5: unconfirmed snapshot; 6: invalid canonical source transition;
    // 7: an unnamed source learns its authenticated identity.
    for case in 0..8 {
        let source = omp_card(
            (case != 7).then_some(CONVERSATION),
            Some(host_proof("prior")),
        );
        let mut expected = source.clone();
        expected.proof = Some(host_proof("live"));
        expected.launch.session_id = Some(CONVERSATION.into());
        if case == 6 {
            expected.launch.recipe.program = "another-program".into();
        }
        let mut live = host_proof("live");
        if case == 3 {
            live = host_proof("unexpected-live");
        }
        let (client, mut server) = UnixStream::pair().unwrap();
        let original = source.clone();
        let canonical = expected.clone();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let mut requests = Vec::new();
            while let Some(request) = codec.read::<ClientEnvelope>(&mut server).unwrap() {
                let finished = matches!(&request.request, ClientRequest::ChatAttach { .. });
                let response = match &request.request {
                    ClientRequest::Connect { expected_host, .. } => {
                        assert_eq!(expected_host, &canonical.proof);
                        ServerMessage::Response {
                            response: ServerResponse::Connected {
                                proof: live.clone(),
                                resumed: Vec::new(),
                            },
                        }
                    }
                    ClientRequest::ResolveUpgradeChat { source } => {
                        assert_eq!(source, &original);
                        assert_eq!(request.proof.as_ref(), Some(&live));
                        if case == 1 {
                            ServerMessage::Error(ipc::ProtocolError::new(
                                ipc::ProtocolErrorCode::TerminalUnavailable,
                                "No authenticated upgrade lineage",
                                false,
                            ))
                        } else {
                            let mut resolved = canonical.clone();
                            if case == 2 {
                                resolved.launch.recipe.arguments.push("--changed".into());
                            }
                            ServerMessage::Response {
                                response: ServerResponse::UpgradeChatResolved { session: resolved },
                            }
                        }
                    }
                    ClientRequest::ChatAttach { session } => {
                        assert_eq!(session, &canonical);
                        assert_eq!(request.proof.as_ref(), Some(&live));
                        let mut session = session.clone();
                        if case == 4 {
                            session.launch.recipe.arguments.push("--changed".into());
                        }
                        ServerMessage::Response {
                            response: ServerResponse::ChatAttached {
                                session,
                                snapshot: Snapshot {
                                    session_id: Some(
                                        if case == 5 {
                                            "other-thread"
                                        } else {
                                            CONVERSATION
                                        }
                                        .into(),
                                    ),
                                    ready: true,
                                    ..Default::default()
                                },
                            },
                        }
                    }
                    other => panic!("strict upgrade must never issue {other:?}"),
                };
                requests.push(request.request);
                codec
                    .write(
                        &mut server,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(request.request_id),
                            proof: live.clone(),
                            message: response,
                        },
                    )
                    .unwrap();
                if finished {
                    break;
                }
            }
            requests
        });
        let result = ClientHandle::connect_on(
            client,
            source.clone(),
            Intent::Restore,
            None,
            false,
            None,
            None,
            Some(expected.clone()),
            |_| {},
        );
        let requests = worker.join().unwrap();
        let count = match case {
            6 => 0,
            3 => 1,
            1 | 2 => 2,
            _ => 3,
        };
        assert_eq!(requests.len(), count, "case {case}");
        if case == 0 || case == 7 {
            let handle = result.unwrap();
            assert_eq!(handle.session(), expected);
            assert_eq!(handle.upgrade_recovery(), Some(&(source, expected)));
        } else {
            assert!(result.is_err(), "case {case} must refuse");
        }
    }
}

#[test]
fn retained_reset_candidates_reject_changed_configuration_before_attach() {
    let live = host_proof("live");
    let mut source = omp_card(Some(CONVERSATION), Some(host_proof("prior")));
    source.launch.provider = crate::agent::Provider::Claude;
    source.launch.recipe.program = "claude".into();
    source.native_continuity = super::super::NativeContinuity::RetainedClaudeActor { protocol: 2 };
    let mut resolved = source.clone();
    resolved.proof = Some(live.clone());
    resolved.launch.session_id = Some("22222222-2222-4222-8222-222222222222".into());
    for mutation in 0..9 {
        let mut changed = resolved.clone();
        match mutation {
            0 => {
                changed
                    .launch
                    .recipe
                    .environment
                    .insert("CLAUDE_CONFIG_DIR".into(), "/other-account".into());
            }
            1 => changed.launch.recipe.working_directory = "/other-project".into(),
            2 => changed.launch.recipe.program = "other-provider".into(),
            3 => changed.launch.recipe.arguments.push("--other-model".into()),
            4 => changed.launch.provider = crate::agent::Provider::Omp,
            5 => {
                changed.native_continuity =
                    super::super::NativeContinuity::RetainedClaudeActor { protocol: 1 }
            }
            6 => changed.launch.session_id = Some("invalid-uuid".into()),
            7 => changed.proof.as_mut().unwrap().boot_id = "other-boot".into(),
            _ => changed.id = TerminalId::new(),
        }
        let (outcome, _, candidate) = negotiate_with_recovery(
            &live,
            source.clone(),
            ServerMessage::Response {
                response: ServerResponse::UpgradeChatResolved { session: changed },
            },
        );
        assert!(outcome.is_err(), "mutation {mutation}");
        assert!(
            candidate.is_none(),
            "invalid resolution cannot become a recovery candidate"
        );
    }
}

#[test]
fn retained_reset_witness_requires_exact_attachment_and_snapshot_confirmation() {
    for mismatch in 0..3 {
        let mut source = omp_card(Some(CONVERSATION), Some(host_proof("prior")));
        source.launch.provider = crate::agent::Provider::Claude;
        source.launch.recipe.program = "claude".into();
        source.native_continuity =
            super::super::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        let mut resolved = source.clone();
        resolved.proof = Some(host_proof("live"));
        resolved.launch.session_id = Some("22222222-2222-4222-8222-222222222222".into());
        assert!(!super::super::is_upgrade_recovery(&source, &resolved));
        assert!(!super::super::is_upgrade_identity_learning(
            &source, &resolved
        ));
        let result = restore_with_lineage(source.clone(), Some(resolved.clone()), move |request| {
            let ClientRequest::ChatAttach { mut session } = request else {
                panic!("resolved recovery must attach without starting");
            };
            let mut snapshot = Snapshot {
                session_id: session.launch.session_id.clone(),
                ..Default::default()
            };
            match mismatch {
                1 => {
                    session
                        .launch
                        .recipe
                        .environment
                        .insert("CLAUDE_CONFIG_DIR".into(), "/other-account".into());
                }
                2 => snapshot.session_id = Some(CONVERSATION.into()),
                _ => {}
            }
            ServerMessage::Response {
                response: ServerResponse::ChatAttached { session, snapshot },
            }
        });
        if mismatch == 0 {
            let handle = result.unwrap();
            let witness = handle.authenticated_upgrade_recovery().unwrap();
            assert_eq!(witness.source(), &source);
            assert_eq!(witness.replacement(), &resolved);
            assert!(matches!(
                witness.kind,
                UpgradeRecoveryKind::BoundRetainedReset
            ));
        } else {
            assert!(result.is_err(), "mismatch {mismatch}");
        }
    }
}

#[test]
fn upgrade_provenance_requires_the_exact_attached_descriptor_and_confirmation() {
    for mismatch in 0..3 {
        let source = omp_card(Some(CONVERSATION), Some(host_proof("prior")));
        let mut resolved = source.clone();
        resolved.proof = Some(host_proof("live"));
        resolved.launch.session_id = Some(CONVERSATION.into());
        let result = restore_with_lineage(source.clone(), Some(resolved.clone()), move |request| {
            let ClientRequest::ChatAttach { mut session } = request else {
                panic!("expected exact attach")
            };
            let mut snapshot = Snapshot {
                session_id: session.launch.session_id.clone(),
                ..Default::default()
            };
            match mismatch {
                1 => {
                    session
                        .launch
                        .recipe
                        .environment
                        .insert("ACCOUNT_HOME".into(), "/other".into());
                }
                2 => snapshot.session_id = Some("unconfirmed-other-thread".into()),
                _ => {}
            }
            ServerMessage::Response {
                response: ServerResponse::ChatAttached { session, snapshot },
            }
        });
        if mismatch == 0 {
            assert_eq!(
                result.unwrap().upgrade_recovery(),
                Some(&(source, resolved))
            );
        } else {
            assert!(result.is_err(), "mismatch {mismatch}");
        }
    }
}

/// Retry is offered only when the daemon says running the same resume
/// can help. Both paths keep its original diagnostic for the reader.
#[test]
fn a_refused_resume_honors_the_daemons_retryability_and_keeps_its_words() {
    let reason = "Cannot join existing Codex daemon for this thread";
    for (retryable, recovery) in [
        (false, crate::chat::Recovery::Blocked),
        (true, crate::chat::Recovery::Retry),
    ] {
        let failure = restore_against(
            omp_card(Some(CONVERSATION), Some(host_proof("before-the-update"))),
            move |_| {
                ServerMessage::Error(ipc::ProtocolError::new(
                    ipc::ProtocolErrorCode::TerminalUnavailable,
                    reason,
                    retryable,
                ))
            },
        )
        .err()
        .expect("the daemon refused this resume");
        assert_eq!(failure.message, reason);
        assert_eq!(failure.recovery, recovery, "retryable={retryable}");
    }
}

/// A conversation the provider has not named yet - one still loading, or
/// one whose resume is about to fail - must not overwrite the identifier
/// the card needs for its next attempt, or one failed restore would turn a
/// recoverable conversation into a permanently blank card.
#[test]
fn a_resume_that_names_no_conversation_yet_keeps_the_saved_one() {
    let handle = restore_against(
        omp_card(Some(CONVERSATION), Some(host_proof("before-the-update"))),
        |request| {
            let ClientRequest::ChatStart { session, .. } = request else {
                panic!("a saved conversation is resumed by starting the provider on it");
            };
            ServerMessage::Response {
                response: ServerResponse::ChatAttached {
                    session,
                    snapshot: Snapshot::default(),
                },
            }
        },
    )
    .expect("the daemon accepted this resume");
    assert_eq!(
        handle.session().launch.session_id.as_deref(),
        Some(CONVERSATION)
    );
}
#[test]
fn hidden_wire_client_releases_history_and_reveals_at_the_same_generation() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let (hidden_tx, hidden_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let proof = host_proof("live");
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
            codec
                .write(
                    server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: proof.clone(),
                        message,
                    },
                )
                .unwrap();
        };
        let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        respond(
            &mut codec,
            &mut server,
            Some(connect.request_id),
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        let ClientRequest::ChatStart { session, .. } = start.request else {
            panic!("chat start expected");
        };
        let chat_id = session.id;
        let snapshot = || Snapshot {
            generation: 41,
            session_id: Some("exact-session".into()),
            ready: true,
            items: vec![Arc::new(super::super::Item {
                id: "history".into(),
                role: super::super::Role::Assistant,
                content: super::super::ItemContent::Text {
                    text: "body".repeat(64 * 1024),
                    thinking: false,
                    phase: Default::default(),
                },
                started_at: None,
                at: None,
            })],
            ..Default::default()
        };
        respond(
            &mut codec,
            &mut server,
            Some(start.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatAttached {
                    session,
                    snapshot: snapshot(),
                },
            },
        );
        let hidden: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            hidden.request,
            ClientRequest::ChatVisibility { visible: false, .. }
        ));
        respond(
            &mut codec,
            &mut server,
            Some(hidden.request_id),
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
        );
        hidden_tx.send(()).unwrap();
        let shown: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            shown.request,
            ClientRequest::ChatVisibility { visible: true, .. }
        ));
        respond(
            &mut codec,
            &mut server,
            Some(shown.request_id),
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
        );
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(
            matches!(
                read.request,
                ClientRequest::ChatRead {
                    after_generation: None,
                    ..
                }
            ),
            "the current generation is metadata only and cannot suppress hydration"
        );
        respond(
            &mut codec,
            &mut server,
            Some(read.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: Some(snapshot()),
                },
            },
        );
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    let handle = ClientHandle::connect_on(
        client,
        omp_card(None, None),
        Intent::Start,
        None,
        false,
        None,
        None,
        None,
        move |wake| {
            let _ = wake_tx.send(wake);
        },
    )
    .unwrap();
    let body = Arc::downgrade(&handle.snapshot().items[0]);
    let installed = handle.inner.publication.current.load_full();
    let (released, retirement) = std::sync::mpsc::channel();
    *installed.retirement_complete.lock().unwrap() = Some(released);
    drop(installed);
    handle.set_visible(false).unwrap();
    hidden_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // The actual wire hide edge no longer implies that the independent
    // local disposal owner has completed. Wait for that exact payload,
    // then retain the same real Weak-body release/residency oracles.
    retirement.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(handle.snapshot().generation, 41);
    assert_eq!(
        handle.snapshot().session_id.as_deref(),
        Some("exact-session")
    );
    assert!(body.upgrade().is_none());
    assert!(handle.resident_snapshot_with_clock().is_none());
    handle.set_visible(true).unwrap();
    while handle.resident_snapshot_with_clock().is_none() {
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    assert_eq!(handle.snapshot().generation, 41);
    assert_eq!(handle.snapshot().transcript_bytes(), 256 * 1024);
    done_tx.send(()).unwrap();
    worker.join().unwrap();
    while !matches!(
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        Wake::Disconnected(_)
    ) {}
    assert!(handle.set_visible(false).is_err());
    assert_eq!(
        handle.snapshot().transcript_bytes(),
        256 * 1024,
        "an unreachable daemon cannot rehydrate the last readable publication yet"
    );
}

#[test]
fn a_read_refused_because_the_chat_just_hid_leaves_its_kept_body_unmarked() {
    use super::super::{Item, ItemContent, Role};
    use crate::terminal_ipc::{ProtocolError, ProtocolErrorCode};
    let answer = Arc::new(Item {
        id: "answer".into(),
        role: Role::Assistant,
        content: ItemContent::Text {
            text: "The kept answer.".into(),
            thinking: false,
            phase: Default::default(),
        },
        started_at: None,
        at: None,
    });
    let body = answer.clone();
    let (client, mut server) = UnixStream::pair().unwrap();
    let (stage_tx, stage_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let proof = host_proof("hide-race");
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
            codec
                .write(
                    server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: proof.clone(),
                        message,
                    },
                )
                .unwrap();
        };
        let accepted = || ServerMessage::Response {
            response: ServerResponse::Accepted,
        };
        let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        respond(
            &mut codec,
            &mut server,
            Some(connect.request_id),
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        let ClientRequest::ChatStart { session, .. } = start.request else {
            panic!("chat start expected");
        };
        let chat_id = session.id;
        respond(
            &mut codec,
            &mut server,
            Some(start.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatAttached {
                    session,
                    snapshot: Snapshot {
                        generation: 1,
                        ..Default::default()
                    },
                },
            },
        );
        let shown: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            shown.request,
            ClientRequest::ChatVisibility { visible: true, .. }
        ));
        respond(&mut codec, &mut server, Some(shown.request_id), accepted());
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(read.request, ClientRequest::ChatRead { .. }));
        respond(
            &mut codec,
            &mut server,
            Some(read.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: Some(Snapshot {
                        generation: 2,
                        ready: true,
                        items: vec![body],
                        ..Default::default()
                    }),
                },
            },
        );
        // Something changed; the client reads, and hides before the
        // daemon answers, so the daemon refuses the read.
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let raced: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(raced.request, ClientRequest::ChatRead { .. }));
        stage_tx.send(()).unwrap();
        let hidden: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            hidden.request,
            ClientRequest::ChatVisibility { visible: false, .. }
        ));
        respond(&mut codec, &mut server, Some(hidden.request_id), accepted());
        respond(
            &mut codec,
            &mut server,
            Some(raced.request_id),
            ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "Hidden chat has no presentation stream; make it visible before reading",
                false,
            )),
        );
        // An ordered edge: the client has consumed the refusal.
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatActivity {
                chat_id,
                state: ActivityState::Working,
                generation: 2,
            },
        );
        // Shown again, it reads after the body it kept.
        let shown: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            shown.request,
            ClientRequest::ChatVisibility { visible: true, .. }
        ));
        respond(&mut codec, &mut server, Some(shown.request_id), accepted());
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(
            matches!(
                read.request,
                ClientRequest::ChatRead {
                    after_generation: Some(2),
                    ..
                }
            ),
            "the kept body stays the base to read after"
        );
        respond(
            &mut codec,
            &mut server,
            Some(read.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: None,
                },
            },
        );
        stage_tx.send(()).unwrap();
    });
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    let handle = ClientHandle::connect_on(
        client,
        omp_card(None, None),
        Intent::Start,
        None,
        false,
        None,
        None,
        None,
        move |wake| {
            let _ = wake_tx.send(wake);
        },
    )
    .unwrap();
    handle.set_interest(true, true).unwrap();
    while handle.resident_snapshot_with_clock().is_none() {
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    stage_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (shown, _) = handle.resident_snapshot_with_clock().unwrap();
    assert_eq!(shown.items[0].content, answer.content);
    // Hidden but keeping its body, as a warm chat does.
    handle.set_interest(false, true).unwrap();
    while !matches!(
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        Wake::Activity
    ) {}
    let (kept, _) = handle.resident_snapshot_with_clock().expect("a kept body");
    assert_eq!(
        kept.error, None,
        "a refused stale read is not the chat's error"
    );
    assert!(
        Arc::ptr_eq(&kept.items[0], &shown.items[0]),
        "the body is the one kept, not one read again"
    );
    handle.set_interest(true, true).unwrap();
    stage_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.join().unwrap();
    assert_eq!(handle.snapshot().error, None);
}

#[test]
fn a_read_refused_after_a_hide_the_chat_has_undone_is_made_again() {
    use super::super::{Item, ItemContent, Role};
    use crate::terminal_ipc::{ProtocolError, ProtocolErrorCode};
    let answer = Arc::new(Item {
        id: "answer".into(),
        role: Role::Assistant,
        content: ItemContent::Text {
            text: "The kept answer.".into(),
            thinking: false,
            phase: Default::default(),
        },
        started_at: None,
        at: None,
    });
    let body = answer.clone();
    let (client, mut server) = UnixStream::pair().unwrap();
    let (stage_tx, stage_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let proof = host_proof("hide-show-race");
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
            codec
                .write(
                    server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: proof.clone(),
                        message,
                    },
                )
                .unwrap();
        };
        let accepted = || ServerMessage::Response {
            response: ServerResponse::Accepted,
        };
        let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        respond(
            &mut codec,
            &mut server,
            Some(connect.request_id),
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        let ClientRequest::ChatStart { session, .. } = start.request else {
            panic!("chat start expected");
        };
        let chat_id = session.id;
        respond(
            &mut codec,
            &mut server,
            Some(start.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatAttached {
                    session,
                    snapshot: Snapshot {
                        generation: 1,
                        ..Default::default()
                    },
                },
            },
        );
        let shown: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            shown.request,
            ClientRequest::ChatVisibility { visible: true, .. }
        ));
        respond(&mut codec, &mut server, Some(shown.request_id), accepted());
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(read.request, ClientRequest::ChatRead { .. }));
        respond(
            &mut codec,
            &mut server,
            Some(read.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: Some(Snapshot {
                        generation: 2,
                        ready: true,
                        items: vec![body],
                        ..Default::default()
                    }),
                },
            },
        );
        // Something changed; the client reads, and hides before the
        // daemon answers, so the daemon refuses the read.
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let raced: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(raced.request, ClientRequest::ChatRead { .. }));
        stage_tx.send(()).unwrap();
        let hidden: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            hidden.request,
            ClientRequest::ChatVisibility { visible: false, .. }
        ));
        respond(&mut codec, &mut server, Some(hidden.request_id), accepted());
        stage_tx.send(()).unwrap();
        // The reader shows the chat again before the refusal arrives.
        let shown: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            shown.request,
            ClientRequest::ChatVisibility { visible: true, .. }
        ));
        respond(&mut codec, &mut server, Some(shown.request_id), accepted());
        respond(
            &mut codec,
            &mut server,
            Some(raced.request_id),
            ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "Hidden chat has no presentation stream; make it visible before reading",
                false,
            )),
        );
        // The refused read is made again, after the body kept.
        let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(
            matches!(
                read.request,
                ClientRequest::ChatRead {
                    after_generation: Some(2),
                    ..
                }
            ),
            "the refused read is made again after the kept body"
        );
        respond(
            &mut codec,
            &mut server,
            Some(read.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: None,
                },
            },
        );
        stage_tx.send(()).unwrap();
    });
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    let handle = ClientHandle::connect_on(
        client,
        omp_card(None, None),
        Intent::Start,
        None,
        false,
        None,
        None,
        None,
        move |wake| {
            let _ = wake_tx.send(wake);
        },
    )
    .unwrap();
    handle.set_interest(true, true).unwrap();
    while handle.resident_snapshot_with_clock().is_none() {
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    stage_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (shown, _) = handle.resident_snapshot_with_clock().unwrap();
    assert_eq!(shown.items[0].content, answer.content);
    // Hidden, keeping its body as a warm chat does, and shown again
    // before the daemon's refusal of the raced read arrives.
    handle.set_interest(false, true).unwrap();
    stage_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    handle.set_interest(true, true).unwrap();
    stage_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.join().unwrap();
    let (kept, _) = handle.resident_snapshot_with_clock().expect("a kept body");
    assert_eq!(
        kept.error, None,
        "a refused raced read is not the chat's error"
    );
    assert!(Arc::ptr_eq(&kept.items[0], &shown.items[0]));
}

#[test]
fn hidden_control_subscribers_advance_metadata_without_rehydration_loops() {
    use super::super::{
        Item, ItemContent, Role,
        presentation::{ItemChange, Presentation},
    };
    let (client, mut server) = UnixStream::pair().unwrap();
    let (advance_tx, advance_rx) = std::sync::mpsc::channel();
    let (stage_tx, stage_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let proof = host_proof("control-reader");
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
            codec
                .write(
                    server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: proof.clone(),
                        message,
                    },
                )
                .unwrap();
        };
        let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        respond(
            &mut codec,
            &mut server,
            Some(connect.request_id),
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        let ClientRequest::ChatStart { session, .. } = start.request else {
            panic!("chat start expected");
        };
        let chat_id = session.id;
        respond(
            &mut codec,
            &mut server,
            Some(start.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatAttached {
                    session,
                    snapshot: Snapshot {
                        generation: 1,
                        ..Default::default()
                    },
                },
            },
        );
        let interested: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            interested.request,
            ClientRequest::ChatVisibility { visible: true, .. }
        ));
        respond(
            &mut codec,
            &mut server,
            Some(interested.request_id),
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
        );
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            read.request,
            ClientRequest::ChatRead {
                after_generation: Some(1),
                ..
            }
        ));
        let prompt = Item {
            id: "received-prompt".into(),
            role: Role::User,
            content: ItemContent::Text {
                text: "prompt".repeat(64 * 1024),
                thinking: false,
                phase: Default::default(),
            },
            started_at: None,
            at: None,
        };
        respond(
            &mut codec,
            &mut server,
            Some(read.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatUpdate {
                    chat_id,
                    update: Presentation {
                        base_generation: 99,
                        file_changes_unchanged: true,
                        metadata: Snapshot {
                            generation: 100,
                            ready: true,
                            ..Default::default()
                        },
                        order: vec![],
                        changes: vec![],
                        children: vec![],
                    },
                },
            },
        );
        let resync: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(
            matches!(
                resync.request,
                ClientRequest::ChatRead {
                    after_generation: None,
                    ..
                }
            ),
            "hidden controls must reject an update with a mismatched metadata base"
        );
        respond(
            &mut codec,
            &mut server,
            Some(resync.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: Some(Snapshot {
                        generation: 1,
                        ..Default::default()
                    }),
                },
            },
        );
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            read.request,
            ClientRequest::ChatRead {
                after_generation: Some(1),
                ..
            }
        ));
        respond(
            &mut codec,
            &mut server,
            Some(read.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatUpdate {
                    chat_id,
                    update: Presentation {
                        base_generation: 1,
                        file_changes_unchanged: true,
                        metadata: Snapshot {
                            generation: 2,
                            ready: true,
                            ..Default::default()
                        },
                        order: vec![prompt.id.clone()],
                        changes: vec![ItemChange::Replace(prompt.clone())],
                        children: vec![],
                    },
                },
            },
        );
        let command: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(&command.request, ClientRequest::ChatCommand {
            command: Command::Prompt { text, .. }, ..
        } if text.len() > 1024 * 1024));
        respond(
            &mut codec,
            &mut server,
            Some(command.request_id),
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
        );
        advance_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let unchanged: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(
            matches!(
                unchanged.request,
                ClientRequest::ChatRead {
                    after_generation: Some(2),
                    ..
                }
            ),
            "discarded bodies must not cause hidden control resync loops"
        );
        respond(
            &mut codec,
            &mut server,
            Some(unchanged.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: None,
                },
            },
        );
        // This ordered control edge lets the client prove it consumed the
        // unchanged response while still hidden before requesting reveal.
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatActivity {
                chat_id,
                state: ActivityState::Working,
                generation: 2,
            },
        );
        stage_tx.send(()).unwrap();
        // The next frame must be reveal, not another automatic full read.
        let shown: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            shown.request,
            ClientRequest::ChatVisibility { visible: true, .. }
        ));
        respond(
            &mut codec,
            &mut server,
            Some(shown.request_id),
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
        );
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            read.request,
            ClientRequest::ChatRead {
                after_generation: None,
                ..
            }
        ));
        respond(
            &mut codec,
            &mut server,
            Some(read.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: Some(Snapshot {
                        generation: 2,
                        ready: true,
                        items: vec![Arc::new(prompt)],
                        ..Default::default()
                    }),
                },
            },
        );
        advance_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    let handle = ClientHandle::connect_on(
        client,
        omp_card(None, None),
        Intent::Start,
        None,
        false,
        None,
        None,
        None,
        move |wake| {
            let _ = wake_tx.send(wake);
        },
    )
    .unwrap();
    {
        let mut wire = handle.inner.wire.lock().unwrap();
        wire.codec
            .write(&mut std::io::sink(), &"large prompt".repeat(128 * 1024))
            .unwrap();
        assert!(wire.codec.buffer_capacities_for_test().1 > 1024 * 1024);
    }
    handle.set_interest(true, false).unwrap();
    while handle.snapshot().generation != 2 {
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle
        .inner
        .wire
        .lock()
        .unwrap()
        .codec
        .buffer_capacities_for_test()
        .1
        > 256 * 1024
    {
        assert!(
            Instant::now() < deadline,
            "hidden interested transport retained its large prompt buffer"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(handle.snapshot().ready);
    assert_eq!(handle.snapshot().transcript_bytes(), 0);
    assert!(handle.has_observed_user_item());
    assert!(handle.resident_snapshot_with_clock().is_none());
    handle
        .send(Command::Prompt {
            text: "large prompt".repeat(128 * 1024),
            attachments: vec![],
            mode: super::super::PromptMode::Send,
        })
        .unwrap();
    assert!(
        handle
            .inner
            .wire
            .lock()
            .unwrap()
            .codec
            .buffer_capacities_for_test()
            .1
            <= 256 * 1024,
        "new large hidden commands release scratch without another visibility transition"
    );
    advance_tx.send(()).unwrap();
    stage_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    while !matches!(
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        Wake::Activity
    ) {}
    handle.set_visible(true).unwrap();
    while handle.resident_snapshot_with_clock().is_none() {
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    assert_eq!(handle.snapshot().generation, 2);
    assert_eq!(handle.snapshot().transcript_bytes(), 6 * 64 * 1024);
    advance_tx.send(()).unwrap();
    worker.join().unwrap();
}

#[test]
fn final_wire_body_respects_frozen_handoff_and_keeps_ordinary_cold_fallback() {
    #[derive(Clone, Copy, Debug)]
    enum Case {
        Visible,
        HiddenBeforeClose,
        HiddenAfterClose,
        FrozenBeforeClose,
        FrozenDuringFinalDerivation,
        NewLiveBodyConsumer,
    }
    for case in [
        Case::Visible,
        Case::HiddenBeforeClose,
        Case::HiddenAfterClose,
        Case::FrozenBeforeClose,
        Case::FrozenDuringFinalDerivation,
        Case::NewLiveBodyConsumer,
    ] {
        let item = |body: &str| {
            Arc::new(super::super::Item {
                id: "final-body".into(),
                role: super::super::Role::Assistant,
                started_at: None,
                at: None,
                content: super::super::ItemContent::Text {
                    text: body.into(),
                    thinking: false,
                    phase: Default::default(),
                },
            })
        };
        let initial = Snapshot {
            generation: 1,
            sampled_at: Some(1_000),
            ready: true,
            items: vec![item("original readable source")],
            ..Default::default()
        };
        // Final identity/control state remains authoritative even when its
        // generation is older and body ownership has moved to the UI.
        let final_snapshot = Snapshot {
            generation: 0,
            session_id: Some("final-session".into()),
            exited: true,
            error: Some("Final close reason".into()),
            items: vec![item("final readable source")],
            ..Default::default()
        };
        let (client, mut server) = UnixStream::pair().unwrap();
        let (send_tx, send_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let proof = host_proof("final-body-policy");
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let respond =
                |codec: &mut FrameCodec, server: &mut UnixStream, request_id, response| {
                    codec
                        .write(
                            server,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id,
                                proof: proof.clone(),
                                message: ServerMessage::Response { response },
                            },
                        )
                        .unwrap();
                };
            let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            respond(
                &mut codec,
                &mut server,
                Some(connect.request_id),
                ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            );
            let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatStart { session, .. } = start.request else {
                panic!("expected start")
            };
            let chat_id = session.id;
            respond(
                &mut codec,
                &mut server,
                Some(start.request_id),
                ServerResponse::ChatAttached {
                    session,
                    snapshot: initial,
                },
            );
            send_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            respond(
                &mut codec,
                &mut server,
                None,
                ServerResponse::ChatClosed {
                    chat_id,
                    snapshot: final_snapshot,
                },
            );
        });
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let handle = ClientHandle::connect_on(
            client,
            omp_card(None, None),
            Intent::Start,
            None,
            false,
            None,
            None,
            None,
            move |wake| {
                let _ = wake_tx.send(wake);
            },
        )
        .unwrap();
        match case {
            Case::HiddenBeforeClose => handle.set_visible(false).unwrap(),
            Case::FrozenBeforeClose => handle.hand_off_source_body().unwrap(),
            Case::NewLiveBodyConsumer => {
                handle.hand_off_source_body().unwrap();
                handle.set_interest(true, true).unwrap();
            }
            _ => {}
        }
        let gate = if matches!(case, Case::FrozenDuringFinalDerivation) {
            let (paused_tx, paused_rx) = std::sync::mpsc::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            REPLACEMENT_GATES.lock().unwrap().insert(
                Arc::as_ptr(&handle.inner.publication) as usize,
                (paused_tx, resume_rx),
            );
            Some((paused_rx, resume_tx))
        } else {
            None
        };
        send_tx.send(()).unwrap();
        if let Some((paused_rx, resume_tx)) = gate {
            paused_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(!handle.is_connected());
            assert!(handle.hand_off_source_body().is_err());
            assert!(!handle.inner.body_requested.load(Ordering::Acquire));
            assert!(handle.resident_snapshot_with_clock().is_none());
            resume_tx.send(()).unwrap();
        }
        while handle.snapshot().generation != 0 {
            wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        worker.join().unwrap();
        assert!(!handle.is_connected());
        if matches!(case, Case::HiddenAfterClose) {
            assert!(handle.set_visible(false).is_err());
        }
        let snapshot = handle.snapshot();
        assert_eq!(
            snapshot.session_id.as_deref(),
            Some("final-session"),
            "{case:?}"
        );
        assert!(snapshot.exited && !snapshot.ready, "{case:?}");
        assert_eq!(
            snapshot.error.as_deref(),
            Some("Final close reason"),
            "{case:?}"
        );
        assert_eq!(handle.snapshot_with_clock().1.unwrap().sampled_at, 1_000);
        let frozen = matches!(
            case,
            Case::FrozenBeforeClose | Case::FrozenDuringFinalDerivation
        );
        assert_eq!(
            handle.resident_snapshot_with_clock().is_none(),
            frozen,
            "{case:?}"
        );
        if frozen {
            assert_eq!(snapshot.transcript_bytes(), 0, "{case:?}");
            assert!(handle.inner.publication.current.load().lineage.is_empty());
        } else {
            // An ordinary cold kept card reveals from this final body;
            // its daemon/actor has already closed and cannot hydrate it.
            let (readable, _) = handle.resident_snapshot_with_clock().unwrap();
            assert!(
                matches!(&readable.items[0].content,
                super::super::ItemContent::Text { text, .. } if text == "final readable source"),
                "{case:?}"
            );
        }
    }
}

fn paused_wire_publication(visibility: Option<bool>) {
    use crate::chat::{
        Item, ItemContent, Role,
        presentation::{ItemChange, Presentation},
    };
    let text = |id: &str, body: &str| Item {
        id: id.into(),
        role: Role::Assistant,
        started_at: None,
        at: None,
        content: ItemContent::Text {
            text: body.into(),
            thinking: false,
            phase: Default::default(),
        },
    };
    let initial = Snapshot {
        generation: 1,
        sampled_at: Some(1_000),
        items: vec![Arc::new(text("live", "hello"))],
        ..Default::default()
    };
    let update = Presentation {
        file_changes_unchanged: false,
        base_generation: 1,
        metadata: Snapshot {
            sampled_at: Some(50_000),
            generation: 2,
            ..Default::default()
        },
        order: vec!["live".into()],
        changes: vec![ItemChange::between(
            Some(&initial.items[0]),
            &text("live", "hello tail"),
        )],
        children: vec![],
    };
    let card = omp_card(None, None);
    let (paused_tx, paused_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    PUBLICATION_GATES
        .lock()
        .unwrap()
        .insert(card.id, (paused_tx, resume_rx));
    let (client, mut server) = UnixStream::pair().unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let (resync_tx, resync_rx) = std::sync::mpsc::channel();
    let (hydrate_tx, hydrate_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let proof = host_proof("live");
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
            codec
                .write(
                    server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: proof.clone(),
                        message,
                    },
                )
                .unwrap();
        };
        let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        respond(
            &mut codec,
            &mut server,
            Some(connect.request_id),
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        let ClientRequest::ChatStart { session, .. } = start.request else {
            panic!("expected new chat")
        };
        let chat_id = session.id;
        respond(
            &mut codec,
            &mut server,
            Some(start.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatAttached {
                    session,
                    snapshot: initial,
                },
            },
        );
        let visible: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            visible.request,
            ClientRequest::ChatVisibility { visible: true, .. }
        ));
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::Response {
                response: ServerResponse::ChatUpdate { chat_id, update },
            },
        );
        if visibility == Some(true) {
            loop {
                let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                match request.request {
                    ClientRequest::ChatVisibility { .. } => {}
                    ClientRequest::ChatRead {
                        after_generation, ..
                    } => {
                        assert_eq!(
                            after_generation, None,
                            "a hide/reveal collision requires unconditional body hydration"
                        );
                        resync_tx.send(()).unwrap();
                        hydrate_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                        respond(
                            &mut codec,
                            &mut server,
                            Some(request.request_id),
                            ServerMessage::Response {
                                response: ServerResponse::ChatSnapshot {
                                    chat_id,
                                    snapshot: Some(Snapshot {
                                        generation: 2,
                                        sampled_at: Some(50_000),
                                        items: vec![Arc::new(text("live", "hello tail"))],
                                        ..Default::default()
                                    }),
                                },
                            },
                        );
                        break;
                    }
                    other => panic!("unexpected request during body resync: {other:?}"),
                }
            }
        }
        done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    let handle = ClientHandle::connect_on(
        client,
        card,
        Intent::Start,
        None,
        false,
        None,
        None,
        None,
        move |wake| {
            let _ = wake_tx.send(wake);
        },
    )
    .unwrap();
    let installed = handle.snapshot_with_clock();
    handle.set_visible(true).unwrap();
    paused_rx.recv_timeout(Duration::from_secs(5)).unwrap();

    // The reader now holds a derived publication it has not installed.
    let (read_tx, read_rx) = std::sync::mpsc::channel();
    let ui = handle.clone();
    let reading = thread::spawn(move || {
        let (snapshot, clock) = ui.snapshot_with_clock();
        let session = ui.session();
        let resident = ui.resident_snapshot_with_clock().unwrap();
        assert!(!ui.known_append(&snapshot.items[0], &snapshot.items[0]));
        let _ = read_tx.send((snapshot, clock, session, ui.snapshot(), resident));
    });
    let (snapshot, clock, session, plain, resident) = read_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("presentation reads must not wait for the connection reader");
    reading.join().unwrap();
    assert!(Arc::ptr_eq(&snapshot, &installed.0));
    assert!(Arc::ptr_eq(&plain, &installed.0));
    assert_eq!(snapshot.generation, 1);
    assert_eq!(clock.unwrap().sampled_at, 1_000);
    assert_eq!(session.id, handle.inner.id);
    assert!(Arc::ptr_eq(&resident.0, &installed.0));
    assert_eq!(resident.1.unwrap().sampled_at, 1_000);
    if let Some(reveal) = visibility {
        // Hide must finish while the source reader is still paused.
        let (hidden_tx, hidden_rx) = std::sync::mpsc::channel();
        let hidden = handle.clone();
        let hiding = thread::spawn(move || {
            hidden.set_visible(false).unwrap();
            if reveal {
                hidden.set_visible(true).unwrap();
            }
            hidden_tx.send(()).unwrap();
        });
        hidden_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("hide must not wait for body derivation");
        hiding.join().unwrap();
        assert_eq!(handle.snapshot().transcript_bytes(), 0);
        assert_eq!(handle.snapshot_with_clock().1.unwrap().sampled_at, 1_000);
        assert!(handle.resident_snapshot_with_clock().is_none());
    }
    resume_tx.send(()).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while handle.snapshot().generation != 2 {
        assert!(
            Instant::now() < until,
            "derived publication was not committed"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let mut metadata_clock = None;
    if visibility.is_some() {
        let state = handle.inner.publication.current.load_full();
        assert!(
            !state.resident,
            "a pre-hide derived body must not be installed after hide or hide/reveal"
        );
        assert_eq!(state.snapshot.transcript_bytes(), 0);
        assert!(
            state.lineage.is_empty(),
            "hidden metadata cannot authorize an append across eviction"
        );
        assert_eq!(state.clock.unwrap().sampled_at, 50_000);
        metadata_clock = state.clock;
        if visibility == Some(true) {
            resync_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(handle.resident_snapshot_with_clock().is_none());
            hydrate_tx.send(()).unwrap();
            while handle.resident_snapshot_with_clock().is_none() {
                wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        }
    }
    let (published, clock) = handle.snapshot_with_clock();
    if visibility != Some(false) {
        assert!(
            matches!(&published.items[0].content, ItemContent::Text { text, .. } if text == "hello tail")
        );
        assert_eq!(
            handle.known_append(&installed.0.items[0], &published.items[0]),
            visibility.is_none()
        );
    }
    assert_eq!(published.sampled_at, Some(50_000));
    let clock = clock.unwrap();
    if visibility == Some(true) {
        let metadata_clock = metadata_clock.unwrap();
        assert!(clock.received_at >= metadata_clock.received_at);
        assert!(clock.at(clock.received_at) >= metadata_clock.at(clock.received_at));
    } else {
        assert_eq!(clock.sampled_at, 50_000);
    }
    done_tx.send(()).unwrap();
    worker.join().unwrap();
}
#[test]
fn readers_keep_the_installed_publication_while_the_next_is_built() {
    paused_wire_publication(None);
}

#[test]
fn hide_while_body_is_derived_commits_only_metadata() {
    paused_wire_publication(Some(false));
}

#[test]
fn hide_then_reveal_while_body_is_derived_requires_fresh_hydration() {
    paused_wire_publication(Some(true));
}

#[test]
fn second_hide_reveal_fences_full_hydration_derived_from_metadata() {
    use crate::chat::{Item, ItemContent, Role};
    let text = |body: &str| {
        Arc::new(Item {
            id: "live".into(),
            role: Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: Default::default(),
            },
        })
    };
    let card = omp_card(None, None);
    let (client, mut server) = UnixStream::pair().unwrap();
    let (first_read_tx, first_read_rx) = std::sync::mpsc::channel();
    let (send_full_tx, send_full_rx) = std::sync::mpsc::channel();
    let (fresh_read_tx, fresh_read_rx) = std::sync::mpsc::channel();
    let (hydrate_tx, hydrate_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let proof = host_proof("live");
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
            codec
                .write(
                    server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: proof.clone(),
                        message,
                    },
                )
                .unwrap();
        };
        let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        respond(
            &mut codec,
            &mut server,
            Some(connect.request_id),
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        let ClientRequest::ChatStart { session, .. } = start.request else {
            panic!("expected start")
        };
        let chat_id = session.id;
        respond(
            &mut codec,
            &mut server,
            Some(start.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatAttached {
                    session,
                    snapshot: Snapshot {
                        generation: 1,
                        sampled_at: Some(1_000),
                        items: vec![text("initial")],
                        ..Default::default()
                    },
                },
            },
        );
        // The first reveal requests a full body from nonresident metadata.
        loop {
            let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            match request.request {
                ClientRequest::ChatVisibility { visible: true, .. } => {
                    respond(
                        &mut codec,
                        &mut server,
                        None,
                        ServerMessage::ChatChanged { chat_id },
                    );
                }
                ClientRequest::ChatVisibility { visible: false, .. } => {}
                ClientRequest::ChatRead {
                    after_generation, ..
                } => {
                    assert_eq!(after_generation, None);
                    first_read_tx.send(()).unwrap();
                    send_full_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    respond(
                        &mut codec,
                        &mut server,
                        Some(request.request_id),
                        ServerMessage::Response {
                            response: ServerResponse::ChatSnapshot {
                                chat_id,
                                snapshot: Some(Snapshot {
                                    generation: 2,
                                    sampled_at: Some(50_000),
                                    items: vec![text("stale pre-hide body")],
                                    ..Default::default()
                                }),
                            },
                        },
                    );
                    break;
                }
                other => panic!("unexpected first hydration request: {other:?}"),
            }
        }
        loop {
            let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            match request.request {
                ClientRequest::ChatVisibility { .. } => {}
                ClientRequest::ChatRead {
                    after_generation, ..
                } => {
                    assert_eq!(
                        after_generation, None,
                        "second hide/show must invalidate the full proposal even when metadata identity is unchanged"
                    );
                    fresh_read_tx.send(()).unwrap();
                    hydrate_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    respond(
                        &mut codec,
                        &mut server,
                        Some(request.request_id),
                        ServerMessage::Response {
                            response: ServerResponse::ChatSnapshot {
                                chat_id,
                                snapshot: Some(Snapshot {
                                    generation: 2,
                                    sampled_at: Some(60_000),
                                    items: vec![text("fresh authorized body")],
                                    ..Default::default()
                                }),
                            },
                        },
                    );
                    break;
                }
                other => panic!("unexpected fresh hydration request: {other:?}"),
            }
        }
        done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    let handle = ClientHandle::connect_on(
        client,
        card,
        Intent::Start,
        None,
        false,
        None,
        None,
        None,
        move |wake| {
            let _ = wake_tx.send(wake);
        },
    )
    .unwrap();
    let old = handle.snapshot();
    handle.set_visible(false).unwrap();
    assert!(handle.resident_snapshot_with_clock().is_none());
    handle.set_visible(true).unwrap();
    first_read_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (paused_tx, paused_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    REPLACEMENT_GATES.lock().unwrap().insert(
        Arc::as_ptr(&handle.inner.publication) as usize,
        (paused_tx, resume_rx),
    );
    send_full_tx.send(()).unwrap();
    paused_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let metadata = handle.inner.publication.current.load_full();
    assert!(!metadata.resident && metadata.hidden.is_none());
    // This is the ABA case: second hide/show changes only body policy,
    // retaining the identical nonresident Publication pointer.
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let hidden = handle.clone();
    let hiding = thread::spawn(move || {
        hidden.set_visible(false).unwrap();
        hidden.set_visible(true).unwrap();
        finished_tx.send(()).unwrap();
    });
    finished_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("visibility must not wait for full hydration derivation");
    hiding.join().unwrap();
    assert!(Arc::ptr_eq(
        &metadata,
        &handle.inner.publication.current.load_full()
    ));
    assert!(handle.resident_snapshot_with_clock().is_none());
    assert_eq!(handle.snapshot_with_clock().1.unwrap().sampled_at, 1_000);
    resume_tx.send(()).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while handle.snapshot().generation != 2 {
        assert!(
            Instant::now() < until,
            "full response proposal did not commit"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let fallback = handle.inner.publication.current.load_full();
    assert!(
        !fallback.resident,
        "a second hide/show must reject the pre-hide full hydration proposal"
    );
    assert_eq!(fallback.snapshot.generation, 2);
    assert_eq!(fallback.clock.unwrap().sampled_at, 50_000);
    assert_eq!(fallback.snapshot.transcript_bytes(), 0);
    assert!(fallback.lineage.is_empty());
    fresh_read_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        handle.resident_snapshot_with_clock().is_none(),
        "pre-hide full response was never admitted"
    );
    hydrate_tx.send(()).unwrap();
    while handle.resident_snapshot_with_clock().is_none() {
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    let (snapshot, clock) = handle.snapshot_with_clock();
    assert_eq!(snapshot.generation, 2);
    assert_eq!(clock.unwrap().sampled_at, 60_000);
    assert!(
        matches!(&snapshot.items[0].content, ItemContent::Text { text, .. } if text == "fresh authorized body")
    );
    assert!(!handle.known_append(&old.items[0], &snapshot.items[0]));
    done_tx.send(()).unwrap();
    worker.join().unwrap();
}

#[test]
fn wire_updates_share_retained_items_and_request_full_resync_on_invalid_patch() {
    use crate::chat::{
        Item, ItemContent, Role,
        presentation::{ItemChange, Presentation},
    };
    let text = |id: &str, body: &str| Item {
        id: id.into(),
        role: Role::Assistant,
        started_at: None,
        at: None,
        content: ItemContent::Text {
            text: body.into(),
            thinking: false,
            phase: Default::default(),
        },
    };
    let initial = Snapshot {
        sampled_at: Some(1000),
        generation: 1,
        items: vec![
            Arc::new(text("fixed", &"x".repeat(128 * 1024))),
            Arc::new(text("live", "hello")),
        ],
        ..Default::default()
    };
    let update = Presentation {
        file_changes_unchanged: false,
        base_generation: 1,
        metadata: Snapshot {
            sampled_at: Some(2000),
            generation: 2,
            ..Default::default()
        },
        order: vec!["fixed".into(), "live".into()],
        changes: vec![ItemChange::between(
            Some(&initial.items[1]),
            &text("live", "hello tail"),
        )],
        children: vec![],
    };
    let full = Snapshot {
        generation: 4,
        items: vec![Arc::new(text("live", "resynchronized"))],
        ..Default::default()
    };
    let (client, mut server) = UnixStream::pair().unwrap();
    let (advance_tx, advance_rx) = std::sync::mpsc::channel();
    let (resync_tx, resync_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let proof = host_proof("live");
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
            codec
                .write(
                    server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: proof.clone(),
                        message,
                    },
                )
                .unwrap();
        };
        let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        respond(
            &mut codec,
            &mut server,
            Some(connect.request_id),
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        let ClientRequest::ChatStart { session, .. } = start.request else {
            panic!("expected new chat")
        };
        let chat_id = session.id;
        respond(
            &mut codec,
            &mut server,
            Some(start.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatAttached {
                    session,
                    snapshot: initial,
                },
            },
        );
        let visible: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            visible.request,
            ClientRequest::ChatVisibility { visible: true, .. }
        ));
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::Response {
                response: ServerResponse::ChatUpdate { chat_id, update },
            },
        );
        advance_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let malformed = Presentation {
            file_changes_unchanged: false,
            base_generation: 2,
            metadata: Snapshot {
                sampled_at: Some(999_999),
                generation: 3,
                subagents: vec![super::super::Subagent {
                    model: None,
                    waiting: false,
                    detail: String::new(),
                    id: "missing-child-patch".into(),
                    kind: "review".into(),
                    description: String::new(),
                    status: super::super::ToolStatus::Running,
                    background: false,
                    items: vec![],
                }],
                ..Default::default()
            },
            order: vec!["fixed".into(), "live".into()],
            changes: vec![ItemChange::between(
                Some(&text("live", "hello tail")),
                &text("live", "hello tail corrupt"),
            )],
            // Parent work is valid, then child validation rejects the
            // entire publication; no partial parent or clock may escape.
            children: vec![],
        };
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::Response {
                response: ServerResponse::ChatUpdate {
                    chat_id,
                    update: malformed,
                },
            },
        );
        let resync: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(
            matches!(
                resync.request,
                ClientRequest::ChatRead {
                    after_generation: None,
                    ..
                }
            ),
            "invalid patch must request an unconditional snapshot"
        );
        resync_tx.send(()).unwrap();
        advance_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        respond(
            &mut codec,
            &mut server,
            Some(resync.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: Some(full),
                },
            },
        );
        advance_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    let handle = ClientHandle::connect_on(
        client,
        omp_card(None, None),
        Intent::Start,
        None,
        false,
        None,
        None,
        None,
        move |wake| {
            let _ = wake_tx.send(wake);
        },
    )
    .unwrap();
    let retained = handle.snapshot();
    handle.set_visible(true).unwrap();
    while handle.snapshot().generation != 2 {
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    let updated = handle.snapshot();
    let clock = handle.snapshot_with_clock().1.unwrap();
    assert!(Arc::ptr_eq(&retained.items[0], &updated.items[0]));
    assert!(
        matches!(&retained.items[1].content, ItemContent::Text { text, .. } if text == "hello")
    );
    assert!(
        matches!(&updated.items[1].content, ItemContent::Text { text, .. } if text == "hello tail")
    );
    advance_tx.send(()).unwrap();
    resync_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (unchanged, unchanged_clock) = handle.snapshot_with_clock();
    assert!(
        Arc::ptr_eq(&updated, &unchanged),
        "rejected update must preserve installed publication identity"
    );
    assert_eq!(unchanged.generation, 2);
    assert_eq!(unchanged_clock.unwrap().sampled_at, clock.sampled_at);
    assert_eq!(unchanged_clock.unwrap().received_at, clock.received_at);
    advance_tx.send(()).unwrap();
    while handle.snapshot().generation != 4 {
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    assert!(
        matches!(&handle.snapshot().items[0].content, ItemContent::Text { text, .. } if text == "resynchronized")
    );
    advance_tx.send(()).unwrap();
    worker.join().unwrap();
}

fn text_copy_source(token: u64, bytes: u64) -> super::super::TextCopySource {
    super::super::TextCopySource {
        cache: uuid::Uuid::new_v4(),
        scope: super::super::TextCopyScope::Subagent { id: "child".into() },
        token,
        revision: 7,
        bytes,
    }
}

#[test]
fn text_copy_pages_require_exact_identity_contiguous_bytes_and_completion() {
    let source = text_copy_source(1, 6);
    let first = super::super::TextCopyPage {
        source: source.clone(),
        offset: 0,
        text: "猫".into(),
        done: false,
    };
    let mut text = String::new();
    append_text_copy_page(&source, &mut text, first.clone()).unwrap();
    assert_eq!(text, "猫");
    let last = super::super::TextCopyPage {
        source: source.clone(),
        offset: 3,
        text: "犬".into(),
        done: true,
    };
    let mut bad_pages = vec![first]; // Duplicate offset.
    let mut changed = last.clone();
    changed.offset = 4;
    bad_pages.push(changed);
    let mut changed = last.clone();
    changed.source.cache = uuid::Uuid::new_v4();
    bad_pages.push(changed);
    let mut changed = last.clone();
    changed.source.scope = super::super::TextCopyScope::Conversation;
    bad_pages.push(changed);
    let mut changed = last.clone();
    changed.source.token += 1;
    bad_pages.push(changed);
    let mut changed = last.clone();
    changed.source.revision += 1;
    bad_pages.push(changed);
    let mut changed = last.clone();
    changed.source.bytes += 1;
    bad_pages.push(changed);
    let mut changed = last.clone();
    changed.done = false;
    bad_pages.push(changed);
    let mut changed = last.clone();
    changed.text.clear();
    changed.done = false;
    bad_pages.push(changed);
    let mut changed = last.clone();
    changed.text = "longer".into();
    bad_pages.push(changed);
    let mut changed = last.clone();
    changed.text = "x".into();
    bad_pages.push(changed);
    for page in bad_pages {
        assert!(append_text_copy_page(&source, &mut text, page).is_err());
        assert_eq!(text, "猫", "invalid pages must not publish partial data");
    }
    append_text_copy_page(&source, &mut text, last).unwrap();
    assert_eq!(text, "猫犬");

    let large = text_copy_source(2, (super::super::TEXT_COPY_PAGE_BYTES + 1) as u64);
    assert!(
        append_text_copy_page(
            &large,
            &mut String::new(),
            super::super::TextCopyPage {
                source: large.clone(),
                offset: 0,
                text: "x".repeat(super::super::TEXT_COPY_PAGE_BYTES + 1),
                done: true,
            }
        )
        .is_err()
    );
    let empty = text_copy_source(3, 0);
    append_text_copy_page(
        &empty,
        &mut String::new(),
        super::super::TextCopyPage {
            source: empty.clone(),
            offset: 0,
            text: String::new(),
            done: true,
        },
    )
    .unwrap();
    let over_limit = text_copy_source(4, super::super::TEXT_COPY_LIMIT_BYTES as u64 + 1);
    assert!(
        append_text_copy_page(
            &over_limit,
            &mut String::new(),
            super::super::TextCopyPage {
                source: over_limit.clone(),
                offset: 0,
                text: String::new(),
                done: false,
            }
        )
        .is_err()
    );
    assert!(text_copy_remaining(Instant::now()).is_err());
}

#[test]
fn text_copy_replies_refuse_other_chat_and_other_response_types() {
    let chat_id = TerminalId::new();
    let source = text_copy_source(1, 0);
    let message = ServerMessage::Response {
        response: ServerResponse::ChatTextCopy {
            chat_id: TerminalId::new(),
            page: super::super::TextCopyPage {
                source,
                offset: 0,
                text: String::new(),
                done: true,
            },
        },
    };
    assert!(text_copy_response(chat_id, message).is_err());
    assert!(
        text_copy_response(
            chat_id,
            ServerMessage::Response {
                response: ServerResponse::Accepted
            }
        )
        .is_err()
    );
}

#[test]
fn text_copy_pending_requests_fail_on_disconnect_and_explicit_close() {
    for explicit_close in [false, true] {
        let (client, mut server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let proof = host_proof("text-copy-cleanup-host");
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let respond =
                |codec: &mut FrameCodec, server: &mut UnixStream, request_id, response| {
                    codec
                        .write(
                            server,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id,
                                proof: proof.clone(),
                                message: ServerMessage::Response { response },
                            },
                        )
                        .unwrap();
                };
            let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            respond(
                &mut codec,
                &mut server,
                Some(connect.request_id),
                ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            );
            let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatStart { session, .. } = start.request else {
                panic!("start expected")
            };
            let chat_id = session.id;
            respond(
                &mut codec,
                &mut server,
                Some(start.request_id),
                ServerResponse::ChatAttached {
                    session,
                    snapshot: Snapshot::default(),
                },
            );
            let copy: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                copy.request,
                ClientRequest::ChatCommand {
                    command: Command::ReadTextCopy { .. },
                    ..
                }
            ));
            if explicit_close {
                respond(
                    &mut codec,
                    &mut server,
                    None,
                    ServerResponse::ChatClosed {
                        chat_id,
                        snapshot: Snapshot {
                            exited: true,
                            ..Default::default()
                        },
                    },
                );
            }
            // EOF otherwise exercises the reader's disconnect drain.
        });
        let client = ClientHandle::connect_on(
            client,
            omp_card(None, None),
            Intent::Start,
            None,
            false,
            None,
            None,
            None,
            |_| {},
        )
        .unwrap();
        let error = client.copy_text(text_copy_source(1, 100)).unwrap_err();
        assert!(
            error.contains(if explicit_close {
                "explicitly closed"
            } else {
                "disconnected"
            }),
            "{error}"
        );
        assert!(client.inner.text_copy_pending.lock().unwrap().is_empty());
        assert_eq!(client.inner.text_copies.load(Ordering::Acquire), 0);
        worker.join().unwrap();
    }
}

#[test]
fn text_copy_expired_deadline_does_not_wait_for_writer_or_send_a_frame() {
    let (stream, _server) = UnixStream::pair().unwrap();
    let wire = Mutex::new(Wire {
        stream,
        codec: FrameCodec::new(ipc::MAX_CLIENT_FRAME_BYTES),
        body_requested: Arc::new(AtomicBool::new(true)),
        announced: false,
        hidings: 0,
    });
    let _held = wire.lock().unwrap();
    let error = transmit_text_copy(
        &wire,
        &host_proof("expired-text-copy"),
        1,
        ClientRequest::ChatCommand {
            chat_id: TerminalId::new(),
            command: Command::ReadTextCopy {
                source: text_copy_source(1, 0),
                offset: 0,
            },
        },
        Instant::now(),
    )
    .unwrap_err();
    assert!(error.contains("timed out"));
}

#[test]
fn text_copy_wire_is_request_specific_bounded_and_pages_exact_raw_utf8() {
    let mut clients = Vec::new();
    let mut servers = Vec::new();
    let mut ready = Vec::new();
    let mut release = Vec::new();
    let mut finish = Vec::new();
    for client_index in 0..2_u64 {
        let (client, mut server) = UnixStream::pair().unwrap();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let (finish_tx, finish_rx) = crossbeam_channel::bounded(1);
        ready.push(ready_rx);
        release.push(release_tx);
        finish.push(finish_tx);
        servers.push(thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let proof = host_proof("text-copy-host");
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let respond =
                |codec: &mut FrameCodec, server: &mut UnixStream, request_id, response| {
                    codec
                        .write(
                            server,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id,
                                proof: proof.clone(),
                                message: ServerMessage::Response { response },
                            },
                        )
                        .unwrap();
                };
            let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            respond(
                &mut codec,
                &mut server,
                Some(connect.request_id),
                ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            );
            let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatStart { session, .. } = start.request else {
                panic!("start expected")
            };
            let chat_id = session.id;
            respond(
                &mut codec,
                &mut server,
                Some(start.request_id),
                ServerResponse::ChatAttached {
                    session,
                    snapshot: Snapshot {
                        generation: 1,
                        ..Default::default()
                    },
                },
            );
            for round in 0..2 {
                let first: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                let second: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                if round == 0 {
                    ready_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                // Coalesced view updates cannot consume requester-specific pages.
                respond(
                    &mut codec,
                    &mut server,
                    None,
                    ServerResponse::ChatSnapshot {
                        chat_id,
                        snapshot: Some(Snapshot {
                            generation: round + 2,
                            ..Default::default()
                        }),
                    },
                );
                for request in [second, first] {
                    let ClientRequest::ChatCommand {
                        chat_id: requested_chat,
                        command: Command::ReadTextCopy { source, offset },
                    } = request.request
                    else {
                        panic!("copy page expected")
                    };
                    assert_eq!(requested_chat, chat_id);
                    assert_eq!(source.token / 10, client_index);
                    let text = if round == 0 {
                        assert_eq!(offset, 0);
                        if source.token % 10 == 1 { "a" } else { "b" }
                            .repeat(super::super::TEXT_COPY_PAGE_BYTES)
                    } else {
                        assert_eq!(offset, super::super::TEXT_COPY_PAGE_BYTES as u64);
                        if source.token % 10 == 1 { "一" } else { "二" }.into()
                    };
                    respond(
                        &mut codec,
                        &mut server,
                        Some(request.request_id),
                        ServerResponse::ChatTextCopy {
                            chat_id,
                            page: super::super::TextCopyPage {
                                source,
                                offset,
                                text,
                                done: round == 1,
                            },
                        },
                    );
                }
            }
            finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }));
        clients.push(
            ClientHandle::connect_on(
                client,
                omp_card(None, None),
                Intent::Start,
                None,
                false,
                None,
                None,
                None,
                |_| {},
            )
            .unwrap(),
        );
    }
    let mut requests = Vec::new();
    for (index, client) in clients.iter().enumerate() {
        for token in [1, 2] {
            let client = client.clone();
            requests.push((
                token,
                thread::spawn(move || {
                    client.copy_text(text_copy_source(
                        index as u64 * 10 + token,
                        super::super::TEXT_COPY_PAGE_BYTES as u64 + 3,
                    ))
                }),
            ));
        }
    }
    for event in ready {
        event.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    for client in &clients {
        assert!(
            client
                .copy_text(text_copy_source(99, 0))
                .unwrap_err()
                .contains("capacity")
        );
        assert_eq!(client.inner.text_copy_pending.lock().unwrap().len(), 2);
    }
    for event in release {
        event.send(()).unwrap();
    }
    for (token, request) in requests {
        let expected = format!(
            "{}{}",
            if token == 1 { "a" } else { "b" }.repeat(super::super::TEXT_COPY_PAGE_BYTES),
            if token == 1 { "一" } else { "二" }
        );
        assert_eq!(request.join().unwrap().unwrap(), expected);
    }
    for client in &clients {
        assert!(client.inner.text_copy_pending.lock().unwrap().is_empty());
        assert_eq!(client.inner.text_copies.load(Ordering::Acquire), 0);
    }
    for event in finish {
        event.send(()).unwrap();
    }
    for server in servers {
        server.join().unwrap();
    }
}

#[test]
fn file_changes_replies_are_request_specific_across_clients_and_coalesced_updates() {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let mut releases = Vec::new();
    let mut handles = Vec::new();
    let mut servers = Vec::new();
    let mut finish = Vec::new();
    for client_index in 0..2_u64 {
        let (client, mut server) = UnixStream::pair().unwrap();
        let ready_tx = ready_tx.clone();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        releases.push(release_tx);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        finish.push(done_tx);
        servers.push(thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let proof = host_proof("file-query-host");
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let respond =
                |codec: &mut FrameCodec, server: &mut UnixStream, request_id, response| {
                    codec
                        .write(
                            server,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id,
                                proof: proof.clone(),
                                message: ServerMessage::Response { response },
                            },
                        )
                        .unwrap()
                };
            let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            respond(
                &mut codec,
                &mut server,
                Some(connect.request_id),
                ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            );
            let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatStart { session, .. } = start.request else {
                panic!("start expected")
            };
            let chat_id = session.id;
            respond(
                &mut codec,
                &mut server,
                Some(start.request_id),
                ServerResponse::ChatAttached {
                    session,
                    snapshot: Snapshot {
                        generation: 1,
                        busy: true,
                        read_only_reason: Some("cached fixture".into()),
                        ..Default::default()
                    },
                },
            );
            let first: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let second: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            // Watch publications intentionally coalesce before either query
            // reply; immutable results still return through their request IDs.
            for generation in [2, 3] {
                respond(
                    &mut codec,
                    &mut server,
                    None,
                    ServerResponse::ChatSnapshot {
                        chat_id,
                        snapshot: Some(Snapshot {
                            generation,
                            ..Default::default()
                        }),
                    },
                );
            }
            for request in [second, first] {
                let ClientRequest::ChatCommand { command, .. } = request.request else {
                    panic!("cache query expected")
                };
                let response = match command {
                    Command::LoadFileChanges { request_id, .. } => {
                        ServerResponse::ChatFileChanges {
                            chat_id,
                            summary: super::super::FileChangesSummary {
                                request_id,
                                total_files: client_index as usize + 1,
                                ..Default::default()
                            },
                        }
                    }
                    Command::LoadFileChangePatches {
                        request_id,
                        key,
                        cursor,
                        ..
                    } => ServerResponse::ChatFileChangePatches {
                        chat_id,
                        page: super::super::FileChangePage {
                            request_id,
                            key,
                            revision: 4,
                            patches: vec![],
                            next: cursor,
                            error: None,
                        },
                    },
                    _ => panic!("unexpected provider command"),
                };
                respond(&mut codec, &mut server, Some(request.request_id), response);
            }
            done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }));
        handles.push(
            ClientHandle::connect_on(
                client,
                omp_card(None, None),
                Intent::Start,
                None,
                false,
                None,
                None,
                None,
                |_| {},
            )
            .unwrap(),
        );
    }
    for handle in &handles {
        assert!(
            handle
                .file_changes(Command::Interrupt)
                .unwrap_err()
                .contains("Expected a file changes cache query")
        );
    }
    let mut queries = Vec::new();
    for (client_index, handle) in handles.iter().enumerate() {
        let a = handle.clone();
        queries.push(thread::spawn(move || {
            match a
                .file_changes(Command::LoadFileChanges {
                    request_id: 100 + client_index as u64,
                    after: None,
                })
                .unwrap()
            {
                super::super::FileChangeReply::Files(summary) => {
                    assert_eq!(summary.request_id, 100 + client_index as u64);
                    assert_eq!(summary.total_files, client_index + 1);
                }
                _ => panic!("wrong query reply"),
            }
        }));
        let b = handle.clone();
        queries.push(thread::spawn(move || {
            let key = super::super::FileChangeKey {
                source: super::super::FileChangeSource::Conversation,
                path: format!("file{client_index}.rs"),
            };
            let cursor = super::super::FilePatchCursor {
                ordinal: 9,
                file_index: 0,
                offset: client_index + 1,
            };
            match b
                .file_changes(Command::LoadFileChangePatches {
                    request_id: 200 + client_index as u64,
                    key: key.clone(),
                    cursor: Some(cursor.clone()),
                    revision: 4,
                })
                .unwrap()
            {
                super::super::FileChangeReply::Patches(page) => {
                    assert_eq!(page.request_id, 200 + client_index as u64);
                    assert_eq!(page.key, key);
                    assert_eq!(page.next, Some(cursor));
                }
                _ => panic!("wrong query reply"),
            }
        }));
    }
    for _ in 0..2 {
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    for release in releases {
        release.send(()).unwrap();
    }
    for query in queries {
        query.join().unwrap();
    }
    for done in finish {
        done.send(()).unwrap();
    }
    for server in servers {
        server.join().unwrap();
    }
}
#[test]
fn retained_claude_pre_id_restore_records_authenticated_writer_lineage() {
    for learned in [false, true] {
        let mut source = omp_card(None, Some(host_proof("prior")));
        source.launch.provider = crate::agent::Provider::Claude;
        source.launch.recipe.program = "claude".into();
        source.native_continuity =
            super::super::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        let mut resolved = source.clone();
        resolved.proof = Some(host_proof("live"));
        resolved.launch.session_id = learned.then(|| CONVERSATION.into());
        let handle = restore_with_lineage(source.clone(), Some(resolved.clone()), |request| {
            let ClientRequest::ChatAttach { session } = request else {
                panic!("retained recovery must attach without starting");
            };
            let snapshot = Snapshot {
                session_id: session.launch.session_id.clone(),
                ..Default::default()
            };
            ServerMessage::Response {
                response: ServerResponse::ChatAttached { session, snapshot },
            }
        })
        .unwrap();
        assert_eq!(handle.upgrade_recovery(), Some(&(source, resolved)));
    }
}
#[test]
fn unavailable_presentation_disables_readiness_but_preserves_body_and_copy_authority() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let (released, release) = std::sync::mpsc::channel();
    let (changed, changes) = std::sync::mpsc::channel();
    let session = omp_card(Some(CONVERSATION), Some(host_proof("presentation")));
    let source = session.clone();
    let body = Arc::new(super::super::Item {
        id: "retained-body".into(),
        role: super::super::Role::Assistant,
        content: super::super::ItemContent::Text {
            text: "Preserved private answer λ".into(),
            thinking: false,
            phase: Default::default(),
        },
        started_at: None,
        at: None,
    });
    let initial = Snapshot {
        generation: 7,
        ready: true,
        session_id: source.launch.session_id.clone(),
        items: vec![body.clone()],
        text_copy_sources: super::super::TextCopySources {
            conversation_revision: 3,
            conversation: vec![Some(text_copy_source(11, 28))],
            ..Default::default()
        },
        ..Default::default()
    };
    let before = initial.clone();
    let worker = thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let proof = host_proof("presentation");
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
            codec
                .write(
                    server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: proof.clone(),
                        message,
                    },
                )
                .unwrap();
        };
        let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        respond(
            &mut codec,
            &mut server,
            Some(connect.request_id),
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(start.request, ClientRequest::ChatStart { .. }));
        respond(
            &mut codec,
            &mut server,
            Some(start.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatAttached {
                    session,
                    snapshot: initial,
                },
            },
        );
        let command: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(command.request, ClientRequest::ChatCommand { .. }));
        respond(
            &mut codec,
            &mut server,
            Some(command.request_id),
            ServerMessage::Error(ipc::ProtocolError::new(
                ipc::ProtocolErrorCode::TerminalUnavailable,
                "one command was refused",
                true,
            )),
        );
        let visible: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            visible.request,
            ClientRequest::ChatVisibility { visible: true, .. }
        ));
        respond(
            &mut codec,
            &mut server,
            Some(visible.request_id),
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
        );
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id: source.id },
        );
        let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            read.request,
            ClientRequest::ChatRead {
                after_generation: Some(7),
                ..
            }
        ));
        respond(
            &mut codec,
            &mut server,
            Some(read.request_id),
            ServerMessage::Error(ipc::ProtocolError::new(
                ipc::ProtocolErrorCode::TerminalUnavailable,
                "retained presentation is unavailable",
                true,
            )),
        );
        release.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let handle = ClientHandle::connect_on(
        client,
        source.clone(),
        Intent::Start,
        None,
        false,
        None,
        None,
        None,
        move |wake| {
            if matches!(wake, Wake::Changed) {
                let _ = changed.send(());
            }
        },
    )
    .unwrap();
    let decoded_body = handle.snapshot().items[0].clone();
    assert_eq!(
        handle.send(Command::Refresh).unwrap_err(),
        "one command was refused"
    );
    assert!(
        handle.snapshot().ready,
        "request-specific refusal must not disable presentation"
    );
    assert_eq!(handle.snapshot().error, None);
    handle.set_visible(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let after = loop {
        let snapshot = handle.snapshot();
        if !snapshot.ready
            && snapshot.error.as_deref() == Some("retained presentation is unavailable")
        {
            break snapshot;
        }
        changes
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
    };
    assert!(!after.ready);
    assert_eq!(
        after.error.as_deref(),
        Some("retained presentation is unavailable")
    );
    assert_eq!(after.generation, before.generation);
    assert_eq!(after.items, before.items);
    assert!(Arc::ptr_eq(&after.items[0], &decoded_body));
    assert_eq!(after.text_copy_sources, before.text_copy_sources);
    assert_eq!(after.session_id, before.session_id);
    assert_eq!(after.exited, before.exited);
    assert_eq!(handle.session(), source);
    assert!(handle.inner.alive.load(Ordering::Acquire));
    released.send(()).unwrap();
    worker.join().unwrap();
}

// Test-only injected thread-factory failure, scoped to an exact fresh client.
// It exercises the real fallible empty-owner startup path before handshake.
static FAIL_RETIREMENT_START: Mutex<std::collections::BTreeSet<TerminalId>> =
    Mutex::new(std::collections::BTreeSet::new());

pub(super) fn fail_retirement_start(id: TerminalId) -> bool {
    FAIL_RETIREMENT_START
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&id)
}

fn retirement_text(body: String) -> Arc<super::super::Item> {
    Arc::new(super::super::Item {
        id: "retirement-source".into(),
        role: super::super::Role::Assistant,
        started_at: None,
        at: None,
        content: super::super::ItemContent::Text {
            text: body,
            thinking: false,
            phase: Default::default(),
        },
    })
}

/// An actual existing FrameCodec/UnixStream connection; no provider fixture,
/// substitute JoinHandle, fabricated lifecycle or synthetic body ownership.
fn retirement_wire(
    session: Session,
    initial: Snapshot,
    update: Option<super::super::presentation::Presentation>,
    wake: impl Fn(Wake) + Send + Sync + 'static,
) -> (ClientHandle, JoinHandle<()>, std::sync::mpsc::Receiver<()>) {
    let (client, mut server) = UnixStream::pair().unwrap();
    let (eof, observed_eof) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        handoff_handshake(&mut server, &mut codec);
        let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        let ClientRequest::ChatStart { session, .. } = request.request else {
            panic!("expected actual ChatStart handshake")
        };
        let chat_id = session.id;
        handoff_reply(
            &mut server,
            &mut codec,
            Some(request.request_id),
            &handoff_proof(),
            ServerMessage::Response {
                response: ServerResponse::ChatAttached {
                    session,
                    snapshot: initial,
                },
            },
        );
        if let Some(update) = update {
            let visible: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                visible.request,
                ClientRequest::ChatVisibility { visible: true, .. }
            ));
            handoff_reply(
                &mut server,
                &mut codec,
                None,
                &handoff_proof(),
                ServerMessage::Response {
                    response: ServerResponse::ChatUpdate { chat_id, update },
                },
            );
            let end: Option<ClientEnvelope> = codec.read(&mut server).unwrap();
            assert!(
                end.is_none(),
                "Drop shutdown must close the real connection while its reader is paused"
            );
            eof.send(()).unwrap();
        }
    });
    let client = ClientHandle::connect_on(
        client,
        session,
        Intent::Start,
        None,
        false,
        None,
        None,
        None,
        wake,
    )
    .unwrap();
    (client, worker, observed_eof)
}

#[test]
fn dropping_a_paused_client_hands_off_both_joins_without_blocking_other_retirements() {
    use super::super::presentation::{ItemChange, Presentation};
    let session = omp_card(None, None);
    let (paused, observed_pause) = std::sync::mpsc::channel();
    let (resume, released) = std::sync::mpsc::channel();
    PUBLICATION_GATES
        .lock()
        .unwrap()
        .insert(session.id, (paused, released));
    let before = retirement_text("before".into());
    let after = retirement_text("before tail".into());
    let update = Presentation {
        base_generation: 1,
        metadata: Snapshot {
            generation: 2,
            ..Default::default()
        },
        order: vec![after.id.clone()],
        changes: vec![ItemChange::between(Some(&before), &after)],
        children: vec![],
        file_changes_unchanged: false,
    };
    let (callback, callbacks) = std::sync::mpsc::channel();
    let (mut client, server, eof) = retirement_wire(
        session,
        Snapshot {
            generation: 1,
            items: vec![before],
            ..Default::default()
        },
        Some(update),
        move |wake| {
            let _ = callback.send(wake);
        },
    );
    let (finished, observed_finish) = std::sync::mpsc::channel();
    Arc::get_mut(&mut client.inner).unwrap().retirement_complete = Some(finished);
    let alive = client.inner.alive.clone();
    client.set_visible(true).unwrap();
    observed_pause.recv_timeout(Duration::from_secs(5)).unwrap();
    while callbacks.try_recv().is_ok() {}
    let (dropped, observed_drop) = std::sync::mpsc::channel();
    let dropping = thread::spawn(move || {
        drop(client);
        dropped.send(thread::current().id()).unwrap();
    });
    let returned_while_paused = observed_drop.recv_timeout(Duration::from_secs(1));
    let cancelled_while_paused = eof.recv_timeout(Duration::from_secs(1));
    let unfinished_was_retained = observed_finish.try_recv().is_err();
    let cancelled_before_resume = !alive.load(Ordering::Acquire);
    let no_callback_before_resume = callbacks.try_recv().is_err();
    // A second real owned payload has no connection threads to wait on.
    // It must retire while the first real connection remains gated.
    let mut independent = ClientHandle::fixture(omp_card(None, None), Snapshot::default());
    let (other_finished, other_observed) = std::sync::mpsc::channel();
    Arc::get_mut(&mut independent.inner)
        .unwrap()
        .retirement_complete = Some(other_finished);
    drop(independent);
    let other_retired_while_paused = other_observed.recv_timeout(Duration::from_secs(1));
    // Always release/retire the real held reader before judging results.
    resume.send(()).unwrap();
    dropping.join().unwrap();
    server.join().unwrap();
    let released_on = observed_finish
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let late: Vec<_> = callbacks.try_iter().collect();
    assert!(
        cancelled_before_resume,
        "alive must be fenced inline before the held reader resumes"
    );
    assert!(no_callback_before_resume);
    assert!(late.iter().any(|wake| matches!(wake, Wake::Changed)));
    assert!(
        late.iter()
            .any(|wake| matches!(wake, Wake::Disconnected(_))),
        "real queued callbacks may arrive after Drop returns; their UI Target must remain fenced"
    );
    let dropped_on =
        returned_while_paused.expect("dropping the last UI owner must not join its gated reader");
    assert!(
        cancelled_while_paused.is_ok(),
        "actual socket shutdown must remain inline"
    );
    assert!(
        unfinished_was_retained,
        "never claim destruction/join completion while the reader is gated"
    );
    assert_ne!(released_on, dropped_on);
    assert_eq!(
        other_retired_while_paused.unwrap(),
        released_on,
        "one unfinished reader must not head-of-line block another ready payload"
    );
}

#[test]
fn settled_reader_and_failed_join_release_the_actual_large_publication_off_the_dropping_thread() {
    let body = "large retained client body\n".repeat(200_000);
    let bytes = body.len();
    let (mut client, server, _) = retirement_wire(
        omp_card(None, None),
        Snapshot {
            generation: 1,
            items: vec![retirement_text(body)],
            ..Default::default()
        },
        None,
        |wake| {
            if matches!(wake, Wake::Disconnected(_)) {
                panic!("fixture settled reader failure");
            }
        },
    );
    server.join().unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let reader_done = client
            .inner
            .reader
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_finished();
        let visibility_done = client
            .inner
            .visibility_worker
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_finished();
        if reader_done && visibility_done {
            break;
        }
        assert!(
            Instant::now() < until,
            "actual connection threads must settle before the final-owner test"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let publication = Arc::downgrade(&client.inner.publication);
    let state = client.inner.publication.current.load_full();
    assert_eq!(state.snapshot.transcript_bytes(), bytes);
    let snapshot = Arc::downgrade(&state.snapshot);
    let (body_released, body_completion) = std::sync::mpsc::channel();
    *state.retirement_complete.lock().unwrap() = Some(body_released);
    drop(state);
    let (finished, observed_finish) = std::sync::mpsc::channel();
    Arc::get_mut(&mut client.inner).unwrap().retirement_complete = Some(finished);
    let dropping_thread = thread::current().id();
    drop(client);
    let released_on = observed_finish
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert_ne!(
        released_on, dropping_thread,
        "even a settled connection's large last publication belongs off UI"
    );
    assert!(
        publication.upgrade().is_none(),
        "retirement must release the actual publication owner"
    );
    assert_eq!(
        body_completion
            .recv_timeout(Duration::from_secs(5))
            .unwrap(),
        released_on
    );
    assert!(
        snapshot.upgrade().is_none(),
        "retirement must destroy the final actual snapshot body, not only its handles"
    );
}

#[test]
fn a_panicking_payload_does_not_strand_later_clients() {
    struct CallbackOwner(std::sync::mpsc::Sender<std::thread::ThreadId>);
    impl Drop for CallbackOwner {
        fn drop(&mut self) {
            let _ = self.0.send(thread::current().id());
            // A second panic while unwinding would abort the whole test binary.
            if !thread::panicking() {
                panic!("fixture callback owner destructor");
            }
        }
    }
    let (destroyed, destruction) = std::sync::mpsc::channel();
    let owned = CallbackOwner(destroyed);
    let mut first = ClientHandle::fixture(omp_card(None, None), Snapshot::default());
    Arc::get_mut(&mut first.inner).unwrap().wake = Arc::new(move |_| {
        let _ = &owned;
    });
    let dropping_thread = thread::current().id();
    drop(first);
    let failed_on = destruction.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut next = ClientHandle::fixture(omp_card(None, None), Snapshot::default());
    let (finished, completion) = std::sync::mpsc::channel();
    Arc::get_mut(&mut next.inner).unwrap().retirement_complete = Some(finished);
    drop(next);
    assert_ne!(failed_on, dropping_thread);
    assert_eq!(
        completion.recv_timeout(Duration::from_secs(5)).unwrap(),
        failed_on,
        "the same process-owned worker must survive an ordinary payload destructor unwind"
    );
}

#[test]
fn retirement_worker_start_failure_refuses_before_handshake_or_payload_admission() {
    use std::io::Read;
    let session = omp_card(None, None);
    FAIL_RETIREMENT_START.lock().unwrap().insert(session.id);
    let (client, mut server) = UnixStream::pair().unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let result = ClientHandle::connect_on(
        client,
        session,
        Intent::Start,
        None,
        false,
        None,
        None,
        None,
        |_| {},
    );
    let Err(error) = result else {
        panic!("thread factory failure must refuse the client");
    };
    assert!(error.message.contains("retirement worker startup failed"));
    let mut byte = [0];
    assert_eq!(
        server.read(&mut byte).unwrap(),
        0,
        "no Connect/ChatStart or body ownership may precede retirement admission"
    );
}

fn observed_large_publication() -> (
    Arc<Publication>,
    std::sync::Weak<Snapshot>,
    std::sync::mpsc::Receiver<std::thread::ThreadId>,
) {
    let body = "large publication last owner\n".repeat(400_000);
    let publication = Publication::initial(Snapshot {
        generation: 1,
        items: vec![retirement_text(body)],
        ..Default::default()
    });
    let snapshot = Arc::downgrade(&publication.snapshot);
    let (completed, completion) = std::sync::mpsc::channel();
    *publication.retirement_complete.lock().unwrap() = Some(completed);
    (publication, snapshot, completion)
}

#[test]
fn an_idle_hide_reports_the_installed_policy_without_another_source_frame() {
    use tracing::field::{Field, Visit};
    use tracing_subscriber::{Layer, layer::SubscriberExt};

    #[derive(Default)]
    struct Fields(BTreeMap<String, serde_json::Value>);
    impl Visit for Fields {
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name().into(), value.into());
        }
        fn record_bool(&mut self, field: &Field, value: bool) {
            self.0.insert(field.name().into(), value.into());
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().into(), value.into());
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().into(), format!("{value:?}").into());
        }
    }
    struct Observe {
        client: std::sync::Weak<Inner>,
        rows: Arc<Mutex<Vec<BTreeMap<String, serde_json::Value>>>>,
    }
    impl<S: tracing::Subscriber> Layer<S> for Observe {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            if fields.0.get("owner") != Some(&serde_json::json!("chat_client_policy")) {
                return;
            }
            let client = self.client.upgrade().expect("caller still owns the client");
            let _gate =
                client.publication.commit.try_lock().expect(
                    "diagnostic subscriber must run after the publication gate is unlocked",
                );
            self.rows.lock().unwrap().push(fields.0);
        }
    }

    let session = omp_card(None, None);
    let (client, mut server) = UnixStream::pair().unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let worker = thread::spawn(move || {
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        handoff_handshake(&mut server, &mut codec);
        let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        let ClientRequest::ChatStart { session, .. } = request.request else {
            panic!("expected real client attachment")
        };
        handoff_reply(
            &mut server,
            &mut codec,
            Some(request.request_id),
            &handoff_proof(),
            ServerMessage::Response {
                response: ServerResponse::ChatAttached {
                    session,
                    snapshot: Snapshot {
                        generation: 41,
                        items: vec![retirement_text("idle body".repeat(128 * 1024))],
                        ..Default::default()
                    },
                },
            },
        );
        // Keep the real reader idle: never send an ACK, snapshot, update or
        // other frame after attachment. Drain visibility only until Drop.
        while codec.read::<ClientEnvelope>(&mut server).unwrap().is_some() {}
    });
    let handle = ClientHandle::connect_on(
        client,
        session,
        Intent::Start,
        None,
        false,
        None,
        None,
        None,
        |_| {},
    )
    .unwrap();
    let body = Arc::downgrade(&handle.snapshot().items[0]);
    let installed = handle.inner.publication.current.load_full();
    let (released, completion) = std::sync::mpsc::channel();
    *installed.retirement_complete.lock().unwrap() = Some(released);
    drop(installed);
    let rows = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Observe {
        client: Arc::downgrade(&handle.inner),
        rows: rows.clone(),
    });
    crate::test_tracing::with_thread_subscriber(subscriber, || {
        handle.set_visible(false).unwrap();
        handle.set_visible(false).unwrap();
        assert!(handle.resident_snapshot_with_clock().is_none());
        assert_eq!(handle.snapshot().generation, 41);
        handle.set_visible(true).unwrap();
        // A show requests hydration; it must not claim the old body became
        // resident merely because interest changed while the source is idle.
        assert!(handle.resident_snapshot_with_clock().is_none());
        handle.set_visible(true).unwrap();
    });
    completion.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(body.upgrade().is_none());
    let observed = rows.lock().unwrap();
    assert_eq!(
        observed.len(),
        2,
        "unchanged calls must not spam policy events"
    );
    for (row, revision, requested, swapped) in [
        (&observed[0], 1, false, true),
        (&observed[1], 2, true, false),
    ] {
        assert_eq!(row["policy_revision"], revision);
        assert_eq!(row["published_generation"], 41);
        assert_eq!(row["published_resident"], false);
        assert_eq!(row["metadata_installed"], swapped);
        assert_eq!(row["requested_interested"], requested);
        assert_eq!(row["requested_retain_body"], requested);
        assert!(!row.contains_key("resident_bytes"));
        assert!(!row.contains_key("released"));
    }
    drop(observed);
    drop(handle);
    worker.join().unwrap();
}

#[test]
fn hiding_retires_the_actual_last_publication_body_off_the_caller() {
    let client = ClientHandle::fixture(omp_card(None, None), Snapshot::default());
    let (publication, body, completion) = observed_large_publication();
    let identity = Arc::downgrade(&publication);
    drop(client.inner.publication.current.swap(publication));
    let caller = thread::current().id();
    client.set_interest(false, false).unwrap();
    assert!(!client.inner.publication.current.load().resident);
    let destroyed_on = completion.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_ne!(destroyed_on, caller);
    assert!(identity.upgrade().is_none());
    assert!(
        body.upgrade().is_none(),
        "the actual large Snapshot must be released, not only a spare wrapper"
    );
}

#[test]
fn a_final_arc_swap_reader_guard_retires_its_body_off_the_reader() {
    let client = ClientHandle::fixture(omp_card(None, None), Snapshot::default());
    let (publication, body, completion) = observed_large_publication();
    drop(client.inner.publication.current.swap(publication));
    let guard = client.inner.publication.current.load();
    let original = Arc::as_ptr(&guard);
    let publications = client.inner.publication.clone();
    let writer = thread::spawn(move || {
        assert_eq!(
            publications.replace(
                Snapshot {
                    generation: 2,
                    ..Default::default()
                },
                &AtomicBool::new(true),
                false,
                false
            ),
            Some(true)
        );
    });
    writer.join().unwrap();
    assert_ne!(
        Arc::as_ptr(&client.inner.publication.current.load()),
        original
    );
    assert_eq!(
        guard.snapshot.generation, 1,
        "the real ArcSwap guard still owns the superseded publication"
    );
    assert!(body.upgrade().is_some());
    assert!(completion.try_recv().is_err());
    let reader = thread::current().id();
    drop(guard);
    assert_ne!(
        completion.recv_timeout(Duration::from_secs(5)).unwrap(),
        reader
    );
    assert!(body.upgrade().is_none());
}

#[test]
fn ordinary_reader_replacement_disposes_its_previous_body_on_that_reader() {
    use super::super::presentation::{ItemChange, Presentation};
    let body = "source reader disposal\n".repeat(200_000);
    let before = retirement_text(body.clone());
    let after = retirement_text(body + "tail");
    let update = Presentation {
        base_generation: 1,
        metadata: Snapshot {
            generation: 2,
            ..Default::default()
        },
        order: vec![after.id.clone()],
        changes: vec![ItemChange::between(Some(&before), &after)],
        children: vec![],
        file_changes_unchanged: false,
    };
    let (client, server, eof) = retirement_wire(
        omp_card(None, None),
        Snapshot {
            generation: 1,
            items: vec![before],
            ..Default::default()
        },
        Some(update),
        |_| {},
    );
    let reader = client
        .inner
        .reader
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .thread()
        .id();
    let current = client.inner.publication.current.load_full();
    let previous_body = Arc::downgrade(&current.snapshot);
    let (completed, completion) = std::sync::mpsc::channel();
    *current.retirement_complete.lock().unwrap() = Some(completed);
    drop(current);
    client.set_visible(true).unwrap();
    let disposal = completion.recv_timeout(Duration::from_secs(5));
    // Release/finish the real wire before judging the thread oracle.
    drop(client);
    eof.recv_timeout(Duration::from_secs(5)).unwrap();
    server.join().unwrap();
    assert_eq!(
        disposal.unwrap(),
        reader,
        "ordinary source updates must keep actual prior-body disposal on their producer, preserving backpressure"
    );
    assert!(previous_body.upgrade().is_none());
}

#[test]
fn retirement_initialization_retries_after_a_thread_factory_error() {
    let initializer = ClientRetirementInitializer::new();
    let starts = AtomicUsize::new(0);
    let failure = initializer.get_or_start(|| {
        starts.fetch_add(1, Ordering::Relaxed);
        ClientRetirement::start_with(|_| Err(io::Error::from(io::ErrorKind::WouldBlock)))
    });
    assert!(failure.is_err());
    assert!(
        initializer.owner.get().is_none(),
        "failed startup must not poison later admission"
    );
    let owner = initializer
        .get_or_start(|| {
            starts.fetch_add(1, Ordering::Relaxed);
            ClientRetirement::start_with(|worker| {
                thread::Builder::new()
                    .name("chat-retire-initializer-fixture".into())
                    .spawn(worker)
            })
        })
        .unwrap();
    let same = initializer
        .get_or_start(|| panic!("a successful owner must be reused without another worker spawn"))
        .unwrap();
    assert!(Arc::ptr_eq(&owner, &same));
    assert_eq!(
        starts.load(Ordering::Relaxed),
        2,
        "one failed attempt and one actual successful worker"
    );
    let worker = owner.worker.lock().unwrap().as_ref().unwrap().thread().id();
    // Exercise real payload admission after successful retry, not merely a
    // stored flag. The isolated worker is process-owned, just like shared().
    let mut client = ClientHandle::fixture(omp_card(None, None), Snapshot::default());
    let inner = Arc::get_mut(&mut client.inner).unwrap();
    inner.retirement = owner;
    let (finished, completion) = std::sync::mpsc::channel();
    inner.retirement_complete = Some(finished);
    drop(client);
    assert_eq!(
        completion.recv_timeout(Duration::from_secs(5)).unwrap(),
        worker
    );
}
