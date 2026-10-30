//! Actual private filesystem and fixed synthetic User policy operands only.
//! No provider, inherited HOME, opaque authority or ambient installation read.
use super::*;
use crate::{
    account::fresh_code::{CodeAttempt, CodeMethod},
    account::preparation::PreparationPayload,
    agent_execution::Authority,
    host::{
        account_preparation::{SeedSelection, with_fixture},
        state::{HostLock, HostState},
    },
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, TerminalId},
    terminal_ipc::ServerResponse,
};
use std::os::unix::fs::PermissionsExt;

const USER: ClientId = 901;

struct Fixture {
    directory: tempfile::TempDir,
    host: SharedHost,
    store: Store,
    homes: Homes,
    source: SeedSelection,
    installation: Uuid,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("registry"));
        // Establish the fixture installation independently of the conditional
        // physical action. This older metadata plan never enters a provider.
        let bootstrap = store
            .plan_preparation(
                Uuid::new_v4(),
                PreparationPayload {
                    provider: Provider::Codex,
                    copy_codex_config: true,
                },
            )
            .unwrap();
        let system = directory.path().join("system");
        fs::create_dir(&system).unwrap();
        fs::write(system.join("config.toml"), "model='private-code-fixture'\n").unwrap();
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("private-code-host"),
            boot_id: BootId::new("private-code-boot"),
            host_incarnation: HostIncarnation::new("private-code-incarnation"),
        });
        state.register_client(USER);
        let host = Arc::new(HostLock::new(state));
        authenticate(&host);
        Self {
            homes: Homes::at(directory.path().join("homes")),
            directory,
            store,
            host,
            source: SeedSelection {
                system,
                claude_global: None,
            },
            installation: bootstrap.binding.installation_id,
        }
    }

    fn run<T>(&self, effect: impl FnOnce() -> T) -> T {
        with_fixture(
            self.store.clone(),
            self.homes.clone(),
            self.source.clone(),
            effect,
        )
    }

    fn request(&self) -> FreshCodeStart {
        let planned = self
            .run(|| {
                conditional_plan(
                    &self.host,
                    USER,
                    &ConditionalPlan {
                        request_id: Uuid::new_v4(),
                        expected_installation: self.installation,
                        payload: PreparationPayload {
                            provider: Provider::Codex,
                            copy_codex_config: true,
                        },
                    },
                )
            })
            .unwrap()
            .unwrap();
        FreshCodeStart {
            binding: planned.binding,
            attempt: CodeAttempt {
                id: Uuid::new_v4(),
                terminal_id: TerminalId::new(),
                method: CodeMethod::CodexDeviceCode,
            },
        }
    }

    fn ready(&self, request: &FreshCodeStart) -> Ready {
        let PreparedStart::Ready(ready) = self
            .run(|| prepare_start(&self.host, USER, request))
            .unwrap()
        else {
            panic!("the actual private creation must produce its new owner and original root");
        };
        ready
    }
}

fn authenticate(host: &SharedHost) {
    assert!(matches!(
        super::super::client::process_request(
            host,
            USER,
            ClientRequest::Authenticate {
                authority: Authority::default()
            }
        )
        .unwrap()
        .0,
        ServerResponse::Accepted
    ));
}

