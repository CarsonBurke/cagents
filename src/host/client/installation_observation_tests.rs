//! Original-connection read fixtures. Every filesystem operand is private;
//! synthetic User/delegated roles never reinterpret inherited authority.
use super::{
    ClientRequest, ProtocolError, ProtocolErrorCode, ServerResponse, SharedHost, process_request,
};
use crate::{
    account::{home::Homes, registry::Store},
    agent_execution::{Authority, Lineage},
    device_service::{Operation, Reply},
    host::{
        account_preparation::{SeedSelection, with_fixture},
        state::{HostLock, HostState},
    },
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, TerminalId},
};
use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::Arc,
};
use uuid::Uuid;

const USER: u64 = 51;
const AGENT: u64 = 52;

struct Fixture {
    directory: tempfile::TempDir,
    host: SharedHost,
    store: Store,
    homes: Homes,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("installation-observation-host"),
            boot_id: BootId::new("installation-observation-boot"),
            host_incarnation: HostIncarnation::new("installation-observation-incarnation"),
        });
        state.register_client(USER);
        state.register_client(AGENT);
        state
            .agent_clients
            .insert(AGENT, Lineage(TerminalId::new()));
        Self {
            store: Store::at(directory.path().join("registry")),
            homes: Homes::at(directory.path().join("homes")),
            host: Arc::new(HostLock::new(state)),
            directory,
        }
    }

    fn run<T>(&self, run: impl FnOnce() -> T) -> T {
        with_fixture(
            self.store.clone(),
            self.homes.clone(),
            SeedSelection {
                // Deliberately absent: reading installation identity must never
                // require System source or managed-home discovery.
                system: self.directory.path().join("unused-system"),
                claude_global: Some(self.directory.path().join("unused-global.json")),
            },
            run,
        )
    }

    fn ask(&self, client: u64, request_id: Uuid) -> Result<Reply, ProtocolError> {
        self.run(|| dispatch(&self.host, client, request_id))
    }

    fn registry(&self, installation: Option<Uuid>) {
        fs::create_dir(self.directory.path().join("registry")).unwrap();
        write_private(
            &self.directory.path().join("registry/accounts.toml"),
            &format!(
                "version = 6\n{}",
                installation
                    .map(|id| format!("installation = '{id}'\n"))
                    .unwrap_or_default()
            ),
        );
    }
}

fn write_private(path: &Path, contents: &str) {
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(contents.as_bytes()).unwrap();
}

fn authenticate(host: &SharedHost, client: u64) {
    assert!(matches!(
        process_request(
            host,
            client,
            ClientRequest::Authenticate {
                authority: Authority::default(),
            }
        )
        .unwrap()
        .0,
        ServerResponse::Accepted
    ));
}

fn dispatch(host: &SharedHost, client: u64, request_id: Uuid) -> Result<Reply, ProtocolError> {
    let (response, extras, shutdown) = process_request(
        host,
        client,
        ClientRequest::DeviceOperation {
            operation: Operation::AccountInstallationRead { request_id },
        },
    )?;
    assert!(extras.is_empty() && !shutdown);
    let ServerResponse::DeviceOperation { reply } = response else {
        panic!("installation read must return its dedicated typed observation");
    };
    Ok(reply)
}

// Atime is intentionally excluded: reading a regular file can update it.
// Names, inode, permission bits and bytes must remain exactly unchanged.
fn snapshot(root: &Path) -> Vec<(std::path::PathBuf, u64, u32, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<(std::path::PathBuf, u64, u32, Vec<u8>)>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        entries.push((
            path.strip_prefix(root).unwrap().to_owned(),
            metadata.ino(),
            metadata.mode(),
            if metadata.is_file() {
                fs::read(path).unwrap()
            } else {
                vec![]
            },
        ));
        if metadata.is_dir() {
            let mut children = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                visit(root, &child, entries);
            }
        }
    }
    let mut entries = vec![];
    visit(root, root, &mut entries);
    entries
}

#[test]
fn installation_observation_requires_registered_explicit_user_and_sticky_delegation_denial() {
    let fixture = Fixture::new();
    let request_id = Uuid::from_u128(81);
    let before = snapshot(fixture.directory.path());
    for client in [USER, AGENT, 99] {
        assert_eq!(
            fixture.ask(client, request_id).unwrap_err().code,
            ProtocolErrorCode::PermissionDenied
        );
    }
    authenticate(&fixture.host, USER);
    assert_eq!(
        fixture.ask(USER, request_id).unwrap(),
        Reply::AccountInstallationObservation {
            request_id,
            installation_id: None
        }
    );
    authenticate(&fixture.host, AGENT);
    assert_eq!(
        fixture.ask(AGENT, request_id).unwrap_err().code,
        ProtocolErrorCode::PermissionDenied
    );
    authenticate(&fixture.host, 99);
    assert_eq!(
        fixture.ask(99, request_id).unwrap_err().code,
        ProtocolErrorCode::PermissionDenied
    );
    assert_eq!(
        process_request(
            &fixture.host,
            USER,
            ClientRequest::Authenticate {
                authority: Authority::session("invalid-installation-observation-fixture".into()),
            }
        )
        .unwrap_err()
        .code,
        ProtocolErrorCode::PermissionDenied
    );
    authenticate(&fixture.host, USER);
    assert_eq!(
        fixture.ask(USER, request_id).unwrap_err().code,
        ProtocolErrorCode::PermissionDenied
    );
    assert_eq!(snapshot(fixture.directory.path()), before);
}

