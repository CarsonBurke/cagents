//! Private synthetic hosts exercise the ordinary admission and framed client.
//! No provider, public daemon, environment rewrite or account home is used.
use super::*;
use crate::{
    account::{
        home::Homes,
        installation::{
            InitializationOutcome, InitializationRefusal, InitializationReply,
            InitializationRequest,
        },
        registry::Store,
    },
    agent_execution::{Authority, Lineage},
    device_service::{Operation, Reply},
    host::{
        account_preparation::{SeedSelection, with_fixture},
        state::{HostLock, HostState},
    },
    model::{BootId, HostIncarnation, HostSessionId, TerminalId},
};
use std::{fs, os::unix::fs::PermissionsExt};
use uuid::Uuid;

struct Fixture {
    root: tempfile::TempDir,
    host: SharedHost,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("initialization-host"),
            boot_id: BootId::new("initialization-boot"),
            host_incarnation: HostIncarnation::new("initialization-incarnation"),
        });
        state.register_client(1);
        state.register_client(2);
        state.agent_clients.insert(2, Lineage(TerminalId::new()));
        Self {
            store: Store::at(root.path().join("registry")),
            root,
            host: Arc::new(HostLock::new(state)),
        }
    }

    fn run<T>(&self, run: impl FnOnce() -> T) -> T {
        with_fixture(
            self.store.clone(),
            Homes::at(self.root.path().join("unused-homes")),
            SeedSelection {
                system: self.root.path().join("unused-system"),
                claude_global: None,
            },
            run,
        )
    }

    fn authenticate(
        &self,
        client: u64,
        authority: Authority,
    ) -> Result<RequestOutcome, ProtocolError> {
        process_request(
            &self.host,
            client,
            ClientRequest::Authenticate { authority },
        )
    }

    fn ask(&self, client: u64, request_id: Uuid) -> Result<InitializationReply, ProtocolError> {
        self.run(|| {
            let (response, extras, shutdown) = process_request(
                &self.host,
                client,
                ClientRequest::DeviceOperation {
                    operation: Operation::AccountInstallationInitialize {
                        request: InitializationRequest { request_id },
                    },
                },
            )?;
            assert!(extras.is_empty() && !shutdown);
            let ServerResponse::DeviceOperation {
                reply: Reply::AccountInstallationInitialized(reply),
            } = response
            else {
                panic!("wrong initializer reply")
            };
            Ok(reply)
        })
    }
}

#[test]
fn initialization_requires_original_explicit_user_and_sticky_delegation_before_storage() {
    let fixture = Fixture::new();
    let id = Uuid::from_u128(101);
    for client in [1, 2, 99] {
        assert_eq!(
            fixture.ask(client, id).unwrap_err().code,
            ProtocolErrorCode::PermissionDenied
        );
    }
    fixture.authenticate(1, Authority::default()).unwrap();
    let reply = fixture.ask(1, id).unwrap();
    assert!(reply.matches(InitializationRequest { request_id: id }));
    assert!(
        matches!(reply.outcome, InitializationOutcome::Confirmed { installation_id } if !installation_id.is_nil())
    );
    let file = fixture.root.path().join("registry/accounts.toml");
    let before = fs::read(&file).unwrap();
    // A refused delegation attempt remains sticky after empty authentication.
    assert!(
        fixture
            .authenticate(
                1,
                Authority::session("invalid-initializer-capability".into())
            )
            .is_err()
    );
    fixture.authenticate(1, Authority::default()).unwrap();
    assert_eq!(
        fixture.ask(1, id).unwrap_err().code,
        ProtocolErrorCode::PermissionDenied
    );
    assert_eq!(fs::read(&file).unwrap(), before);
    assert!(!fixture.root.path().join("unused-homes").exists());
    assert!(!fixture.root.path().join("unused-system").exists());
}

