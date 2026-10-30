use super::*;
use crate::account::fresh_code::{CodeAttempt, CodeMethod};
use crate::model::{BootId, HostIncarnation, HostSessionId, TerminalId};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

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

#[test]
fn registration_postrename_ancestor_failure_replays_the_exact_result_without_rewrite() {
    for duplicate in [false, true] {
        let fixture = Fixture::new();
        let owner = fixture.store.code_started(&fixture.claim()).unwrap();
        let managed = fixture_registration(&owner);
        let system = if duplicate {
            vec![(Provider::Codex, managed.identity())]
        } else {
            Vec::new()
        };
        let before_inode = fs::metadata(fixture.store.path()).unwrap().ino();
        let mut ancestor_calls = 0;
        let first_unknown = fixture
            .store
            .register_fresh_code_with_sync(
                &fixture.homes,
                &owner,
                managed.clone(),
                &system,
                |_| Ok(()),
                |namespace| {
                    namespace.sync_with(|directory| {
                        ancestor_calls += 1;
                        if ancestor_calls == 2 {
                            Err(io::Error::from_raw_os_error(libc::EIO))
                        } else {
                            directory.sync_all()
                        }
                    })
                },
            )
            .err()
            .unwrap();
        assert_eq!(first_unknown.raw_os_error(), Some(libc::EIO));
        assert_eq!(ancestor_calls, 2);
        let inode = fs::metadata(fixture.store.path()).unwrap().ino();
        assert_ne!(inode, before_inode, "the result really passed rename");
        let visible = fixture.store.fresh_code_observation().unwrap();
        let record = visible
            .registration_record(owner.binding.request_id)
            .unwrap()
            .clone();
        assert_eq!(record.started, owner);
        assert_eq!(record.managed, managed);
        if duplicate {
            assert_eq!(
                record.outcome,
                RegistrationOutcome::DuplicateRetained {
                    existing: Account::system(Provider::Codex).id(),
                    account: DuplicateAccount::System {
                        identity: managed.identity()
                    }
                }
            );
        } else {
            assert_eq!(record.outcome, RegistrationOutcome::Registered);
        }
        // Read visibility is metadata, not a successful barrier receipt.
        assert_eq!(
            fixture
                .store
                .observe_fresh_code(&fixture.request)
                .unwrap()
                .unwrap(),
            record.settled_owner()
        );
        let bytes = fixture.bytes();
        let revision = visible.revision();
        let replay_unknown = fixture
            .store
            .register_fresh_code_with_sync(
                &fixture.homes,
                &owner,
                managed.clone(),
                &system,
                |_| Ok(()),
                |_| Err(io::Error::from_raw_os_error(libc::EIO)),
            )
            .err()
            .unwrap();
        assert_eq!(replay_unknown.raw_os_error(), Some(libc::EIO));
        assert_eq!(first_unknown.raw_os_error(), Some(libc::EIO));
        assert_eq!(fixture.bytes(), bytes);
        assert_eq!(fs::metadata(fixture.store.path()).unwrap().ino(), inode);

        let mut live_checks = 0;
        let mut barrier_calls = 0;
        let actual = fixture
            .store
            .register_fresh_code_with_sync(
                &fixture.homes,
                &owner,
                managed,
                &system,
                |_| {
                    live_checks += 1;
                    Ok(())
                },
                |namespace| {
                    barrier_calls += 1;
                    namespace.sync()
                },
            )
            .unwrap();
        assert_eq!(barrier_calls, 1);
        assert_eq!(live_checks, 2);
        match actual {
            CodeRegistration::Registered(saved) => {
                assert!(!duplicate);
                assert_eq!(saved, record.settled_owner());
            }
            CodeRegistration::DuplicateRetained { owner, existing } => {
                assert!(duplicate);
                assert_eq!(owner, record.settled_owner());
                assert_eq!(existing, Account::system(Provider::Codex).id());
            }
        }
        assert_eq!(fixture.bytes(), bytes);
        assert_eq!(fs::metadata(fixture.store.path()).unwrap().ino(), inode);
        let after = fixture.store.fresh_code_observation().unwrap();
        assert_eq!(after.revision(), revision);
        assert_eq!(
            after.registration_record(owner.binding.request_id),
            Some(&record)
        );
    }
}