#[test]
fn installation_observation_preserves_disconnect_recovery_shutdown_and_lifecycle_admission() {
    for edge in 0..6 {
        let fixture = Fixture::new();
        authenticate(&fixture.host, USER);
        let before = snapshot(fixture.directory.path());
        let expected = match edge {
            0 => {
                fixture.host.lock().unwrap().unregister_client(USER);
                ProtocolErrorCode::PermissionDenied
            }
            1 => {
                fixture.host.lock().unwrap().forget_client_delivery(USER);
                ProtocolErrorCode::PermissionDenied
            }
            2 => {
                fixture.host.lock().unwrap().register_client(USER);
                ProtocolErrorCode::PermissionDenied
            }
            3 => {
                fixture
                    .host
                    .lock()
                    .unwrap()
                    .retain_startup_recovery("original owner retained".into());
                ProtocolErrorCode::TerminalUnavailable
            }
            4 => {
                fixture.host.lock().unwrap().shutting_down = true;
                ProtocolErrorCode::TerminalUnavailable
            }
            _ => ProtocolErrorCode::InvalidRequest,
        };
        // Private metadata initialization is an explicit reservation fixture,
        // not a write by the observation operation.
        let reservation = (edge == 5).then(|| {
            crate::host::account_lifecycle::Reservation::test_install(
                &fixture.host,
                &[],
                fixture.store.clone(),
                &fixture.homes,
            )
            .unwrap()
        });
        let after_setup = snapshot(fixture.directory.path());
        assert_eq!(
            fixture.ask(USER, Uuid::from_u128(82)).unwrap_err().code,
            expected
        );
        assert_eq!(snapshot(fixture.directory.path()), after_setup);
        if edge != 5 {
            assert_eq!(after_setup, before);
        }
        drop(reservation);
    }
}

#[test]
fn installation_observation_has_no_generic_or_missing_fixture_fallback() {
    let request_id = Uuid::from_u128(83);
    assert!(
        crate::device_service::execute(Operation::AccountInstallationRead { request_id })
            .unwrap_err()
            .contains("explicit user and daemon admission")
    );
    let fixture = Fixture::new();
    authenticate(&fixture.host, USER);
    let before = snapshot(fixture.directory.path());
    assert_eq!(
        dispatch(&fixture.host, USER, request_id).unwrap_err().code,
        ProtocolErrorCode::InvalidRequest
    );
    assert_eq!(snapshot(fixture.directory.path()), before);
}

#[test]
fn installation_observation_refuses_nil_correlation_before_store_inspection() {
    let fixture = Fixture::new();
    authenticate(&fixture.host, USER);
    fixture.registry(None);
    fs::write(
        fixture.directory.path().join("registry/accounts.toml"),
        "private-invalid-fixture=[",
    )
    .unwrap();
    let before = snapshot(fixture.directory.path());
    let error = fixture.ask(USER, Uuid::nil()).unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
    assert_eq!(
        error.message,
        "Installation observation requires a nonempty request identity"
    );
    // A missing fixture gets the same admission refusal: no store was selected.
    let missing = dispatch(&fixture.host, USER, Uuid::nil()).unwrap_err();
    assert_eq!(missing.code, error.code);
    assert_eq!(missing.message, error.message);
    assert_eq!(snapshot(fixture.directory.path()), before);
}

