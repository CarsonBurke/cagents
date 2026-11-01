//! Private filesystem and fixed User-role fixtures. No provider or inherited
//! HOME/opaque authority is inspected or rewritten by these tests.
use super::*;
use crate::{
    account::{
        preparation::{PreparationBinding, PreparationPayload},
        registry::Registered,
    },
    agent_execution::Authority,
    device_service::{Operation, Reply},
    host::state::{HostLock, HostState},
    model::{BootId, HostIncarnation, HostSessionId},
    terminal_ipc::ServerResponse,
};
use std::{fs, os::unix::fs::MetadataExt};
use uuid::Uuid;

const USER: ClientId = 51;

struct Fixture {
    directory: tempfile::TempDir,
    host: SharedHost,
    store: Store,
    homes: Homes,
    source: SeedSelection,
}

impl Fixture {
    fn new(provider: Provider) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system");
        fs::create_dir(&system).unwrap();
        let claude_global =
            (provider == Provider::Claude).then(|| directory.path().join(".claude.json"));
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("physical-create-fixture"),
            boot_id: BootId::new("physical-create-boot"),
            host_incarnation: HostIncarnation::new("physical-create-incarnation"),
        });
        state.register_client(USER);
        let host = Arc::new(HostLock::new(state));
        let accepted = super::super::client::process_request(
            &host,
            USER,
            ClientRequest::Authenticate {
                authority: Authority::default(),
            },
        )
        .unwrap()
        .0;
        assert!(matches!(accepted, ServerResponse::Accepted));
        Self {
            store: Store::at(directory.path().join("registry")),
            homes: Homes::at(directory.path().join("homes")),
            source: SeedSelection {
                system,
                claude_global,
            },
            directory,
            host,
        }
    }

    fn run<T>(&self, run: impl FnOnce() -> T) -> T {
        with_fixture(
            self.store.clone(),
            self.homes.clone(),
            self.source.clone(),
            run,
        )
    }

    fn ask(&self, request: AccountPreparationRequest) -> AccountPreparationReply {
        let expected = request.clone();
        self.run(|| {
            let (response, extra, shutdown) = super::super::client::process_request(
                &self.host,
                USER,
                ClientRequest::DeviceOperation {
                    operation: Operation::AccountPreparation { request },
                },
            )
            .unwrap();
            assert!(extra.is_empty() && !shutdown);
            let ServerResponse::DeviceOperation {
                reply: Reply::AccountPreparation(reply),
            } = response
            else {
                panic!("Create must return its typed preparation reply");
            };
            assert_eq!(reply.request, expected);
            reply
        })
    }

    fn plan(&self, provider: Provider, copy: bool) -> Preparation {
        record(self.ask(AccountPreparationRequest::Preparation(
            PreparationRequest::Plan {
                request_id: Uuid::new_v4(),
                payload: PreparationPayload {
                    provider,
                    copy_codex_config: copy,
                },
            },
        )))
    }

    fn request(binding: &PreparationBinding) -> AccountPreparationRequest {
        AccountPreparationRequest::Preparation(PreparationRequest::Create {
            binding: binding.clone(),
        })
    }

    fn reserve(&self, binding: &PreparationBinding) -> PreparedCreationReservation {
        let request = ClientRequest::DeviceOperation {
            operation: Operation::AccountPreparation {
                request: Self::request(binding),
            },
        };
        let caller = OriginalCaller::capture(&self.host, USER).unwrap();
        let PreparedAdmission::Reserved(reservation) = PreparedCreationReservation::reserve(
            &self.host,
            self.store.clone(),
            self.homes.clone(),
            binding,
            || Ok(self.source.clone()),
            || caller.revalidate(&self.host, &request),
        )
        .unwrap() else {
            panic!("fixture should reserve Planned exactly");
        };
        reservation
    }
}

fn record(reply: AccountPreparationReply) -> Preparation {
    match reply.outcome {
        AccountPreparationOutcome::Confirmed {
            record: AccountPreparationRecord::Preparation(record),
        } => record,
        other => panic!("expected confirmed metadata: {other:?}"),
    }
}