#[test]
fn initialization_nil_and_store_unavailable_are_the_only_typed_pre_store_refusals() {
    let fixture = Fixture::new();
    fixture.authenticate(1, Authority::default()).unwrap();
    assert_eq!(
        fixture.ask(1, Uuid::nil()).unwrap().outcome,
        InitializationOutcome::NotAttempted {
            reason: InitializationRefusal::InvalidRequestId
        }
    );
    assert!(!fixture.root.path().join("registry").exists());
    let request = ClientRequest::DeviceOperation {
        operation: Operation::AccountInstallationInitialize {
            request: InitializationRequest {
                request_id: Uuid::from_u128(102),
            },
        },
    };
    let (response, _, _) = process_request(&fixture.host, 1, request).unwrap();
    assert!(matches!(
        response,
        ServerResponse::DeviceOperation {
            reply: Reply::AccountInstallationInitialized(InitializationReply {
                outcome: InitializationOutcome::NotAttempted {
                    reason: InitializationRefusal::StoreUnavailable
                },
                ..
            })
        }
    ));
    fs::create_dir(fixture.root.path().join("registry")).unwrap();
    fs::set_permissions(
        fixture.root.path().join("registry"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let path = fixture.root.path().join("registry/accounts.toml");
    fs::write(&path, "private-fixture-contents=[").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let reply = fixture.ask(1, Uuid::from_u128(103)).unwrap();
    assert_eq!(reply.outcome, InitializationOutcome::Unknown);
    assert!(
        !serde_json::to_string(&reply)
            .unwrap()
            .contains("private-fixture")
    );
}

#[test]
fn initialization_preserves_shutdown_and_recovery_gates_without_storage_effects() {
    for shutdown in [false, true] {
        let fixture = Fixture::new();
        fixture.authenticate(1, Authority::default()).unwrap();
        {
            let mut state = fixture.host.lock().unwrap();
            if shutdown {
                state.shutting_down = true;
            } else {
                state.retain_startup_recovery("held initializer fixture".into());
            }
        }
        assert!(fixture.ask(1, Uuid::from_u128(104)).is_err());
        assert!(!fixture.root.path().join("registry").exists());
    }
}

#[test]
fn initialization_keeps_disconnect_client_reuse_and_physical_lifecycle_reservations_fenced() {
    for edge in 0..4 {
        let fixture = Fixture::new();
        fixture.authenticate(1, Authority::default()).unwrap();
        let homes = Homes::at(fixture.root.path().join("reservation-homes"));
        let reservation = if edge == 3 {
            Some(
                crate::host::account_lifecycle::Reservation::test_install(
                    &fixture.host,
                    &[],
                    fixture.store.clone(),
                    &homes,
                )
                .unwrap(),
            )
        } else {
            None
        };
        match edge {
            0 => {
                fixture.host.lock().unwrap().unregister_client(1);
            }
            1 => {
                fixture.host.lock().unwrap().forget_client_delivery(1);
            }
            2 => {
                fixture.host.lock().unwrap().register_client(1);
            }
            _ => {}
        }
        let before = fs::read(fixture.root.path().join("registry/accounts.toml")).ok();
        assert!(fixture.ask(1, Uuid::from_u128(110)).is_err());
        assert_eq!(
            fs::read(fixture.root.path().join("registry/accounts.toml")).ok(),
            before
        );
        drop(reservation);
    }
}

#[test]
fn initialization_original_control_client_and_foreign_versions_never_retarget_storage() {
    use std::os::unix::net::UnixStream;
    for version in [PROTOCOL_VERSION, 107, 133] {
        let fixture = Fixture::new();
        let (stream, peer) = UnixStream::pair().unwrap();
        let host = Arc::clone(&fixture.host);
        let store = fixture.store.clone();
        let homes = Homes::at(fixture.root.path().join("unused-homes"));
        let source = SeedSelection {
            system: fixture.root.path().join("unused-system"),
            claude_global: None,
        };
        let serving = thread::spawn(move || {
            with_fixture(store, homes, source, || super::serve(peer, 71, host))
        });
        if version == PROTOCOL_VERSION {
            let mut client = crate::control::ControlClient::from_stream(stream).unwrap();
            client.authenticate(Authority::default()).unwrap();
            let proof = client.proof().clone();
            let mut observed = None;
            for number in [105, 106] {
                let request = InitializationRequest {
                    request_id: Uuid::from_u128(number),
                };
                let Reply::AccountInstallationInitialized(reply) = client
                    .device_operation(Operation::AccountInstallationInitialize { request })
                    .unwrap()
                else {
                    panic!("wrong reply")
                };
                assert!(reply.matches(request));
                let InitializationOutcome::Confirmed { installation_id } = reply.outcome else {
                    panic!("not confirmed")
                };
                if let Some(old) = observed {
                    assert_eq!(installation_id, old);
                }
                observed = Some(installation_id);
                assert_eq!(client.proof(), &proof);
            }
            drop(client);
        } else {
            let mut stream = stream;
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            FrameCodec::new(MAX_CLIENT_FRAME_BYTES)
                .write(
                    &mut stream,
                    &ClientEnvelope {
                        version,
                        request_id: 1,
                        proof: None,
                        request: ClientRequest::DeviceOperation {
                            operation: Operation::AccountInstallationInitialize {
                                request: InitializationRequest {
                                    request_id: Uuid::from_u128(107),
                                },
                            },
                        },
                    },
                )
                .unwrap();
            let reply = FrameCodec::new(MAX_FRAME_BYTES)
                .read::<ServerEnvelope>(&mut stream)
                .unwrap()
                .unwrap();
            assert!(
                matches!(reply.message, ServerMessage::Error(error) if error.code == ProtocolErrorCode::UnsupportedVersion)
            );
            assert!(!fixture.root.path().join("registry").exists());
        }
        serving.join().unwrap();
        assert!(!fixture.root.path().join("unused-homes").exists());
    }
}
