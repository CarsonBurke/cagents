use super::*;
use crate::agent::Provider;
use std::os::unix::fs::symlink;

struct Fixture {
    _directory: tempfile::TempDir,
    store: Store,
    intent: Intent,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("private/account-preparations/outbox"));
        let intent = Intent {
            request_id: Uuid::new_v4(),
            payload: PreparationPayload {
                provider: Provider::Codex,
                copy_codex_config: true,
            },
            original_route: RouteSnapshot::capture(Route::Remote {
                name: "Original device".into(),
                host: "original-device".into(),
                executable: "/usr/bin/cagents".into(),
            })
            .unwrap(),
            device_code: true,
            blocked_login: Some(Uuid::new_v4()),
        };
        Self {
            _directory: directory,
            store,
            intent,
        }
    }

    fn binding(&self) -> PreparationBinding {
        PreparationBinding {
            request_id: self.intent.request_id,
            installation_id: Uuid::new_v4(),
            key: Uuid::new_v4(),
            payload: self.intent.payload.clone(),
        }
    }

    fn bytes(&self) -> Vec<u8> {
        fs::read(self.store.directory.join(JOURNAL)).unwrap()
    }
}

#[test]
fn cancel_before_prepare_survives_restart_and_late_binding_without_downgrade() {
    let fixture = Fixture::new();
    let cancelled = fixture
        .store
        .upsert(&fixture.intent, Desired::Cancel)
        .unwrap();
    assert_eq!(cancelled.desired, Desired::Cancel);
    assert_eq!(cancelled.revision, 1);
    assert!(cancelled.binding.is_none());
    let bytes = fixture.bytes();
    let restarted = Store::at(fixture.store.directory.clone());
    assert_eq!(restarted.load().unwrap(), Some(vec![cancelled.clone()]));
    assert_eq!(
        restarted.upsert(&fixture.intent, Desired::Prepare).unwrap(),
        cancelled,
        "a delayed initial worker cannot undo the saved cancellation"
    );
    assert_eq!(fixture.bytes(), bytes);
    let binding = fixture.binding();
    let bound = restarted
        .bind(&fixture.intent, cancelled.revision, &binding)
        .unwrap();
    assert_eq!(bound.desired, Desired::Cancel);
    assert_eq!(bound.revision, 2);
    assert_eq!(bound.binding, Some(binding));
    assert_eq!(restarted.load().unwrap(), Some(vec![bound.clone()]));
    assert_eq!(
        fixture
            .store
            .upsert(&fixture.intent, Desired::Prepare)
            .unwrap(),
        bound
    );
    let metadata = fs::metadata(fixture.store.directory.join(JOURNAL)).unwrap();
    assert_eq!(metadata.mode() & 0o7777, 0o600);
    assert_eq!(metadata.nlink(), 1);
    assert_eq!(
        fs::metadata(&fixture.store.directory).unwrap().mode() & 0o7777,
        0o700
    );
}