#[test]
fn admitted_host_create_uses_planned_key_and_persisted_copy_policy_without_provider() {
    for copy in [true, false] {
        let fixture = Fixture::new(Provider::Codex);
        let config = b"model = 'private-fixture'\n";
        fs::write(fixture.source.system.join("config.toml"), config).unwrap();
        let planned = fixture.plan(Provider::Codex, copy);
        let created = record(fixture.ask(Fixture::request(&planned.binding)));
        assert_eq!(created.binding, planned.binding);
        assert_eq!(created.state, PreparationState::Created);
        let home = fixture.homes.path(Provider::Codex, planned.binding.key);
        assert_eq!(fs::read(home.join("config.toml")).unwrap(), config);
        assert_eq!(
            fs::symlink_metadata(home.join("config.toml"))
                .unwrap()
                .file_type()
                .is_symlink(),
            !copy
        );
        let registry = fixture.store.load().unwrap();
        assert!(registry.registered().is_empty());
        assert_eq!(registry.pending().len(), 1);
        assert_eq!(registry.pending()[0].key, planned.binding.key);
        assert_eq!(fs::read_dir(home.parent().unwrap()).unwrap().count(), 1);
        if copy {
            assert_eq!(
                fs::metadata(home.join("config.toml")).unwrap().mode() & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn retained_replay_precedes_unavailable_source_and_never_reopens_home_or_pending() {
    for state in [
        PreparationState::Uncertain,
        PreparationState::Created,
        PreparationState::CancelledRetained,
        PreparationState::Registered,
    ] {
        let mut fixture = Fixture::new(Provider::Codex);
        fs::write(
            fixture.source.system.join("config.toml"),
            "model='original'\n",
        )
        .unwrap();
        let planned = fixture.plan(Provider::Codex, true);
        if state == PreparationState::CancelledRetained {
            fixture
                .store
                .cancel_preparation(&fixture.homes, &planned.binding)
                .unwrap();
        } else if state == PreparationState::Uncertain {
            let reservation = fixture.reserve(&planned.binding);
            assert!(
                reservation
                    .create_for_test(|_| Err(io::Error::other("fixture after mkdir")), |_| Ok(()))
                    .is_err()
            );
            drop(reservation);
        } else {
            record(fixture.ask(Fixture::request(&planned.binding)));
            if state == PreparationState::Registered {
                fixture
                    .store
                    .update(|registry| {
                        registry
                            .register(
                                Registered {
                                    key: planned.binding.key,
                                    provider: Provider::Codex,
                                    linked_home: None,
                                    label: None,
                                    email: Some("fixture@example.invalid".into()),
                                    organization: None,
                                    added_at: 1,
                                },
                                &[],
                            )
                            .unwrap();
                        registry.unregister(planned.binding.key);
                    })
                    .unwrap();
            }
        }
        let expected = fixture
            .store
            .load()
            .unwrap()
            .preparation(planned.binding.request_id)
            .unwrap()
            .clone();
        assert_eq!(expected.state, state);
        let before = fs::read(fixture.directory.path().join("registry/accounts.toml")).unwrap();
        let home = fixture.homes.path(Provider::Codex, planned.binding.key);
        if home.exists() {
            fs::rename(&home, home.with_extension("retained-original")).unwrap();
        }
        fs::rename(
            &fixture.source.system,
            fixture.directory.path().join("original-system"),
        )
        .unwrap();
        fixture.source.system = fixture.directory.path().join("unavailable-system");
        let replay = record(fixture.ask(Fixture::request(&planned.binding)));
        assert_eq!(replay, expected);
        assert_eq!(
            fs::read(fixture.directory.path().join("registry/accounts.toml")).unwrap(),
            before
        );
        assert!(!home.exists());
        assert!(!fixture.source.system.exists());
        assert!(fixture.store.load().unwrap().registered().is_empty());
    }
}

#[test]
fn every_changed_binding_is_rejected_before_physical_source_capture() {
    let fixture = Fixture::new(Provider::Codex);
    let planned = fixture.plan(Provider::Codex, true);
    let mut changed = Vec::new();
    let mut binding = planned.binding.clone();
    binding.request_id = Uuid::new_v4();
    changed.push(binding);
    let mut binding = planned.binding.clone();
    binding.installation_id = Uuid::new_v4();
    changed.push(binding);
    let mut binding = planned.binding.clone();
    binding.key = Uuid::new_v4();
    changed.push(binding);
    let mut binding = planned.binding.clone();
    binding.payload.provider = Provider::Claude;
    changed.push(binding);
    let mut binding = planned.binding.clone();
    binding.payload.copy_codex_config = false;
    changed.push(binding);
    let before = fs::read(fixture.directory.path().join("registry/accounts.toml")).unwrap();
    for binding in changed {
        assert!(
            PreparedCreationReservation::reserve(
                &fixture.host,
                fixture.store.clone(),
                fixture.homes.clone(),
                &binding,
                || panic!("mismatched binding reached source capture"),
                || panic!("mismatched binding reached physical admission"),
            )
            .is_err()
        );
        assert!(matches!(
            fixture.ask(Fixture::request(&binding)).outcome,
            AccountPreparationOutcome::ReconcileRequired { observed: None, .. }
        ));
        assert_eq!(
            fs::read(fixture.directory.path().join("registry/accounts.toml")).unwrap(),
            before
        );
    }
    let foreign = Store::at(fixture.directory.path().join("foreign"));
    let foreign_plan = foreign
        .plan_preparation(Uuid::new_v4(), planned.binding.payload)
        .unwrap();
    assert!(matches!(
        fixture.ask(Fixture::request(&foreign_plan.binding)).outcome,
        AccountPreparationOutcome::ReconcileRequired { observed: None, .. }
    ));
    assert!(!fixture.directory.path().join("homes").exists());
}

#[test]
fn final_store_lease_preserves_a_cancellation_that_won_after_physical_admission() {
    let fixture = Fixture::new(Provider::Codex);
    let planned = fixture.plan(Provider::Codex, true);
    let reservation = fixture.reserve(&planned.binding);
    let cancelled = fixture
        .store
        .cancel_preparation(&fixture.homes, &planned.binding)
        .unwrap();
    assert_eq!(
        reservation
            .create_for_test(
                |_| panic!("cancelled attempt minted a receipt"),
                |_| panic!("cancelled attempt seeded")
            )
            .unwrap(),
        cancelled
    );
    assert!(
        !fixture
            .homes
            .path(Provider::Codex, planned.binding.key)
            .exists()
    );
    assert!(fixture.store.load().unwrap().pending().is_empty());
}

#[test]
fn source_retarget_before_consumption_cannot_supply_replacement_configuration() {
    let fixture = Fixture::new(Provider::Codex);
    fs::write(fixture.source.system.join("config.toml"), "original").unwrap();
    let planned = fixture.plan(Provider::Codex, true);
    let reservation = fixture.reserve(&planned.binding);
    fs::rename(
        &fixture.source.system,
        fixture.directory.path().join("pinned-source"),
    )
    .unwrap();
    fs::create_dir(&fixture.source.system).unwrap();
    fs::write(fixture.source.system.join("config.toml"), "replacement").unwrap();
    assert!(reservation.create().is_err());
    assert_eq!(
        fixture
            .store
            .load()
            .unwrap()
            .preparation(planned.binding.request_id)
            .unwrap()
            .state,
        PreparationState::Planned
    );
    assert!(
        !fixture
            .homes
            .path(Provider::Codex, planned.binding.key)
            .exists()
    );
}

#[test]
fn malformed_source_stays_planned_while_post_mkdir_failure_stays_uncertain() {
    let fixture = Fixture::new(Provider::Claude);
    fs::write(fixture.source.claude_global.as_ref().unwrap(), "not JSON").unwrap();
    let planned = fixture.plan(Provider::Claude, true);
    let reply = fixture.ask(Fixture::request(&planned.binding));
    assert!(matches!(
        reply.outcome,
        AccountPreparationOutcome::ReconcileRequired {
            observed: Some(_),
            ..
        }
    ));
    assert_eq!(
        fixture
            .store
            .load()
            .unwrap()
            .preparation(planned.binding.request_id)
            .unwrap()
            .state,
        PreparationState::Planned
    );
    assert!(
        !fixture
            .homes
            .path(Provider::Claude, planned.binding.key)
            .exists()
    );

    fs::write(fixture.source.claude_global.as_ref().unwrap(), r#"{"mcpServers":{"fixture":{"command":"local-fixture"}},"oauthAccount":{"secret":"do-not-copy"}}"#).unwrap();
    fs::write(
        fixture.source.system.join("settings.json"),
        r#"{"fixture":true}"#,
    )
    .unwrap();
    let reservation = fixture.reserve(&planned.binding);
    let error = reservation
        .create_for_test(
            |receipt| {
                // A fixed fixture collision after fresh receipt enrollment. The seed
                // writer must retain it, and must not truncate or unlink it.
                fs::write(receipt.home().join(".claude.json"), "collision")
            },
            |_| Ok(()),
        )
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    drop(reservation);
    let home = fixture.homes.path(Provider::Claude, planned.binding.key);
    assert_eq!(
        fs::read_to_string(home.join(".claude.json")).unwrap(),
        "collision"
    );
    assert_eq!(
        fs::read_to_string(home.join("settings.json")).unwrap(),
        r#"{"fixture":true}"#
    );
    assert_eq!(
        fixture
            .store
            .load()
            .unwrap()
            .preparation(planned.binding.request_id)
            .unwrap()
            .state,
        PreparationState::Uncertain
    );
    assert!(fixture.store.load().unwrap().pending().is_empty());
    let before = fs::read(home.join(".claude.json")).unwrap();
    assert_eq!(
        record(fixture.ask(Fixture::request(&planned.binding))).state,
        PreparationState::Uncertain
    );
    assert_eq!(fs::read(home.join(".claude.json")).unwrap(), before);
}

#[test]
fn enrollment_writer_failure_retains_unseeded_consumed_home_without_recreation() {
    let fixture = Fixture::new(Provider::Codex);
    fs::write(fixture.source.system.join("config.toml"), "original").unwrap();
    let planned = fixture.plan(Provider::Codex, true);
    let reservation = fixture.reserve(&planned.binding);
    let gate = fixture.host.lock().unwrap().control_gate.clone();
    let reader = gate.read().unwrap();
    let error = reservation.create().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    drop(reader);
    drop(reservation);
    let home = fixture.homes.path(Provider::Codex, planned.binding.key);
    assert!(home.is_dir());
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
    assert_eq!(
        fixture
            .store
            .load()
            .unwrap()
            .preparation(planned.binding.request_id)
            .unwrap()
            .state,
        PreparationState::Uncertain
    );
    assert!(fixture.store.load().unwrap().pending().is_empty());
    assert_eq!(
        record(fixture.ask(Fixture::request(&planned.binding))).state,
        PreparationState::Uncertain
    );
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
}

#[test]
fn lost_creation_completion_after_actual_seed_sync_never_confirms_created() {
    let fixture = Fixture::new(Provider::Codex);
    fs::write(fixture.source.system.join("config.toml"), "original").unwrap();
    let planned = fixture.plan(Provider::Codex, true);
    let reservation = fixture.reserve(&planned.binding);
    // Inject the lost completion boundary after actual seed/receipt sync and
    // before Store can publish Created. This is not a power-loss experiment.
    let completed = std::cell::Cell::new(false);
    let error = reservation
        .create_for_test(
            |_| Ok(()),
            |_| {
                completed.set(true);
                Err(io::Error::from_raw_os_error(libc::EIO))
            },
        )
        .unwrap_err();
    assert!(
        completed.get(),
        "the actual seed and receipt sync must precede injected completion loss"
    );
    assert_eq!(error.raw_os_error(), Some(libc::EIO));
    drop(reservation);
    assert_eq!(
        fs::read_to_string(
            fixture
                .homes
                .path(Provider::Codex, planned.binding.key)
                .join("config.toml")
        )
        .unwrap(),
        "original"
    );
    assert_eq!(
        record(fixture.ask(Fixture::request(&planned.binding))).state,
        PreparationState::Uncertain
    );
    assert!(fixture.store.load().unwrap().pending().is_empty());
}

#[test]
fn early_create_requires_original_user_and_recovery_shutdown_admission_before_storage() {
    for case in 0..5 {
        let fixture = Fixture::new(Provider::Codex);
        let planned = fixture.plan(Provider::Codex, true);
        let before = fs::read(fixture.directory.path().join("registry/accounts.toml")).unwrap();
        let expected = match case {
            0 => {
                fixture
                    .host
                    .lock()
                    .unwrap()
                    .account_preparation_callers
                    .remove(&USER);
                ProtocolErrorCode::PermissionDenied
            }
            1 => {
                fixture.host.lock().unwrap().agent_clients.insert(
                    USER,
                    crate::agent_execution::Lineage(crate::model::TerminalId::new()),
                );
                ProtocolErrorCode::PermissionDenied
            }
            2 => {
                assert!(
                    super::super::client::process_request(
                        &fixture.host,
                        USER,
                        ClientRequest::Authenticate {
                            authority: Authority::session("invalid-private-create-fixture".into())
                        }
                    )
                    .is_err()
                );
                ProtocolErrorCode::PermissionDenied
            }
            3 => {
                fixture
                    .host
                    .lock()
                    .unwrap()
                    .retain_startup_recovery("private original owner retained".into());
                ProtocolErrorCode::TerminalUnavailable
            }
            _ => {
                fixture.host.lock().unwrap().shutting_down = true;
                ProtocolErrorCode::TerminalUnavailable
            }
        };
        let error = fixture
            .run(|| {
                super::super::client::process_request(
                    &fixture.host,
                    USER,
                    ClientRequest::DeviceOperation {
                        operation: Operation::AccountPreparation {
                            request: Fixture::request(&planned.binding),
                        },
                    },
                )
            })
            .unwrap_err();
        assert_eq!(error.code, expected);
        assert_eq!(
            fs::read(fixture.directory.path().join("registry/accounts.toml")).unwrap(),
            before
        );
        assert!(!fixture.directory.path().join("homes").exists());
    }
}

#[test]
fn preparation_fixture_operands_fail_closed_when_not_installed_on_handler_thread() {
    let fixture = Fixture::new(Provider::Codex);
    let planned = fixture.plan(Provider::Codex, true);
    let before = fs::read(fixture.directory.path().join("registry/accounts.toml")).unwrap();
    for request in [
        AccountPreparationRequest::Preparation(PreparationRequest::Plan {
            // A restored ambient fallback would return a storage-stage error
            // for this nil ID without writing real account metadata. Missing
            // fixture admission must refuse even before that stage.
            request_id: Uuid::nil(),
            payload: planned.binding.payload.clone(),
        }),
        Fixture::request(&planned.binding),
    ] {
        let expected = request.clone();
        let (response, _, _) = super::super::client::process_request(
            &fixture.host,
            USER,
            ClientRequest::DeviceOperation {
                operation: Operation::AccountPreparation { request },
            },
        )
        .unwrap();
        let ServerResponse::DeviceOperation {
            reply: Reply::AccountPreparation(reply),
        } = response
        else {
            panic!("missing fixture must retain the typed preparation response");
        };
        assert_eq!(reply.request, expected);
        assert!(matches!(
            reply.outcome,
            AccountPreparationOutcome::NotAttempted { .. }
        ));
    }
    assert!(seed_selection(Provider::Codex).is_err());
    assert_eq!(
        fs::read(fixture.directory.path().join("registry/accounts.toml")).unwrap(),
        before
    );
    assert!(!fixture.directory.path().join("homes").exists());
}

#[test]
fn terminal_constructor_check_precedes_source_and_original_user_identity_is_rechecked() {
    let fixture = Fixture::new(Provider::Codex);
    let planned = fixture.plan(Provider::Codex, true);
    fixture
        .store
        .cancel_preparation(&fixture.homes, &planned.binding)
        .unwrap();
    assert!(matches!(
        PreparedCreationReservation::reserve(
            &fixture.host,
            fixture.store.clone(),
            fixture.homes.clone(),
            &planned.binding,
            || panic!("retained replay inspected System"),
            || panic!("retained replay installed physical authority"),
        )
        .unwrap(),
        PreparedAdmission::ReplayNeeded
    ));

    let planned = fixture.plan(Provider::Codex, true);
    let request = ClientRequest::DeviceOperation {
        operation: Operation::AccountPreparation {
            request: Fixture::request(&planned.binding),
        },
    };
    let original = OriginalCaller::capture(&fixture.host, USER).unwrap();
    let result = PreparedCreationReservation::reserve(
        &fixture.host,
        fixture.store.clone(),
        fixture.homes.clone(),
        &planned.binding,
        || Ok(fixture.source.clone()),
        || {
            // Numeric ID reuse and a newly authenticated User do not replace
            // the original admitted delivery identity.
            {
                let mut state = fixture.host.lock().unwrap();
                state.register_client(USER);
                state.account_preparation_callers.insert(
                    USER,
                    super::super::agent_execution::terminals::AccountPreparationCaller::User,
                );
            }
            original.revalidate(&fixture.host, &request)
        },
    );
    assert!(matches!(
        result,
        Err(ProtocolError {
            code: ProtocolErrorCode::PermissionDenied,
            ..
        })
    ));
    assert!(
        !fixture
            .homes
            .path(Provider::Codex, planned.binding.key)
            .exists()
    );
    assert_eq!(
        fixture
            .store
            .load()
            .unwrap()
            .preparation(planned.binding.request_id)
            .unwrap()
            .state,
        PreparationState::Planned
    );
}

#[test]
fn effective_source_provenance_keeps_default_claude_global_at_original_home() {
    let environment = BTreeMap::from([
        ("HOME", "/private-fixture/user".to_owned()),
        (
            "CLAUDE_CONFIG_DIR",
            "/private-fixture/accounts/claude/00000000-0000-0000-0000-000000000001".to_owned(),
        ),
    ]);
    let selected =
        SeedSelection::from_environment(Provider::Claude, &|name| environment.get(name).cloned())
            .unwrap();
    assert_eq!(
        selected.system,
        PathBuf::from("/private-fixture/user/.claude")
    );
    assert_eq!(
        selected.claude_global,
        Some(PathBuf::from("/private-fixture/user/.claude.json"))
    );
    let overridden = SeedSelection::from_environment(Provider::Claude, &|name| match name {
        "HOME" => Some("/private-fixture/user".into()),
        "CLAUDE_CONFIG_DIR" => Some("/private-fixture/custom".into()),
        _ => None,
    })
    .unwrap();
    assert_eq!(
        overridden.claude_global,
        Some(PathBuf::from("/private-fixture/custom/.claude.json"))
    );
}

#[test]
fn ignored_inherited_claude_selector_never_seeds_its_poison_configuration() {
    for marked in [false, true] {
        let mut fixture = Fixture::new(Provider::Claude);
        let user_home = fixture.directory.path().join("user");
        fs::create_dir_all(user_home.join(".claude")).unwrap();
        fs::write(
            user_home.join(".claude/settings.json"),
            "original-system-settings",
        )
        .unwrap();
        fs::write(user_home.join(".claude.json"), r#"{"mcpServers":{"fixture":{"command":"private-fixture"}},"oauthAccount":{"secret":"not-copied"}}"#).unwrap();
        let ignored = if marked {
            fixture.directory.path().join("ignored-selection")
        } else {
            fixture
                .directory
                .path()
                .join("accounts/claude")
                .join(Uuid::new_v4().to_string())
        };
        fs::create_dir_all(&ignored).unwrap();
        fs::write(ignored.join("settings.json"), "wrong selected settings").unwrap();
        fs::write(
            ignored.join(".claude.json"),
            "malformed ignored provider state",
        )
        .unwrap();
        let mut environment = BTreeMap::from([
            ("HOME", user_home.to_string_lossy().into_owned()),
            ("CLAUDE_CONFIG_DIR", ignored.to_string_lossy().into_owned()),
        ]);
        if marked {
            environment.insert(
                crate::account::SELECTION_MARKER,
                format!("CLAUDE_CONFIG_DIR={}", ignored.display()),
            );
        }
        fixture.source = SeedSelection::from_environment(Provider::Claude, &|name| {
            environment.get(name).cloned()
        })
        .unwrap();
        let planned = fixture.plan(Provider::Claude, false);
        assert_eq!(
            record(fixture.ask(Fixture::request(&planned.binding))).state,
            PreparationState::Created
        );
        let home = fixture.homes.path(Provider::Claude, planned.binding.key);
        assert_eq!(
            fs::read_to_string(home.join("settings.json")).unwrap(),
            "original-system-settings"
        );
        let global: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".claude.json")).unwrap()).unwrap();
        assert!(global.get("mcpServers").is_some());
        assert!(global.get("oauthAccount").is_none());
        assert_eq!(
            fs::read_to_string(ignored.join(".claude.json")).unwrap(),
            "malformed ignored provider state"
        );
    }
}

#[test]
fn actual_framed_user_plan_and_create_use_only_private_selected_store_and_source() {
    use std::{
        os::unix::net::UnixListener,
        thread,
        time::{Duration, Instant},
    };
    let fixture = Fixture::new(Provider::Codex);
    fs::write(
        fixture.source.system.join("config.toml"),
        "model='wire-fixture'\n",
    )
    .unwrap();
    let socket = fixture.directory.path().join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let host = Arc::clone(&fixture.host);
    let store = fixture.store.clone();
    let homes = fixture.homes.clone();
    let source = fixture.source.clone();
    let server = thread::spawn(move || {
        with_fixture(store, homes, source, || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "private Create wire connection was not opened"
                        );
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("private Create listener: {error}"),
                }
            };
            super::super::client::serve(stream, 72, host);
        })
    });
    let mut client = crate::control::ControlClient::connect(&socket).unwrap();
    client.authenticate(Authority::default()).unwrap();
    let plan = AccountPreparationRequest::Preparation(PreparationRequest::Plan {
        request_id: Uuid::new_v4(),
        payload: PreparationPayload {
            provider: Provider::Codex,
            copy_codex_config: true,
        },
    });
    let Reply::AccountPreparation(reply) = client
        .device_operation(Operation::AccountPreparation {
            request: plan.clone(),
        })
        .unwrap()
    else {
        panic!("typed Plan reply");
    };
    assert_eq!(reply.request, plan);
    let planned = record(reply);
    let request = Fixture::request(&planned.binding);
    let Reply::AccountPreparation(reply) = client
        .device_operation(Operation::AccountPreparation {
            request: request.clone(),
        })
        .unwrap()
    else {
        panic!("typed Create reply");
    };
    assert_eq!(reply.request, request);
    assert_eq!(record(reply).state, PreparationState::Created);
    drop(client);
    server.join().unwrap();
    assert_eq!(
        fs::read_to_string(
            fixture
                .homes
                .path(Provider::Codex, planned.binding.key)
                .join("config.toml")
        )
        .unwrap(),
        "model='wire-fixture'\n"
    );
    assert!(fixture.store.load().unwrap().registered().is_empty());
}