#[test]
fn actual_created_root_and_new_durable_owner_allow_one_constructor_claim_only() {
    let fixture = Fixture::new();
    let request = fixture.request();
    let ready = fixture.ready(&request);
    assert_eq!(ready.owner.binding, request.binding);
    assert_eq!(ready.owner.attempt, request.attempt);
    assert!(
        ready.root.matches(
            &fs::metadata(fixture.homes.path(Provider::Codex, request.binding.key)).unwrap()
        )
    );
    let admission = ready
        .admission(
            PathBuf::from("/private-fixture/codex"),
            fixture.directory.path().to_owned(),
            None,
        )
        .unwrap();
    let lease = admission.authorize(admission.recipe()).unwrap();
    assert_eq!(
        fixture
            .store
            .try_cancel_preparation(&fixture.homes, &request.binding)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    drop(lease);
    assert!(admission.authorize(admission.recipe()).is_err());
    // Claiming is not a provider-start receipt. No terminal was constructed.
    assert_eq!(
        fixture
            .store
            .observe_fresh_code(&request)
            .unwrap()
            .unwrap()
            .state,
        crate::account::fresh_code::CodeState::Owned
    );
}

#[test]
fn loaded_owner_and_created_metadata_cannot_reconstruct_the_original_pin() {
    let fixture = Fixture::new();
    let request = fixture.request();
    drop(fixture.ready(&request));
    let observed = fixture.store.observe_fresh_code(&request).unwrap().unwrap();
    let result = fixture
        .run(|| prepare_start(&fixture.host, USER, &request))
        .unwrap();
    assert!(
        matches!(result, PreparedStart::Retained { preparation, owner: Some(owner) }
        if preparation.state == PreparationState::Created && owner == observed)
    );
    assert_eq!(fixture.store.load().unwrap().pending().len(), 1);
}

#[test]
fn foreign_replacement_root_cannot_supply_configuration_or_constructor_authority() {
    let fixture = Fixture::new();
    let request = fixture.request();
    let ready = fixture.ready(&request);
    let path = fixture.homes.path(Provider::Codex, request.binding.key);
    let original = fixture.directory.path().join("actual-created-root");
    fs::rename(&path, &original).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("config.toml"), "foreign replacement").unwrap();
    assert!(
        ready
            .admission(
                PathBuf::from("/private-fixture/codex"),
                fixture.directory.path().to_owned(),
                None
            )
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(path.join("config.toml")).unwrap(),
        "foreign replacement"
    );
    assert_eq!(
        fs::read_to_string(original.join("config.toml")).unwrap(),
        "model='private-code-fixture'\n"
    );
    assert!(
        fixture
            .store
            .observe_fresh_code(&request)
            .unwrap()
            .is_some()
    );
}

#[test]
fn original_user_delivery_and_current_cancel_are_revalidated_at_constructor_lease() {
    for cancel in [false, true] {
        let fixture = Fixture::new();
        let request = fixture.request();
        let ready = fixture.ready(&request);
        let admission = ready
            .admission(
                PathBuf::from("/private-fixture/codex"),
                fixture.directory.path().to_owned(),
                None,
            )
            .unwrap();
        if cancel {
            fixture
                .store
                .cancel_preparation(&fixture.homes, &request.binding)
                .unwrap();
        } else {
            fixture.host.lock().unwrap().register_client(USER);
            authenticate(&fixture.host);
        }
        assert!(admission.authorize(admission.recipe()).is_err());
        assert!(!admission.claimed.load(Ordering::Acquire));
        assert!(
            fixture
                .homes
                .path(Provider::Codex, request.binding.key)
                .is_dir()
        );
    }
}