#[test]
fn installation_observation_reads_existing_id_without_repair_or_mutation_and_sanitizes_errors() {
    for kind in 0..5 {
        let fixture = Fixture::new();
        authenticate(&fixture.host, USER);
        let installation_id = Uuid::from_u128(84);
        if kind != 0 {
            fixture.registry((kind != 1).then_some(installation_id));
        }
        let file = fixture.directory.path().join("registry/accounts.toml");
        if kind == 3 {
            fs::write(&file, "secret-private-fixture=[broken").unwrap();
        }
        if kind == 4 {
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let before = snapshot(fixture.directory.path());
        let request_id = Uuid::from_u128(85 + kind);
        if kind < 3 {
            assert_eq!(
                fixture.ask(USER, request_id).unwrap(),
                Reply::AccountInstallationObservation {
                    request_id,
                    installation_id: (kind == 2).then_some(installation_id),
                }
            );
        } else {
            let error = fixture.ask(USER, request_id).unwrap_err();
            assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
            assert_eq!(
                error.message,
                "The device installation identity could not be safely observed"
            );
        }
        assert_eq!(snapshot(fixture.directory.path()), before);
    }
}

#[test]
fn installation_observation_actual_original_control_connection_frames_none_and_some_with_uuid_echo()
{
    use std::{
        os::unix::net::UnixListener,
        thread,
        time::{Duration, Instant},
    };
    let fixture = Fixture::new();
    let socket = fixture.directory.path().join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let host = Arc::clone(&fixture.host);
    let store = fixture.store.clone();
    let homes = fixture.homes.clone();
    let source = SeedSelection {
        system: fixture.directory.path().join("unused-system"),
        claude_global: None,
    };
    let server = thread::spawn(move || {
        with_fixture(store, homes, source, || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "private observation wire connection missing"
                        );
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("private observation listener: {error}"),
                }
            };
            super::serve(stream, 73, host);
        })
    });
    let mut client = crate::control::ControlClient::connect(&socket).unwrap();
    client.authenticate(Authority::default()).unwrap();
    let proof = client.proof().clone();
    let before_missing = snapshot(fixture.directory.path());
    let first = Uuid::from_u128(90);
    assert_eq!(
        client
            .device_operation(Operation::AccountInstallationRead { request_id: first })
            .unwrap(),
        Reply::AccountInstallationObservation {
            request_id: first,
            installation_id: None
        }
    );
    assert_eq!(snapshot(fixture.directory.path()), before_missing);
    let installation_id = Uuid::from_u128(91);
    fixture.registry(Some(installation_id));
    let foreign = fixture.directory.path().join("foreign");
    fs::create_dir(&foreign).unwrap();
    write_private(
        &foreign.join("accounts.toml"),
        &format!("version=6\ninstallation='{}'\n", Uuid::from_u128(92)),
    );
    let before_existing = snapshot(fixture.directory.path());
    for request_id in [Uuid::from_u128(93), Uuid::from_u128(94)] {
        assert_eq!(
            client
                .device_operation(Operation::AccountInstallationRead { request_id })
                .unwrap(),
            Reply::AccountInstallationObservation {
                request_id,
                installation_id: Some(installation_id)
            }
        );
        assert_eq!(client.proof(), &proof);
        assert_eq!(snapshot(fixture.directory.path()), before_existing);
    }
    drop(client);
    server.join().unwrap();
    assert!(!fixture.directory.path().join("homes").exists());
    assert!(!fixture.directory.path().join("unused-system").exists());
    assert!(
        !fixture
            .directory
            .path()
            .join("registry/accounts.toml.lock")
            .exists()
    );
}

#[test]
fn installation_observation_actual_host_wire_rejects_anonymous_sticky_delegation_and_foreign_proof()
{
    use crate::terminal_ipc::{
        ClientEnvelope, PROTOCOL_VERSION, ServerEnvelope, ServerMessage, read_frame, write_frame,
    };
    use std::{os::unix::net::UnixStream, thread, time::Duration};
    let fixture = Fixture::new();
    let proof = fixture.host.lock().unwrap().proof().clone();
    let (mut stream, peer) = UnixStream::pair().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let host = Arc::clone(&fixture.host);
    // Every request below is refused before store resolution; missing TLS
    // would fail closed even if a regression accidentally reached dispatch.
    let server = thread::spawn(move || super::serve(peer, 74, host));
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
        let envelope: ServerEnvelope = read_frame(stream).unwrap().unwrap();
        assert_eq!(envelope.request_id, Some(id));
        envelope.message
    };
    assert!(matches!(
        exchange(
            &mut stream,
            1,
            ClientRequest::Connect {
                expected_host: Some(proof.clone()),
                resume: vec![]
            },
            None
        ),
        ServerMessage::Response {
            response: ServerResponse::Connected { .. }
        }
    ));
    let request = || ClientRequest::DeviceOperation {
        operation: Operation::AccountInstallationRead {
            request_id: Uuid::from_u128(95),
        },
    };
    let denied = |message| {
        let ServerMessage::Error(error) = message else {
            panic!("observation admission must refuse");
        };
        assert_eq!(error.code, ProtocolErrorCode::PermissionDenied);
    };
    denied(exchange(&mut stream, 2, request(), Some(proof.clone())));
    denied(exchange(
        &mut stream,
        3,
        ClientRequest::Authenticate {
            authority: Authority::session("invalid-wire-observation-fixture".into()),
        },
        Some(proof.clone()),
    ));
    assert!(matches!(
        exchange(
            &mut stream,
            4,
            ClientRequest::Authenticate {
                authority: Authority::default()
            },
            Some(proof.clone())
        ),
        ServerMessage::Response {
            response: ServerResponse::Accepted
        }
    ));
    denied(exchange(&mut stream, 5, request(), Some(proof.clone())));
    let mut foreign = proof;
    foreign.host_incarnation = HostIncarnation::new("foreign-observation-incarnation");
    let ServerMessage::Error(error) = exchange(&mut stream, 6, request(), Some(foreign)) else {
        panic!("foreign proof must fail before observation");
    };
    assert_eq!(error.code, ProtocolErrorCode::ProofMismatch);
    drop(stream);
    server.join().unwrap();
    assert_eq!(snapshot(fixture.directory.path()).len(), 1);
}