#[test]
fn binding_cas_requires_current_revision_and_preserves_concurrent_cancel() {
    let mut fixture = Fixture::new();
    fixture.intent.original_route = RouteSnapshot::capture(Route::Local {
        application: "cagents-test".into(),
        state_directory: fixture._directory.path().join("state"),
    })
    .unwrap();
    let prepare = fixture
        .store
        .upsert(&fixture.intent, Desired::Prepare)
        .unwrap();
    let cancelled = fixture
        .store
        .upsert(&fixture.intent, Desired::Cancel)
        .unwrap();
    assert_eq!(cancelled.revision, prepare.revision + 1);
    let binding = fixture.binding();
    let bytes = fixture.bytes();
    assert_eq!(
        fixture
            .store
            .bind(&fixture.intent, prepare.revision, &binding)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(fixture.bytes(), bytes);
    let bound = fixture
        .store
        .bind(&fixture.intent, cancelled.revision, &binding)
        .unwrap();
    assert_eq!(bound.desired, Desired::Cancel);
    assert_eq!(bound.revision, cancelled.revision + 1);
    let bytes = fixture.bytes();
    for revision in [cancelled.revision, bound.revision + 1] {
        assert_eq!(
            fixture
                .store
                .bind(&fixture.intent, revision, &binding)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock,
            "an identical binding cannot bypass the local revision CAS"
        );
        assert_eq!(fixture.bytes(), bytes);
    }
    assert_eq!(
        fixture
            .store
            .bind(&fixture.intent, bound.revision, &binding)
            .unwrap(),
        bound
    );
    assert_eq!(fixture.bytes(), bytes);
}

#[test]
fn immutable_payload_route_and_exact_binding_conflicts_preserve_journal_bytes() {
    let fixture = Fixture::new();
    let prepare = fixture
        .store
        .upsert(&fixture.intent, Desired::Prepare)
        .unwrap();
    let binding = fixture.binding();
    let bound = fixture
        .store
        .bind(&fixture.intent, prepare.revision, &binding)
        .unwrap();
    let bytes = fixture.bytes();
    let mut changed_route = fixture.intent.clone();
    changed_route.original_route = RouteSnapshot::capture(Route::Remote {
        name: "Original device".into(),
        host: "replacement-device".into(),
        executable: "/usr/bin/cagents".into(),
    })
    .unwrap();
    let mut changed_config = fixture.intent.clone();
    changed_config.payload.copy_codex_config = false;
    let mut changed_provider = fixture.intent.clone();
    changed_provider.payload.provider = Provider::Claude;
    let mut changed_mode = fixture.intent.clone();
    changed_mode.device_code = false;
    let mut changed_login = fixture.intent.clone();
    changed_login.blocked_login = Some(Uuid::new_v4());
    for intent in [
        changed_route,
        changed_config,
        changed_provider,
        changed_mode,
        changed_login,
    ] {
        assert!(fixture.store.upsert(&intent, Desired::Cancel).is_err());
        assert!(
            fixture
                .store
                .bind(&intent, bound.revision, &binding)
                .is_err()
        );
        assert_eq!(fixture.bytes(), bytes);
    }
    let mut wrong_request = binding.clone();
    wrong_request.request_id = Uuid::new_v4();
    let mut wrong_installation = binding.clone();
    wrong_installation.installation_id = Uuid::new_v4();
    let mut wrong_key = binding.clone();
    wrong_key.key = Uuid::new_v4();
    let mut wrong_payload = binding.clone();
    wrong_payload.payload.copy_codex_config = false;
    for binding in [wrong_request, wrong_installation, wrong_key, wrong_payload] {
        assert!(
            fixture
                .store
                .bind(&fixture.intent, bound.revision, &binding)
                .is_err()
        );
        assert_eq!(fixture.bytes(), bytes);
    }
    // A second request cannot bind another outbox entry to this exact home.
    let mut second = fixture.intent.clone();
    second.request_id = Uuid::new_v4();
    let second_record = fixture.store.upsert(&second, Desired::Prepare).unwrap();
    let bytes = fixture.bytes();
    let mut duplicate_home = binding;
    duplicate_home.request_id = second.request_id;
    assert!(
        fixture
            .store
            .bind(&second, second_record.revision, &duplicate_home)
            .is_err()
    );
    assert_eq!(fixture.bytes(), bytes);
    assert!(fixture.store.load().unwrap().unwrap().contains(&bound));
}

#[test]
fn missing_corrupt_unsupported_and_oversized_journals_are_never_empty_proof() {
    let fixture = Fixture::new();
    let binding = fixture.binding();
    assert!(fixture.store.load().unwrap().is_none());
    assert!(!fixture.store.directory.exists());
    assert_eq!(
        fixture
            .store
            .bind(&fixture.intent, 1, &binding)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    assert!(!fixture.store.directory.exists());
    let original_record = fixture
        .store
        .upsert(&fixture.intent, Desired::Prepare)
        .unwrap();
    let original = fixture.bytes();
    let mut newer: serde_json::Value = serde_json::from_slice(&original).unwrap();
    newer["version"] = 2.into();
    let mut extra: serde_json::Value = serde_json::from_slice(&original).unwrap();
    extra["records"][0]["intent"]["payload"]["unsupported_policy"] = true.into();
    let mut duplicate: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let first = duplicate["records"][0].clone();
    duplicate["records"].as_array_mut().unwrap().push(first);
    let mut wrong_route: serde_json::Value = serde_json::from_slice(&original).unwrap();
    wrong_route["records"][0]["intent"]["original_route"]["route_revision"] =
        "route-v1:unmatched".into();
    for bytes in [
        b"{broken".to_vec(),
        serde_json::to_vec(&newer).unwrap(),
        serde_json::to_vec(&extra).unwrap(),
        serde_json::to_vec(&duplicate).unwrap(),
        serde_json::to_vec(&wrong_route).unwrap(),
    ] {
        fs::write(fixture.store.directory.join(JOURNAL), &bytes).unwrap();
        assert!(fixture.store.load().is_err());
        assert!(
            fixture
                .store
                .upsert(&fixture.intent, Desired::Cancel)
                .is_err()
        );
        assert_eq!(fixture.bytes(), bytes);
    }

    // Parsing and schema validation would succeed; the bounded reader must
    // refuse the actual file size rather than a malformed JSON document.
    let mut oversized = original.clone();
    oversized.resize(MAX_BYTES as usize + 1, b' ');
    serde_json::from_slice::<Journal>(&oversized)
        .unwrap()
        .validate()
        .unwrap();
    fs::write(fixture.store.directory.join(JOURNAL), &oversized).unwrap();
    assert_eq!(
        fs::metadata(fixture.store.directory.join(JOURNAL))
            .unwrap()
            .len(),
        MAX_BYTES + 1
    );
    for error in [
        fixture.store.load().unwrap_err(),
        fixture
            .store
            .upsert(&fixture.intent, Desired::Cancel)
            .unwrap_err(),
    ] {
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "The account setup outbox is too large");
        assert_eq!(fixture.bytes(), oversized);
    }

    // Fill the journal with complete, individually valid records. Distinct
    // UUIDs have equal serialized lengths, so this is the largest whole-record
    // fixture that fits, without repeated large serializations or padding.
    let empty_size = serde_json::to_vec(&Journal {
        version: VERSION,
        records: Vec::new(),
    })
    .unwrap()
    .len();
    let record_size = serde_json::to_vec(&original_record).unwrap().len();
    let count = (MAX_BYTES as usize - empty_size + 1) / (record_size + 1);
    assert!(count > 1);
    let records: Vec<_> = (1..=count)
        .map(|index| {
            let mut record = original_record.clone();
            record.intent.request_id = Uuid::from_u128(index as u128);
            record.validate().unwrap();
            record
        })
        .collect();
    let mut near_limit = Journal {
        version: VERSION,
        records: records.clone(),
    };
    near_limit.validate().unwrap();
    let bytes = serde_json::to_vec(&near_limit).unwrap();
    assert!(bytes.len() as u64 <= MAX_BYTES);
    let mut additional = original_record;
    additional.intent.request_id = Uuid::from_u128(count as u128 + 1);
    additional.validate().unwrap();
    near_limit.records.push(additional.clone());
    near_limit.validate().unwrap();
    assert!(serde_json::to_vec(&near_limit).unwrap().len() as u64 > MAX_BYTES);
    fs::write(fixture.store.directory.join(JOURNAL), &bytes).unwrap();
    assert_eq!(fixture.store.load().unwrap(), Some(records.clone()));
    let error = fixture
        .store
        .upsert(&additional.intent, Desired::Prepare)
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "The account setup outbox is too large");
    assert_eq!(fixture.bytes(), bytes);
    assert_eq!(fixture.store.load().unwrap(), Some(records));

    fs::write(fixture.store.directory.join(JOURNAL), original).unwrap();
    let mut unknown = fixture.intent.clone();
    unknown.request_id = Uuid::new_v4();
    let mut unknown_binding = binding;
    unknown_binding.request_id = unknown.request_id;
    let bytes = fixture.bytes();
    assert_eq!(
        fixture
            .store
            .bind(&unknown, 1, &unknown_binding)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(fixture.bytes(), bytes);
    fs::remove_file(fixture.store.directory.join(JOURNAL)).unwrap();
    assert!(fixture.store.load().unwrap().is_none());
    assert_eq!(
        fixture
            .store
            .bind(&fixture.intent, 1, &fixture.binding())
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn unsafe_files_locks_and_directories_are_refused_without_repair_or_target_writes() {
    for kind in [
        "symlink",
        "hardlink",
        "mode",
        "directory",
        "lock",
        "parent",
        "directory_mode",
    ] {
        let fixture = Fixture::new();
        fixture
            .store
            .upsert(&fixture.intent, Desired::Prepare)
            .unwrap();
        let path = fixture.store.directory.join(JOURNAL);
        let bytes = fixture.bytes();
        let target = fixture._directory.path().join("preserve");
        fs::write(&target, b"untouched outside target").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let original_directory = fixture._directory.path().join("original-outbox");
        match kind {
            "symlink" => {
                fs::remove_file(&path).unwrap();
                symlink(&target, &path).unwrap();
            }
            "hardlink" => fs::hard_link(&path, fixture._directory.path().join("alias")).unwrap(),
            "mode" => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
            "directory" => {
                fs::remove_file(&path).unwrap();
                fs::create_dir(&path).unwrap();
            }
            "lock" => {
                fs::remove_file(fixture.store.directory.join(LOCK)).unwrap();
                symlink(&target, fixture.store.directory.join(LOCK)).unwrap();
            }
            "parent" => {
                fs::rename(&fixture.store.directory, &original_directory).unwrap();
                symlink(&original_directory, &fixture.store.directory).unwrap();
            }
            "directory_mode" => {
                fs::set_permissions(&fixture.store.directory, fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(fixture.store.load().is_err(), "{kind}");
        assert!(
            fixture
                .store
                .upsert(&fixture.intent, Desired::Cancel)
                .is_err(),
            "{kind}"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"untouched outside target",
            "{kind}"
        );
        match kind {
            "symlink" | "lock" => {
                let link = if kind == "lock" {
                    fixture.store.directory.join(LOCK)
                } else {
                    path
                };
                assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
            }
            "mode" => {
                assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
                assert_eq!(fixture.bytes(), bytes);
            }
            "directory" => assert!(path.is_dir()),
            "parent" => assert_eq!(fs::read(original_directory.join(JOURNAL)).unwrap(), bytes),
            "directory_mode" => {
                assert_eq!(
                    fs::metadata(&fixture.store.directory).unwrap().mode() & 0o777,
                    0o755
                );
                assert_eq!(fixture.bytes(), bytes);
            }
            _ => assert_eq!(fixture.bytes(), bytes),
        }
    }
}

#[test]
fn visible_rename_and_equal_replay_require_directory_durability_and_release_lock() {
    let fixture = Fixture::new();
    let result = fixture
        .store
        .change(&fixture.intent, Change::Intent(Desired::Cancel), |_| {
            Err(io::Error::from_raw_os_error(libc::EIO))
        });
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EIO));
    let visible = fixture.store.load().unwrap().unwrap().remove(0);
    assert_eq!(visible.desired, Desired::Cancel);
    let bytes = fixture.bytes();
    let inode = fs::metadata(fixture.store.directory.join(JOURNAL))
        .unwrap()
        .ino();
    // The error released its process lock; another owner can hold it now.
    let held = fixture.store.lock(false).unwrap().unwrap();
    assert_eq!(
        fixture
            .store
            .upsert(&fixture.intent, Desired::Prepare)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    drop(held);
    let mut calls = 0;
    let result = fixture
        .store
        .change(&fixture.intent, Change::Intent(Desired::Prepare), |_| {
            calls += 1;
            Err(io::Error::from_raw_os_error(libc::EIO))
        });
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EIO));
    assert_eq!(
        calls, 1,
        "equal visibility cannot skip the durability barrier"
    );
    assert_eq!(fixture.bytes(), bytes);
    let durable = fixture
        .store
        .change(
            &fixture.intent,
            Change::Intent(Desired::Prepare),
            |namespace| {
                calls += 1;
                namespace.sync()
            },
        )
        .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(durable, visible);
    assert_eq!(fixture.bytes(), bytes);
    assert_eq!(
        fs::metadata(fixture.store.directory.join(JOURNAL))
            .unwrap()
            .ino(),
        inode
    );
    assert!(fixture.store.lock(false).unwrap().is_some());
}
