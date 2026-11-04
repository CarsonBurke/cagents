//! Actual HOST admission and dispatch. Explicit User and delegated roles below
//! are predetermined policy fixtures, never a reinterpretation of the test
//! process's inherited opaque authority or proof of provider authentication.
use super::{ClientRequest, ProtocolErrorCode, ServerResponse, SharedHost, process_request};
use crate::{
    account::{
        home::Homes,
        preparation::{
            AccountPreparationOutcome, AccountPreparationRecord, AccountPreparationReply,
            AccountPreparationRequest, ExistingPendingRequest, ExistingPendingTarget,
            PreparationBinding, PreparationPayload, PreparationRequest, PreparationState,
        },
        registry::Store,
    },
    agent::Provider,
    agent_execution::{Authority, Lineage},
    device_service::{Operation, Reply},
    host::state::{HostLock, HostState},
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, TerminalId},
};
use std::sync::Arc;
use uuid::Uuid;

const USER: u64 = 1;
const AGENT: u64 = 2;

fn host() -> SharedHost {
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("preparation-policy-fixture"),
        boot_id: BootId::new("preparation-policy-boot"),
        host_incarnation: HostIncarnation::new("preparation-policy-incarnation"),
    });
    state.register_client(USER);
    state.register_client(AGENT);
    state
        .agent_clients
        .insert(AGENT, Lineage(TerminalId::new()));
    Arc::new(HostLock::new(state))
}

fn binding() -> PreparationBinding {
    PreparationBinding {
        request_id: Uuid::from_u128(1),
        installation_id: Uuid::from_u128(2),
        key: Uuid::from_u128(3),
        payload: PreparationPayload {
            provider: Provider::Codex,
            copy_codex_config: true,
        },
    }
}

fn operation(request: AccountPreparationRequest) -> Operation {
    Operation::AccountPreparation { request }
}

fn refused_metadata() -> AccountPreparationRequest {
    // Policy tests must not enter physical Create or inspect ambient System
    // configuration. This request is explicitly refused before storage.
    AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Resume {
        target: ExistingPendingTarget::Observed {
            provider: Provider::Codex,
            key: Uuid::from_u128(3),
            started_at: 17,
            expected_installation: None,
        },
    })
}

fn authenticate_user(host: &SharedHost, client: u64) {
    // Fixed synthetic User policy role, chosen before any request or refusal.
    assert!(matches!(
        process_request(
            host,
            client,
            ClientRequest::Authenticate {
                authority: Authority::default(),
            },
        )
        .unwrap()
        .0,
        ServerResponse::Accepted
    ));
}

fn dispatch(
    host: &SharedHost,
    client: u64,
    request: AccountPreparationRequest,
) -> Result<AccountPreparationReply, crate::terminal_ipc::ProtocolError> {
    let (response, extra, shutdown) = process_request(
        host,
        client,
        ClientRequest::DeviceOperation {
            operation: operation(request),
        },
    )?;
    assert!(extra.is_empty());
    assert!(!shutdown);
    let ServerResponse::DeviceOperation {
        reply: Reply::AccountPreparation(reply),
    } = response
    else {
        panic!("preparation must use its distinct typed reply")
    };
    Ok(reply)
}

