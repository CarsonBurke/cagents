use super::*;
use crate::account::fresh_code::{CodeAttempt, CodeMethod};
use crate::model::{BootId, HostIncarnation, HostSessionId, TerminalId};
use std::os::unix::fs::PermissionsExt;

// Frozen version-6 bytes, independent of the version-7 writer/owner serializer.
const V6: &str = r#"version = 6
revision = 19
installation = "00000000-0000-0000-0000-000000000001"

[[preparations]]
state = "created"
[preparations.binding]
request_id = "00000000-0000-0000-0000-000000000002"
installation_id = "00000000-0000-0000-0000-000000000001"
key = "00000000-0000-0000-0000-000000000003"
[preparations.binding.payload]
provider = "codex"
copy_codex_config = true

[[existing_pending]]
state = "pending"
cancellation_ids = []
[existing_pending.binding]
installation_id = "00000000-0000-0000-0000-000000000001"
provider = "claude"
key = "00000000-0000-0000-0000-000000000004"
started_at = 0
generation_id = "00000000-0000-0000-0000-000000000005"

[[pending]]
key = "00000000-0000-0000-0000-000000000003"
provider = "codex"
started_at = 0

[[pending]]
key = "00000000-0000-0000-0000-000000000004"
provider = "claude"
started_at = 0
"#;

struct Fixture {
    _directory: tempfile::TempDir,
    store: Store,
    homes: Homes,
    request: FreshCodeStart,
    proof: HostSessionProof,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("registry"));
        fs::create_dir(&store.directory).unwrap();
        fs::set_permissions(&store.directory, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(store.path(), V6).unwrap();
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o600)).unwrap();
        let registry = read(&store.path()).unwrap();
        let request = FreshCodeStart {
            binding: registry
                .preparation(Uuid::from_u128(2))
                .unwrap()
                .binding
                .clone(),
            attempt: CodeAttempt {
                id: Uuid::from_u128(6),
                terminal_id: TerminalId::from_uuid(Uuid::from_u128(7)),
                method: CodeMethod::CodexDeviceCode,
            },
        };
        Self {
            homes: Homes::at(directory.path().join("homes")),
            _directory: directory,
            store,
            request,
            proof: HostSessionProof {
                host_session_id: HostSessionId::new("fresh-code-fixture-host"),
                boot_id: BootId::new("fresh-code-fixture-boot"),
                host_incarnation: HostIncarnation::new("fresh-code-fixture-incarnation"),
            },
        }
    }

    fn bytes(&self) -> Vec<u8> {
        fs::read(self.store.path()).unwrap()
    }
    fn claim(&self) -> CodeOwner {
        let CodeClaim::Fresh(owner) = self
            .store
            .claim_fresh_code(&self.request, &self.proof)
            .unwrap()
        else {
            panic!("only the first successful new ownership barrier is fresh");
        };
        owner
    }
}