#[test]
fn root_or_parent_replacement_after_admission_refuses_before_the_claim() {
    for replacement in ["root", "parent", "root-symlink"] {
        let fixture = Fixture::new();
        let request = fixture.request();
        let admission = fixture
            .ready(&request)
            .admission(
                PathBuf::from("/private-fixture/codex"),
                fixture.directory.path().to_owned(),
                None,
            )
            .unwrap();
        let before = fs::read(fixture.directory.path().join("registry/accounts.toml")).unwrap();
        let path = fixture.homes.path(Provider::Codex, request.binding.key);
        let retained = fixture.directory.path().join("retained-original");
        let original = if replacement == "parent" {
            let parent = path.parent().unwrap();
            fs::rename(parent, &retained).unwrap();
            fs::create_dir(parent).unwrap();
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
            retained.join(path.file_name().unwrap())
        } else {
            fs::rename(&path, &retained).unwrap();
            retained
        };
        let foreign = if replacement == "root-symlink" {
            let foreign = fixture.directory.path().join("foreign-target");
            fs::create_dir(&foreign).unwrap();
            fs::set_permissions(&foreign, fs::Permissions::from_mode(0o700)).unwrap();
            std::os::unix::fs::symlink(&foreign, &path).unwrap();
            foreign
        } else {
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            path
        };
        fs::write(foreign.join("config.toml"), "foreign after admission\n").unwrap();

        assert!(admission.authorize(admission.recipe()).is_err());
        assert!(!admission.claimed.load(Ordering::Acquire));
        assert_eq!(
            fs::read_to_string(original.join("config.toml")).unwrap(),
            "model='private-code-fixture'\n"
        );
        assert_eq!(
            fs::read_to_string(foreign.join("config.toml")).unwrap(),
            "foreign after admission\n"
        );
        assert_eq!(
            fs::read(fixture.directory.path().join("registry/accounts.toml")).unwrap(),
            before
        );
        assert_eq!(
            fixture.store.observe_fresh_code(&request).unwrap().unwrap(),
            admission.owner().clone()
        );
    }
}

#[test]
fn exact_proof_and_user_role_are_checked_again_after_admission() {
    use crate::host::agent_execution::terminals::AccountPreparationCaller;
    for changed in ["proof", "missing-role", "delegated-role"] {
        let fixture = Fixture::new();
        let request = fixture.request();
        let admission = fixture
            .ready(&request)
            .admission(
                PathBuf::from("/private-fixture/codex"),
                fixture.directory.path().to_owned(),
                None,
            )
            .unwrap();
        let before = fs::read(fixture.directory.path().join("registry/accounts.toml")).unwrap();
        {
            let mut state = fixture.host.lock().unwrap();
            let original_delivery = state.clients.get(&USER).unwrap().clone();
            if changed == "proof" {
                // Keep the same live delivery and explicit User role so only
                // the original proof comparison can refuse this case.
                let mut replacement = HostState::new(HostSessionProof {
                    host_session_id: HostSessionId::new("private-code-host"),
                    boot_id: BootId::new("private-code-boot"),
                    host_incarnation: HostIncarnation::new("replacement-code-incarnation"),
                });
                replacement.clients = state.clients.clone();
                replacement.account_preparation_callers = state.account_preparation_callers.clone();
                replacement.control_gate = state.control_gate.clone();
                *state = replacement;
                assert!(matches!(
                    state.account_preparation_callers.get(&USER),
                    Some(AccountPreparationCaller::User)
                ));
            } else if changed == "missing-role" {
                state.account_preparation_callers.remove(&USER);
            } else {
                state
                    .account_preparation_callers
                    .insert(USER, AccountPreparationCaller::DelegatedAttempt);
            }
            assert!(Arc::ptr_eq(
                state.clients.get(&USER).unwrap(),
                &original_delivery
            ));
            if changed != "proof" {
                assert_eq!(state.proof(), &admission.owner().host);
            }
        }
        assert!(admission.authorize(admission.recipe()).is_err());
        assert!(!admission.claimed.load(Ordering::Acquire));
        assert_eq!(
            fs::read(fixture.directory.path().join("registry/accounts.toml")).unwrap(),
            before
        );
        assert_eq!(
            fs::read_to_string(
                fixture
                    .homes
                    .path(Provider::Codex, request.binding.key)
                    .join("config.toml")
            )
            .unwrap(),
            "model='private-code-fixture'\n"
        );
    }
}

#[test]
fn missing_handler_fixture_refuses_before_ambient_storage_or_source_resolution() {
    let fixture = Fixture::new();
    let request = fixture.request();
    let before = fs::read(fixture.directory.path().join("registry/accounts.toml")).unwrap();
    assert!(prepare_start(&fixture.host, USER, &request).is_err());
    assert_eq!(
        fs::read(fixture.directory.path().join("registry/accounts.toml")).unwrap(),
        before
    );
    assert!(!fixture.directory.path().join("homes").exists());
}