#[test]
fn account_preparation_requires_registered_explicit_user_without_delegation_fallback() {
    let host = host();
    for client in [USER, AGENT, 99] {
        assert_eq!(
            dispatch(&host, client, refused_metadata())
                .unwrap_err()
                .code,
            ProtocolErrorCode::PermissionDenied
        );
    }
    authenticate_user(&host, USER);
    let reply = dispatch(&host, USER, refused_metadata()).unwrap();
    assert_eq!(reply.request, refused_metadata());
    assert!(matches!(
        reply.outcome,
        AccountPreparationOutcome::NotAttempted { .. }
    ));

    // Existing authentication policy remains unchanged for other requests,
    // but default authentication cannot upgrade a verified delegated client.
    authenticate_user(&host, AGENT);
    assert_eq!(
        dispatch(&host, AGENT, refused_metadata()).unwrap_err().code,
        ProtocolErrorCode::PermissionDenied
    );
    authenticate_user(&host, 99);
    assert_eq!(
        dispatch(&host, 99, refused_metadata()).unwrap_err().code,
        ProtocolErrorCode::PermissionDenied
    );

    // A token-bearing attempt invalidates prior explicit User scope before
    // resolution. Failed delegation never leaves or reacquires that scope.
    let error = process_request(
        &host,
        USER,
        ClientRequest::Authenticate {
            authority: Authority::session("invalid-policy-fixture".into()),
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::PermissionDenied);
    assert_eq!(
        dispatch(&host, USER, refused_metadata()).unwrap_err().code,
        ProtocolErrorCode::PermissionDenied
    );
    authenticate_user(&host, USER);
    assert_eq!(
        dispatch(&host, USER, refused_metadata()).unwrap_err().code,
        ProtocolErrorCode::PermissionDenied
    );
}

#[test]
fn account_preparation_scope_is_removed_at_both_disconnect_edges_and_id_reuse() {
    for cleanup in [0, 1, 2] {
        let host = host();
        authenticate_user(&host, USER);
        {
            let mut state = host.lock().unwrap();
            match cleanup {
                0 => {
                    state.unregister_client(USER);
                }
                1 => state.forget_client_delivery(USER),
                _ => {
                    state.register_client(USER);
                }
            }
            assert!(!state.account_preparation_callers.contains_key(&USER));
        }
        assert_eq!(
            dispatch(&host, USER, refused_metadata()).unwrap_err().code,
            ProtocolErrorCode::PermissionDenied
        );
    }
}

#[test]
fn account_preparation_preserves_recovery_shutdown_and_physical_lifecycle_admission() {
    let recovering = host();
    authenticate_user(&recovering, USER);
    recovering
        .lock()
        .unwrap()
        .retain_startup_recovery("original owner retained".into());
    assert_eq!(
        dispatch(&recovering, USER, refused_metadata())
            .unwrap_err()
            .code,
        ProtocolErrorCode::TerminalUnavailable
    );
    let shutdown = host();
    authenticate_user(&shutdown, USER);
    shutdown.lock().unwrap().shutting_down = true;
    assert_eq!(
        dispatch(&shutdown, USER, refused_metadata())
            .unwrap_err()
            .code,
        ProtocolErrorCode::TerminalUnavailable
    );
    let host = host();
    authenticate_user(&host, USER);
    let root = tempfile::tempdir().unwrap();
    let reservation = crate::host::account_lifecycle::Reservation::test_install(
        &host,
        &[],
        Store::at(root.path().join("registry")),
        &Homes::at(root.path().join("homes")),
    )
    .unwrap();
    let error = dispatch(&host, USER, refused_metadata()).unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
    assert!(
        error
            .message
            .contains("account operation is still finishing")
    );
    drop(reservation);
    assert!(matches!(
        dispatch(&host, USER, refused_metadata()).unwrap().outcome,
        AccountPreparationOutcome::NotAttempted { .. }
    ));
}

#[test]
fn account_preparation_cannot_bypass_host_admission_through_generic_execute() {
    assert!(
        crate::device_service::execute(operation(refused_metadata()))
            .unwrap_err()
            .contains("explicit user and daemon admission")
    );
}

#[test]
fn account_preparation_actual_host_wire_refuses_anonymous_failed_delegation_and_foreign_proof() {
    use crate::terminal_ipc::{
        ClientEnvelope, PROTOCOL_VERSION, ServerEnvelope, ServerMessage, read_frame, write_frame,
    };
    use std::{os::unix::net::UnixStream, thread, time::Duration};
    let host = host();
    let proof = host.lock().unwrap().proof().clone();
    let (mut stream, peer) = UnixStream::pair().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let serving = Arc::clone(&host);
    let server = thread::spawn(move || super::serve(peer, 10, serving));
    let exchange = |stream: &mut UnixStream, id, request, proof: Option<HostSessionProof>| {
        write_frame(
            stream,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id: id,
                proof,
                request,
            },
        )
        .unwrap();
        let response: ServerEnvelope = read_frame(stream).unwrap().unwrap();
        assert_eq!(response.request_id, Some(id));
        response.message
    };
    assert!(matches!(
        exchange(
            &mut stream,
            1,
            ClientRequest::Connect {
                expected_host: Some(proof.clone()),
                resume: vec![],
            },
            None
        ),
        ServerMessage::Response {
            response: ServerResponse::Connected { .. }
        }
    ));
    let request = || ClientRequest::DeviceOperation {
        operation: operation(refused_metadata()),
    };
    let refused = |message| {
        let ServerMessage::Error(error) = message else {
            panic!("HOST must refuse before dispatch")
        };
        assert_eq!(error.code, ProtocolErrorCode::PermissionDenied);
    };
    refused(exchange(&mut stream, 2, request(), Some(proof.clone())));
    refused(exchange(
        &mut stream,
        3,
        ClientRequest::Authenticate {
            authority: Authority::session("invalid-policy-fixture".into()),
        },
        Some(proof.clone()),
    ));
    // No fallback: even an explicit default request on this exact connection
    // cannot reclaim setup scope after its token-bearing attempt was refused.
    assert!(matches!(
        exchange(
            &mut stream,
            4,
            ClientRequest::Authenticate {
                authority: Authority::default(),
            },
            Some(proof.clone())
        ),
        ServerMessage::Response {
            response: ServerResponse::Accepted
        }
    ));
    refused(exchange(&mut stream, 5, request(), Some(proof.clone())));
    let mut foreign = proof;
    foreign.host_incarnation = HostIncarnation::new("foreign-policy-incarnation");
    let ServerMessage::Error(error) = exchange(&mut stream, 6, request(), Some(foreign)) else {
        panic!("HOST must reject a foreign route before dispatch")
    };
    assert_eq!(error.code, ProtocolErrorCode::ProofMismatch);
    drop(stream);
    server.join().unwrap();
    assert!(!host.lock().unwrap().clients.contains_key(&10));
    assert!(
        !host
            .lock()
            .unwrap()
            .account_preparation_callers
            .contains_key(&10)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn account_preparation_actual_host_wire_mutates_only_the_selected_installation_metadata() {
    use std::{
        os::unix::net::UnixListener,
        thread,
        time::{Duration, Instant},
    };
    let name = thread::current().name().unwrap().to_owned();
    // This existing helper preserves inherited opaque authority and isolates
    // XDG state in an exact self-exec; it never converts that caller to User.
    let Some(root) = crate::host::exit_drain_tests::private_runtime_case(&name, None) else {
        return;
    };
    let host = host();
    let socket = root.join("run/preparation-policy.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let serving = Arc::clone(&host);
    let server_store = Store::installation().unwrap();
    let server_homes = Homes::installation().unwrap();
    let source = crate::host::account_preparation::SeedSelection {
        system: root.join("unused-private-metadata-source"),
        claude_global: None,
    };
    let server = thread::spawn(move || {
        crate::host::account_preparation::with_fixture(server_store, server_homes, source, || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        super::serve(stream, 10, serving);
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "private HOST connection was not opened"
                        );
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("private HOST listener: {error}"),
                }
            }
        })
    });
    let mut client = crate::control::ControlClient::connect(&socket).unwrap();
    // Predetermined synthetic User policy fixture, unrelated to inherited scope.
    client.authenticate(Authority::default()).unwrap();
    let store = Store::installation().unwrap();
    let foreign = Store::at(root.join("foreign-registry"));
    let foreign_planned = foreign
        .plan_preparation(Uuid::from_u128(99), binding().payload)
        .unwrap();
    let foreign_before = foreign.load().unwrap();
    let request_id = Uuid::from_u128(42);
    let plan = AccountPreparationRequest::Preparation(PreparationRequest::Plan {
        request_id,
        payload: binding().payload,
    });
    let ask = |client: &mut crate::control::ControlClient, request: AccountPreparationRequest| {
        let Reply::AccountPreparation(reply) =
            client.device_operation(operation(request.clone())).unwrap()
        else {
            panic!("actual HOST must return typed preparation metadata")
        };
        assert_eq!(reply.request, request);
        reply.outcome
    };
    let AccountPreparationOutcome::Confirmed {
        record: AccountPreparationRecord::Preparation(planned),
    } = ask(&mut client, plan)
    else {
        panic!("selected Store must confirm the Plan barrier")
    };
    assert_eq!(
        store.load().unwrap().preparation(request_id),
        Some(&planned)
    );
    let before_refusal = store.load().unwrap();
    assert!(matches!(
        ask(&mut client, refused_metadata()),
        AccountPreparationOutcome::NotAttempted { .. }
    ));
    assert_eq!(store.load().unwrap(), before_refusal);
    for cancel in [false, true] {
        let target = ExistingPendingTarget::Observed {
            provider: Provider::Claude,
            key: Uuid::from_u128(50),
            started_at: 1234,
            expected_installation: None,
        };
        let request = if cancel {
            ExistingPendingRequest::Cancel {
                operation_id: Uuid::from_u128(51),
                target,
            }
        } else {
            ExistingPendingRequest::Resume { target }
        };
        assert!(matches!(
            ask(
                &mut client,
                AccountPreparationRequest::ExistingPending(request)
            ),
            AccountPreparationOutcome::NotAttempted { .. }
        ));
        assert_eq!(store.load().unwrap(), before_refusal);
    }
    assert!(matches!(
        ask(
            &mut client,
            AccountPreparationRequest::Preparation(PreparationRequest::Reconcile {
                binding: foreign_planned.binding,
            })
        ),
        AccountPreparationOutcome::ReconcileRequired { observed: None, .. }
    ));
    assert_eq!(store.load().unwrap(), before_refusal);
    let AccountPreparationOutcome::Confirmed {
        record: AccountPreparationRecord::Preparation(cancelled),
    } = ask(
        &mut client,
        AccountPreparationRequest::Preparation(PreparationRequest::CancelRetained {
            binding: planned.binding.clone(),
        }),
    )
    else {
        panic!("selected Store must confirm retained cancellation")
    };
    assert_eq!(cancelled.binding, planned.binding);
    assert_eq!(cancelled.state, PreparationState::CancelledRetained);
    assert!(matches!(
        ask(
            &mut client,
            AccountPreparationRequest::Preparation(PreparationRequest::Reconcile {
                binding: cancelled.binding,
            })
        ),
        AccountPreparationOutcome::Confirmed { .. }
    ));
    assert_eq!(foreign.load().unwrap(), foreign_before);
    assert!(
        !Homes::installation()
            .unwrap()
            .path(Provider::Codex, planned.binding.key)
            .exists()
    );
    drop(client);
    server.join().unwrap();
    assert!(
        !host
            .lock()
            .unwrap()
            .account_preparation_callers
            .contains_key(&10)
    );
}