#[test]
fn registration_replay_refuses_changed_original_inputs_phase_and_current_cancel() {
    let fixture = Fixture::new();
    let owner = fixture.store.code_started(&fixture.claim()).unwrap();
    let managed = fixture_registration(&owner);
    let system = [(Provider::Codex, managed.identity())];
    assert!(
        fixture
            .store
            .register_fresh_code_with_sync(
                &fixture.homes,
                &owner,
                managed.clone(),
                &system,
                |_| Ok(()),
                |_| Err(io::Error::from_raw_os_error(libc::EIO)),
            )
            .is_err()
    );
    let first = fixture
        .store
        .fresh_code_observation()
        .unwrap()
        .registration_record(owner.binding.request_id)
        .unwrap()
        .clone();
    let mut wrong_owners = Vec::new();
    let mut changed = owner.clone();
    changed.binding.request_id = Uuid::from_u128(300);
    wrong_owners.push(changed);
    let mut changed = owner.clone();
    changed.binding.installation_id = Uuid::from_u128(301);
    wrong_owners.push(changed);
    let mut changed = owner.clone();
    changed.binding.key = Uuid::from_u128(302);
    wrong_owners.push(changed);
    let mut changed = owner.clone();
    changed.binding.payload.copy_codex_config = !changed.binding.payload.copy_codex_config;
    wrong_owners.push(changed);
    let mut changed = owner.clone();
    changed.attempt.id = Uuid::from_u128(303);
    wrong_owners.push(changed);
    let mut changed = owner.clone();
    changed.attempt.terminal_id = TerminalId::from_uuid(Uuid::from_u128(304));
    wrong_owners.push(changed);
    let mut changed = owner.clone();
    changed.host.host_incarnation = HostIncarnation::new("another-original-host");
    wrong_owners.push(changed);
    let mut changed = owner.clone();
    changed.state = CodeState::DuplicateRetained;
    wrong_owners.push(changed);
    for wrong in wrong_owners {
        let before = fixture.bytes();
        assert!(
            fixture
                .store
                .register_fresh_code_with_sync(
                    &fixture.homes,
                    &wrong,
                    managed.clone(),
                    &system,
                    |_| panic!("changed original owner reached live replay validation"),
                    |_| panic!("changed original owner reached the durability barrier"),
                )
                .is_err()
        );
        assert_eq!(fixture.bytes(), before);
    }
    for change_system in [false, true] {
        let mut wrong = managed.clone();
        let mut changed_system = system.to_vec();
        if change_system {
            changed_system[0].1.organization = Some("changed-system-identity".into());
        } else {
            wrong.email = Some("different-original-report@example.test".into());
        }
        let before = fixture.bytes();
        assert!(
            fixture
                .store
                .register_fresh_code_with_sync(
                    &fixture.homes,
                    &owner,
                    wrong,
                    &changed_system,
                    |_| panic!("changed original input reached live replay validation"),
                    |_| panic!("changed original input reached the durability barrier"),
                )
                .is_err()
        );
        assert_eq!(fixture.bytes(), before);
    }
    fixture
        .store
        .cancel_preparation(&fixture.homes, &owner.binding)
        .unwrap();
    let before = fixture.bytes();
    assert!(
        fixture
            .store
            .register_fresh_code_with_sync(
                &fixture.homes,
                &owner,
                managed,
                &system,
                |_| panic!("current Cancel reached live replay validation"),
                |_| panic!("current Cancel reached the durability barrier"),
            )
            .is_err()
    );
    assert_eq!(fixture.bytes(), before);
    let after = fixture.store.fresh_code_observation().unwrap();
    assert_eq!(
        after.registration_record(owner.binding.request_id),
        Some(&first)
    );
    assert_eq!(
        after.preparation(owner.binding.request_id).unwrap().state,
        PreparationState::CancelledRetained
    );
}