#[test]
fn frozen_v6_load_preserves_preparations_retention_timestamps_and_bytes() {
    let fixture = Fixture::new();
    let before = fixture.bytes();
    let registry = fixture.store.load().unwrap();
    assert_eq!(registry.version, 6);
    assert_eq!(registry.revision, 19);
    assert_eq!(registry.pending().len(), 2);
    assert_eq!(
        registry
            .existing_pending(Uuid::from_u128(4))
            .unwrap()
            .binding
            .started_at,
        0
    );
    assert_eq!(
        fixture.store.inspect_installation_id().unwrap(),
        Some(Uuid::from_u128(1))
    );
    assert!(
        fixture
            .store
            .observe_fresh_code(&fixture.request)
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn observation_never_repairs_permissions_links_or_missing_namespace() {
    let fixture = Fixture::new();
    let plan = ConditionalPlan {
        request_id: fixture.request.binding.request_id,
        expected_installation: fixture.request.binding.installation_id,
        payload: fixture.request.binding.payload.clone(),
    };
    let before = fixture.bytes();
    let assert_refusal = || {
        assert!(fixture.store.inspect_fresh_code_plan(&plan).is_err());
        assert!(fixture.store.observe_fresh_code(&fixture.request).is_err());
        assert_eq!(fixture.bytes(), before);
    };

    fs::set_permissions(fixture.store.path(), fs::Permissions::from_mode(0o640)).unwrap();
    assert_refusal();
    assert_eq!(
        fs::metadata(fixture.store.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );

    fs::set_permissions(fixture.store.path(), fs::Permissions::from_mode(0o600)).unwrap();
    let linked = fixture.store.directory.join("linked-registry");
    fs::hard_link(fixture.store.path(), &linked).unwrap();
    assert_refusal();
    fs::remove_file(&linked).unwrap();
    fs::rename(fixture.store.path(), &linked).unwrap();
    std::os::unix::fs::symlink(&linked, fixture.store.path()).unwrap();
    assert_refusal();
    assert!(
        fs::symlink_metadata(fixture.store.path())
            .unwrap()
            .is_symlink()
    );

    let missing = Store::at(fixture._directory.path().join("missing-registry"));
    assert!(missing.inspect_fresh_code_plan(&plan).is_err());
    assert!(missing.observe_fresh_code(&fixture.request).is_err());
    assert!(!missing.directory.exists());
}

#[test]
fn conditional_plan_never_mints_missing_identity_and_refuses_changed_installation() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::at(directory.path().join("missing"));
    let request = ConditionalPlan {
        request_id: Uuid::new_v4(),
        expected_installation: Uuid::from_u128(1),
        payload: PreparationPayload {
            provider: Provider::Codex,
            copy_codex_config: false,
        },
    };
    assert!(store.plan_fresh_code(&request).is_err());
    assert!(!store.directory.exists());
    let fixture = Fixture::new();
    let before = fixture.bytes();
    let mut changed = request.clone();
    changed.expected_installation = Uuid::from_u128(80);
    assert!(fixture.store.plan_fresh_code(&changed).is_err());
    assert_eq!(fixture.bytes(), before);
    let planned = fixture.store.plan_fresh_code(&request).unwrap();
    assert_eq!(
        planned.binding.installation_id,
        request.expected_installation
    );
    assert_eq!(fixture.store.plan_fresh_code(&request).unwrap(), planned);
    let before_read = fixture.bytes();
    assert_eq!(
        fixture.store.inspect_fresh_code_plan(&request).unwrap(),
        Some(planned.clone())
    );
    assert_eq!(fixture.bytes(), before_read);
    let mut reused = request;
    reused.payload.copy_codex_config = true;
    let before = fixture.bytes();
    assert!(fixture.store.plan_fresh_code(&reused).is_err());
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn prepublication_lock_refusal_and_exhausted_revision_never_publish_an_owner() {
    let fixture = Fixture::new();
    let before = fixture.bytes();
    let held = fixture.store.try_lock().unwrap();
    assert_eq!(
        fixture
            .store
            .claim_fresh_code(&fixture.request, &fixture.proof)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(
        fixture
            .store
            .observe_fresh_code(&fixture.request)
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.bytes(), before);
    drop(held);
    let mut exhausted = read(&fixture.store.path()).unwrap();
    exhausted.revision = u64::MAX;
    write(&fixture.store.directory, &fixture.store.path(), &exhausted).unwrap();
    let before = fixture.bytes();
    assert!(
        fixture
            .store
            .claim_fresh_code(&fixture.request, &fixture.proof)
            .is_err()
    );
    assert_eq!(fixture.bytes(), before);
    assert!(
        fixture
            .store
            .observe_fresh_code(&fixture.request)
            .unwrap()
            .is_none()
    );
}

#[test]
fn published_but_unacknowledged_owner_replay_is_retained_and_never_fresh() {
    let fixture = Fixture::new();
    let before = read(&fixture.store.path()).unwrap();
    let result = fixture
        .store
        .claim_fresh_code_with_sync(&fixture.request, &fixture.proof, |_| {
            Err(io::Error::from_raw_os_error(libc::EIO))
        });
    assert_eq!(result.err().unwrap().raw_os_error(), Some(libc::EIO));
    let visible = fixture
        .store
        .observe_fresh_code(&fixture.request)
        .unwrap()
        .unwrap();
    assert_eq!(visible.state, CodeState::Owned);
    let after = read(&fixture.store.path()).unwrap();
    assert_eq!(after.version, 7);
    assert_eq!(after.preparations, before.preparations);
    assert_eq!(after.existing_pending, before.existing_pending);
    assert_eq!(after.pending, before.pending);
    assert_eq!(after.revision, before.revision + 1);
    let bytes = fixture.bytes();
    let mut barriers = 0;
    let replay = fixture
        .store
        .claim_fresh_code_with_sync(&fixture.request, &fixture.proof, |namespace| {
            barriers += 1;
            namespace.sync()
        })
        .unwrap();
    assert!(matches!(replay, CodeClaim::Retained(ref owner) if owner == &visible));
    assert_eq!(barriers, 1);
    assert_eq!(fixture.bytes(), bytes);
}

#[test]
fn stable_owner_rejects_changed_binding_attempt_terminal_and_original_host() {
    let fixture = Fixture::new();
    let owner = fixture.claim();
    let bytes = fixture.bytes();
    for field in 0..4 {
        let mut changed = fixture.request.clone();
        match field {
            0 => changed.binding.installation_id = Uuid::new_v4(),
            1 => changed.binding.payload.copy_codex_config = false,
            2 => changed.attempt.id = Uuid::new_v4(),
            _ => changed.attempt.terminal_id = TerminalId::new(),
        }
        assert!(
            fixture
                .store
                .claim_fresh_code(&changed, &fixture.proof)
                .is_err()
        );
        assert_eq!(fixture.bytes(), bytes);
    }
    let mut foreign = fixture.proof.clone();
    foreign.host_incarnation = HostIncarnation::new("foreign");
    assert!(
        fixture
            .store
            .claim_fresh_code(&fixture.request, &foreign)
            .is_err()
    );
    assert_eq!(fixture.bytes(), bytes);
    assert!(
        matches!(fixture.store.claim_fresh_code(&fixture.request, &fixture.proof).unwrap(), CodeClaim::Retained(saved) if saved == owner)
    );
}

#[cfg(feature = "host")]
#[test]
fn final_single_lifecycle_lease_fences_cancel_through_fake_constructor() {
    let fixture = Fixture::new();
    let owner = fixture.claim();
    let lease = fixture
        .store
        .authorize_fresh_code_spawn(&fixture.homes, &owner, || Ok(()))
        .unwrap();
    // This is the actual lock used by Cancel, not a second test-only gate.
    assert_eq!(
        fixture
            .store
            .try_cancel_preparation(&fixture.homes, &owner.binding)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    let mut constructions = 0;
    constructions += 1;
    assert_eq!(constructions, 1);
    drop(lease);
    fixture
        .store
        .cancel_preparation(&fixture.homes, &owner.binding)
        .unwrap();
    assert!(
        fixture
            .store
            .authorize_fresh_code_spawn(&fixture.homes, &owner, || panic!(
                "cancelled owner reached live root validation"
            ))
            .is_err()
    );
}

#[test]
fn duplicate_durable_owners_and_owner_under_frozen_v6_are_rejected_without_repair() {
    let fixture = Fixture::new();
    fixture.claim();
    let mut registry = read(&fixture.store.path()).unwrap();
    let owner = registry.fresh_code_owners[0].clone();
    registry.fresh_code_owners.push(owner.clone());
    assert!(registry.validate_preparations().is_err());
    registry.fresh_code_owners.pop();
    registry.version = 6;
    assert!(registry.validate_preparations().is_err());
    let bytes = fixture.bytes();
    assert!(fixture.store.inspect_installation_id().unwrap().is_some());
    assert_eq!(fixture.bytes(), bytes);
}

fn fixture_registration(owner: &CodeOwner) -> Registered {
    Registered {
        key: owner.binding.key,
        provider: Provider::Codex,
        linked_home: None,
        label: None,
        email: Some("private-code@example.invalid".into()),
        organization: None,
        added_at: 1,
    }
}

#[test]
fn bound_registration_is_atomic_and_legacy_unbound_watcher_cannot_settle_owner() {
    let fixture = Fixture::new();
    let owned = fixture.claim();
    // Synthetic positive start/probe metadata tests ONLY the Store contract.
    // This fixture is not evidence of an actual terminal or provider login.
    let owner = fixture.store.code_started(&owned).unwrap();
    let managed = fixture_registration(&owner);
    let before = fixture.bytes();
    let (_, result) = fixture
        .store
        .update(|registry| registry.register(managed.clone(), &[]))
        .unwrap();
    assert_eq!(result, Err(RegistrationError::RetainedPreparation));
    assert_eq!(fixture.bytes(), before);
    let settled = fixture
        .store
        .register_fresh_code(&fixture.homes, &owner, managed, &[], |_| Ok(()))
        .unwrap();
    assert!(
        matches!(settled, CodeRegistration::Registered(ref saved) if saved.state == CodeState::Registered)
    );
    let registry = fixture.store.load().unwrap();
    assert_eq!(
        registry
            .fresh_code_owner(owner.binding.request_id)
            .unwrap()
            .state,
        CodeState::Registered
    );
    assert_eq!(
        registry
            .preparation(owner.binding.request_id)
            .unwrap()
            .state,
        PreparationState::Registered
    );
    assert!(registry.find(owner.binding.key).is_some());
    assert!(
        !registry
            .pending()
            .iter()
            .any(|pending| pending.key == owner.binding.key)
    );
}

#[test]
fn late_probe_cannot_register_after_current_cancel_and_duplicate_retains_home() {
    for cancel in [true, false] {
        let fixture = Fixture::new();
        let owner = fixture.store.code_started(&fixture.claim()).unwrap();
        let managed = fixture_registration(&owner);
        if cancel {
            fixture
                .store
                .cancel_preparation(&fixture.homes, &owner.binding)
                .unwrap();
            let before = fixture.bytes();
            assert!(
                fixture
                    .store
                    .register_fresh_code(&fixture.homes, &owner, managed, &[], |_| panic!(
                        "cancelled probe reached live registration validation"
                    ))
                    .is_err()
            );
            assert_eq!(fixture.bytes(), before);
        } else {
            let system = [(Provider::Codex, managed.identity())];
            let settled = fixture
                .store
                .register_fresh_code(&fixture.homes, &owner, managed, &system, |_| Ok(()))
                .unwrap();
            assert!(
                matches!(settled, CodeRegistration::DuplicateRetained { ref owner, .. } if owner.state == CodeState::DuplicateRetained)
            );
            let registry = fixture.store.load().unwrap();
            assert!(
                registry
                    .pending()
                    .iter()
                    .any(|pending| pending.key == owner.binding.key)
            );
            assert_eq!(
                registry
                    .preparation(owner.binding.request_id)
                    .unwrap()
                    .state,
                PreparationState::Created
            );
            assert!(registry.find(owner.binding.key).is_none());
        }
    }
}