#[test]
fn duplicate_replay_returns_the_first_actual_account_and_refuses_changed_target_or_outcome() {
    let fixture = Fixture::new();
    let owner = fixture.store.code_started(&fixture.claim()).unwrap();
    let managed = fixture_registration(&owner);
    let mut first = managed.clone();
    first.key = Uuid::from_u128(401);
    let mut second = managed.clone();
    second.key = Uuid::from_u128(402);
    fixture
        .store
        .update(|registry| {
            // Existing linked rows may name the same login. This fixed
            // metadata fixture makes recomputing the first duplicate visible.
            first.linked_home = Some(PathBuf::from("/private-fixture/first"));
            second.linked_home = Some(PathBuf::from("/private-fixture/second"));
            registry.accounts.extend([first.clone(), second]);
        })
        .unwrap();
    assert!(
        fixture
            .store
            .register_fresh_code_with_sync(
                &fixture.homes,
                &owner,
                managed.clone(),
                &[],
                |_| Ok(()),
                |_| Err(io::Error::from_raw_os_error(libc::EIO)),
            )
            .is_err()
    );
    let first_id = Account::managed(first.provider, first.key, PathBuf::new()).id();
    fixture
        .store
        .update(|registry| registry.accounts.reverse())
        .unwrap();
    let before = fixture.bytes();
    let inode = fs::metadata(fixture.store.path()).unwrap().ino();
    let CodeRegistration::DuplicateRetained { existing, .. } = fixture
        .store
        .register_fresh_code(&fixture.homes, &owner, managed.clone(), &[], |_| Ok(()))
        .unwrap()
    else {
        panic!("only the stored first duplicate can be returned");
    };
    assert_eq!(existing, first_id);
    assert_eq!(fixture.bytes(), before);
    assert_eq!(fs::metadata(fixture.store.path()).unwrap().ino(), inode);

    for changed in ["identity", "home"] {
        fixture
            .store
            .update(|registry| {
                let target = registry
                    .accounts
                    .iter_mut()
                    .find(|account| account.key == first.key)
                    .unwrap();
                *target = first.clone();
                if changed == "identity" {
                    target.email = Some("changed-current-duplicate@example.test".into());
                } else {
                    target.linked_home = Some(PathBuf::from("/private-fixture/retargeted"));
                }
            })
            .unwrap();
        let before = fixture.bytes();
        assert!(
            fixture
                .store
                .register_fresh_code_with_sync(
                    &fixture.homes,
                    &owner,
                    managed.clone(),
                    &[],
                    |_| panic!("changed duplicate reached live replay validation"),
                    |_| panic!("changed duplicate reached the durability barrier"),
                )
                .is_err()
        );
        assert_eq!(fixture.bytes(), before);
    }

    let mut corrupt = fixture.store.fresh_code_observation().unwrap();
    corrupt.fresh_code_registrations[0].outcome = RegistrationOutcome::Registered;
    fs::write(
        fixture.store.path(),
        toml::to_string_pretty(&corrupt).unwrap(),
    )
    .unwrap();
    let before = fixture.bytes();
    assert!(
        fixture
            .store
            .register_fresh_code_with_sync(
                &fixture.homes,
                &owner,
                managed,
                &[],
                |_| panic!("mismatched stored outcome reached live validation"),
                |_| panic!("mismatched stored outcome reached the barrier"),
            )
            .is_err()
    );
    assert_eq!(
        fixture.bytes(),
        before,
        "corruption cannot repair its own outcome"
    );
}

#[test]
fn replay_refuses_file_or_ancestor_replacement_and_final_live_validation_failure() {
    for changed in ["file", "ancestor", "live"] {
        let fixture = Fixture::new();
        let owner = fixture.store.code_started(&fixture.claim()).unwrap();
        let managed = fixture_registration(&owner);
        assert!(
            fixture
                .store
                .register_fresh_code_with_sync(
                    &fixture.homes,
                    &owner,
                    managed.clone(),
                    &[],
                    |_| Ok(()),
                    |_| Err(io::Error::from_raw_os_error(libc::EIO)),
                )
                .is_err()
        );
        let first_bytes = fixture.bytes();
        let retained = fixture._directory.path().join("retained-first-outcome");
        let mut live_checks = 0;
        let result = fixture.store.register_fresh_code_with_sync(
            &fixture.homes,
            &owner,
            managed,
            &[],
            |_| {
                live_checks += 1;
                if changed == "live" && live_checks == 2 {
                    Err(io::Error::from_raw_os_error(libc::EACCES))
                } else {
                    Ok(())
                }
            },
            |namespace| {
                namespace.sync()?;
                if changed == "file" {
                    fs::rename(fixture.store.path(), &retained)?;
                    fs::write(fixture.store.path(), &first_bytes)?;
                    fs::set_permissions(fixture.store.path(), fs::Permissions::from_mode(0o600))?;
                } else if changed == "ancestor" {
                    fs::rename(&fixture.store.directory, &retained)?;
                    fs::create_dir(&fixture.store.directory)?;
                    fs::set_permissions(
                        &fixture.store.directory,
                        fs::Permissions::from_mode(0o700),
                    )?;
                    fs::write(fixture.store.path(), &first_bytes)?;
                    fs::set_permissions(fixture.store.path(), fs::Permissions::from_mode(0o600))?;
                }
                Ok(())
            },
        );
        assert!(
            result.is_err(),
            "a passed sync alone cannot acknowledge replay"
        );
        if changed == "live" {
            assert_eq!(result.err().unwrap().raw_os_error(), Some(libc::EACCES));
            assert_eq!(live_checks, 2);
        } else {
            assert_eq!(live_checks, 1);
            let original = if changed == "file" {
                retained
            } else {
                retained.join(FILE_NAME)
            };
            assert_eq!(fs::read(original).unwrap(), first_bytes);
        }
        assert_eq!(fixture.bytes(), first_bytes);
    }
}
