use super::*;
use crate::account::preparation::{ExistingPendingBinding, ExistingPendingTarget};
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
            target: Target::Prepared {
                payload: PreparationPayload {
                    provider: Provider::Codex,
                    copy_codex_config: true,
                },
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

    fn binding(&self) -> Binding {
        let Target::Prepared { payload } = &self.intent.target else {
            panic!("a prepared fixture is required")
        };
        Binding::Prepared {
            binding: PreparationBinding {
                request_id: self.intent.request_id,
                installation_id: Uuid::new_v4(),
                key: Uuid::new_v4(),
                payload: payload.clone(),
            },
        }
    }

    fn bytes(&self) -> Vec<u8> {
        fs::read(self.store.directory.join(JOURNAL)).unwrap()
    }
}

fn prepared_payload_mut(intent: &mut Intent) -> &mut PreparationPayload {
    let Target::Prepared { payload } = &mut intent.target else {
        panic!("a prepared fixture is required")
    };
    payload
}

fn prepared_binding_mut(value: &mut Binding) -> &mut PreparationBinding {
    let Binding::Prepared { binding } = value else {
        panic!("a prepared fixture is required")
    };
    binding
}

// Frozen V1 wire bytes, independent of the current serializer and migration types.
// The valid record order is deliberately not UUID order.
const V1_DOCUMENT: &str = r#"{
  "version": 1,
  "records": [
    {
      "intent": {
        "request_id": "00000000-0000-0000-0000-000000000002",
        "payload": {"provider": "claude", "copy_codex_config": false},
        "original_route": {
          "route": {"kind": "local", "application": "cagents-test", "state_directory": "/tmp/cagents-v1-state"},
          "route_revision": "route-v1:d80bd020f25724a7f779945bb78d546354f3797817845d5f1e03a11fcb9877a0"
        },
        "device_code": false,
        "blocked_login": null
      },
      "desired": "prepare",
      "binding": null,
      "revision": 7
    },
    {
      "intent": {
        "request_id": "00000000-0000-0000-0000-000000000001",
        "payload": {"provider": "codex", "copy_codex_config": true},
        "original_route": {
          "route": {"kind": "remote", "name": "Original device", "host": "original-device", "executable": "/usr/bin/cagents"},
          "route_revision": "route-v1:b58c991cd0d1be3efa21c946bccc2afeb1c7e5354d3d98c55069740004d2b9a4"
        },
        "device_code": true,
        "blocked_login": "00000000-0000-0000-0000-000000000005"
      },
      "desired": "cancel",
      "binding": {
        "request_id": "00000000-0000-0000-0000-000000000001",
        "installation_id": "00000000-0000-0000-0000-000000000003",
        "key": "00000000-0000-0000-0000-000000000004",
        "payload": {"provider": "codex", "copy_codex_config": true}
      },
      "revision": 18446744073709551615
    }
  ]
}"#;

fn expected_v1_records() -> Vec<Record> {
    let local_payload = PreparationPayload {
        provider: Provider::Claude,
        copy_codex_config: false,
    };
    let remote_payload = PreparationPayload {
        provider: Provider::Codex,
        copy_codex_config: true,
    };
    vec![
        Record {
            intent: Intent {
                request_id: Uuid::from_u128(2),
                target: Target::Prepared {
                    payload: local_payload,
                },
                original_route: RouteSnapshot {
                    route: Route::Local {
                        application: "cagents-test".into(),
                        state_directory: "/tmp/cagents-v1-state".into(),
                    },
                    route_revision:
                        "route-v1:d80bd020f25724a7f779945bb78d546354f3797817845d5f1e03a11fcb9877a0"
                            .into(),
                },
                device_code: false,
                blocked_login: None,
            },
            desired: Desired::Prepare,
            binding: None,
            revision: 7,
            caused_by: None,
            phase: Phase::Retained,
        },
        Record {
            intent: Intent {
                request_id: Uuid::from_u128(1),
                target: Target::Prepared {
                    payload: remote_payload.clone(),
                },
                original_route: RouteSnapshot {
                    route: Route::Remote {
                        name: "Original device".into(),
                        host: "original-device".into(),
                        executable: "/usr/bin/cagents".into(),
                    },
                    route_revision:
                        "route-v1:b58c991cd0d1be3efa21c946bccc2afeb1c7e5354d3d98c55069740004d2b9a4"
                            .into(),
                },
                device_code: true,
                blocked_login: Some(Uuid::from_u128(5)),
            },
            desired: Desired::Cancel,
            binding: Some(Binding::Prepared {
                binding: PreparationBinding {
                    request_id: Uuid::from_u128(1),
                    installation_id: Uuid::from_u128(3),
                    key: Uuid::from_u128(4),
                    payload: remote_payload,
                },
            }),
            revision: u64::MAX,
            caused_by: None,
            phase: Phase::Retained,
        },
    ]
}

fn write_v1(fixture: &Fixture, bytes: &[u8]) {
    crate::secure_file::prepare_private_directory(&fixture.store.directory).unwrap();
    let path = fixture.store.directory.join("preparations-v1.json");
    fs::write(&path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn observed_fixture(expected_installation: Option<Uuid>) -> (Fixture, ExistingPendingBinding) {
    let mut fixture = Fixture::new();
    let binding = ExistingPendingBinding {
        installation_id: expected_installation.unwrap_or_else(Uuid::new_v4),
        provider: Provider::Codex,
        key: Uuid::new_v4(),
        started_at: 0,
        generation_id: Uuid::new_v4(),
    };
    fixture.intent.target = Target::ExistingPending {
        original: ExistingPendingTarget::Observed {
            provider: binding.provider,
            key: binding.key,
            started_at: binding.started_at,
            expected_installation,
        },
    };
    (fixture, binding)
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
    prepared_payload_mut(&mut changed_config).copy_codex_config = false;
    let mut changed_provider = fixture.intent.clone();
    prepared_payload_mut(&mut changed_provider).provider = Provider::Claude;
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
    prepared_binding_mut(&mut wrong_request).request_id = Uuid::new_v4();
    let mut wrong_installation = binding.clone();
    prepared_binding_mut(&mut wrong_installation).installation_id = Uuid::new_v4();
    let mut wrong_key = binding.clone();
    prepared_binding_mut(&mut wrong_key).key = Uuid::new_v4();
    let mut wrong_payload = binding.clone();
    prepared_binding_mut(&mut wrong_payload)
        .payload
        .copy_codex_config = false;
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
    prepared_binding_mut(&mut duplicate_home).request_id = second.request_id;
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
    newer["version"] = (VERSION + 1).into();
    let mut extra: serde_json::Value = serde_json::from_slice(&original).unwrap();
    extra["records"][0]["intent"]["target"]["payload"]["unsupported_policy"] = true.into();
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
    prepared_binding_mut(&mut unknown_binding).request_id = unknown.request_id;
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

#[test]
fn v1_visibility_and_equal_migration_preserve_identity_revision_and_durability() {
    let expected = expected_v1_records();
    let cancelled = &expected[1];
    let binding = cancelled.binding.as_ref().unwrap();
    for operation in ["upsert", "bind", "sync_failure"] {
        let fixture = Fixture::new();
        write_v1(&fixture, V1_DOCUMENT.as_bytes());
        let path = fixture.store.directory.join("preparations-v1.json");
        let inode = fs::metadata(&path).unwrap().ino();
        assert_eq!(fixture.store.load().unwrap(), Some(expected.clone()));
        assert_eq!(fs::read(&path).unwrap(), V1_DOCUMENT.as_bytes());
        assert_eq!(
            fs::metadata(&path).unwrap().ino(),
            inode,
            "load is visibility only"
        );

        let migrated = match operation {
            "upsert" => fixture
                .store
                .upsert(&cancelled.intent, Desired::Prepare)
                .unwrap(),
            "bind" => {
                assert_eq!(
                    fixture
                        .store
                        .bind(&cancelled.intent, cancelled.revision - 1, binding)
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::WouldBlock
                );
                assert_eq!(fs::read(&path).unwrap(), V1_DOCUMENT.as_bytes());
                fixture
                    .store
                    .bind(&cancelled.intent, cancelled.revision, binding)
                    .unwrap()
            }
            "sync_failure" => {
                let result = fixture.store.change(
                    &cancelled.intent,
                    Change::Intent(Desired::Prepare),
                    |_| Err(io::Error::from_raw_os_error(libc::EIO)),
                );
                assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EIO));
                let visible = fs::read(&path).unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&visible).unwrap()["version"],
                    VERSION
                );
                assert_eq!(fixture.store.load().unwrap(), Some(expected.clone()));
                let held = fixture.store.lock(false).unwrap().unwrap();
                drop(held);
                let mut calls = 0;
                let result = fixture.store.change(
                    &cancelled.intent,
                    Change::Intent(Desired::Prepare),
                    |_| {
                        calls += 1;
                        Err(io::Error::from_raw_os_error(libc::EIO))
                    },
                );
                assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EIO));
                assert_eq!(
                    calls, 1,
                    "visible V3 cannot replace an outstanding durability barrier"
                );
                assert_eq!(fs::read(&path).unwrap(), visible);
                let result = fixture
                    .store
                    .change(
                        &cancelled.intent,
                        Change::Intent(Desired::Prepare),
                        |namespace| {
                            calls += 1;
                            namespace.sync()
                        },
                    )
                    .unwrap();
                assert_eq!(calls, 2);
                result
            }
            _ => unreachable!(),
        };
        assert_eq!(
            &migrated, cancelled,
            "encoding migration cannot bump even u64::MAX"
        );
        let bytes = fs::read(&path).unwrap();
        assert_ne!(bytes, V1_DOCUMENT.as_bytes());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["version"],
            VERSION
        );
        assert_ne!(
            fs::metadata(&path).unwrap().ino(),
            inode,
            "migration atomically replaces the same legacy path"
        );
        assert_eq!(
            fixture.store.load().unwrap(),
            Some(expected.clone()),
            "all identities, options, routes, bindings, revisions and record order survive"
        );
    }
}

#[test]
fn v1_rejected_documents_and_expanding_migrations_preserve_original_bytes() {
    let fixture = Fixture::new();
    let expected = expected_v1_records();
    let original: serde_json::Value = serde_json::from_str(V1_DOCUMENT).unwrap();
    let mut future = original.clone();
    future["version"] = (VERSION + 1).into();
    let mut mislabeled = original.clone();
    mislabeled["version"] = 2.into();
    let mut extra = original.clone();
    extra["records"][0]["intent"]["payload"]["unsupported_policy"] = true.into();
    let mut omitted = original.clone();
    omitted["records"][0]["intent"]
        .as_object_mut()
        .unwrap()
        .remove("blocked_login");
    let duplicate = V1_DOCUMENT.replacen(
        "\"provider\": \"claude\"",
        "\"provider\": \"claude\", \"provider\": \"claude\"",
        1,
    );
    assert_ne!(duplicate, V1_DOCUMENT);
    let mut oversized = V1_DOCUMENT.as_bytes().to_vec();
    oversized.resize(MAX_BYTES as usize + 1, b' ');
    serde_json::from_slice::<serde_json::Value>(&oversized).unwrap();
    for (kind, bytes) in [
        ("corrupt", b"{broken".to_vec()),
        ("future", serde_json::to_vec(&future).unwrap()),
        ("schema_mismatch", serde_json::to_vec(&mislabeled).unwrap()),
        ("extra", serde_json::to_vec(&extra).unwrap()),
        ("omitted", serde_json::to_vec(&omitted).unwrap()),
        ("duplicate", duplicate.into_bytes()),
        ("oversized", oversized),
    ] {
        write_v1(&fixture, &bytes);
        let error = fixture.store.load().unwrap_err();
        if kind == "future" {
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        } else if kind == "oversized" {
            assert_eq!(error.to_string(), "The account setup outbox is too large");
        } else if kind == "duplicate" {
            assert!(error.to_string().contains("repeats a field"));
        }
        assert!(
            fixture
                .store
                .upsert(&expected[1].intent, Desired::Prepare)
                .is_err(),
            "{kind}"
        );
        assert_eq!(fixture.bytes(), bytes, "{kind}");
        assert!(fixture.store.lock(false).unwrap().is_some());
    }

    // A valid, fully readable V1 can expand beyond the bound when current encoding adds
    // target tags. Migration must preserve it, not partially publish the new schema.
    let template = original["records"][0].clone();
    let empty_size = serde_json::to_vec(&serde_json::json!({"version": 1, "records": []}))
        .unwrap()
        .len();
    let record_size = serde_json::to_vec(&template).unwrap().len();
    let count = (MAX_BYTES as usize - empty_size + 1) / (record_size + 1);
    assert!(count > 1);
    let mut normalized = Vec::new();
    let records: Vec<_> = (1..=count)
        .map(|index| {
            let id = Uuid::from_u128(index as u128);
            let mut record = template.clone();
            record["intent"]["request_id"] = id.to_string().into();
            let mut expected_record = expected[0].clone();
            expected_record.intent.request_id = id;
            normalized.push(expected_record);
            record
        })
        .collect();
    let bytes = serde_json::to_vec(&serde_json::json!({"version": 1, "records": records})).unwrap();
    assert!(bytes.len() as u64 <= MAX_BYTES);
    let v2 = Journal {
        version: VERSION,
        records: normalized.clone(),
    };
    v2.validate().unwrap();
    assert!(serde_json::to_vec(&v2).unwrap().len() as u64 > MAX_BYTES);
    write_v1(&fixture, &bytes);
    assert_eq!(fixture.store.load().unwrap(), Some(normalized.clone()));
    let inode = fs::metadata(fixture.store.directory.join(JOURNAL))
        .unwrap()
        .ino();
    let error = fixture
        .store
        .upsert(&normalized[0].intent, Desired::Prepare)
        .unwrap_err();
    assert_eq!(error.to_string(), "The account setup outbox is too large");
    assert_eq!(fixture.bytes(), bytes);
    assert_eq!(
        fs::metadata(fixture.store.directory.join(JOURNAL))
            .unwrap()
            .ino(),
        inode
    );
    assert_eq!(fixture.store.load().unwrap(), Some(normalized));
    assert!(fixture.store.lock(false).unwrap().is_some());
}

#[test]
fn observed_pending_identity_and_cancel_survive_late_binding_and_revision_cas() {
    for installation in [None, Some(Uuid::new_v4())] {
        let (fixture, binding) = observed_fixture(installation);
        let original = fixture.intent.clone();
        let prepare = fixture.store.upsert(&original, Desired::Prepare).unwrap();
        assert!(prepare.binding.is_none());
        let cancelled = fixture.store.upsert(&original, Desired::Cancel).unwrap();
        let bytes = fixture.bytes();
        let mut provider = binding.clone();
        provider.provider = Provider::Claude;
        let mut key = binding.clone();
        key.key = Uuid::new_v4();
        let mut time = binding.clone();
        time.started_at = 1;
        let mut nil_installation = binding.clone();
        nil_installation.installation_id = Uuid::nil();
        let mut nil_generation = binding.clone();
        nil_generation.generation_id = Uuid::nil();
        let mut wrong = vec![provider, key, time, nil_installation, nil_generation];
        if installation.is_some() {
            let mut changed = binding.clone();
            changed.installation_id = Uuid::new_v4();
            wrong.push(changed);
        }
        for binding in wrong {
            assert!(
                fixture
                    .store
                    .bind(
                        &original,
                        cancelled.revision,
                        &Binding::ExistingPending { binding }
                    )
                    .is_err()
            );
            assert_eq!(fixture.bytes(), bytes);
        }
        let wrong_tag = Binding::Prepared {
            binding: PreparationBinding {
                request_id: original.request_id,
                installation_id: binding.installation_id,
                key: binding.key,
                payload: PreparationPayload {
                    provider: binding.provider,
                    copy_codex_config: false,
                },
            },
        };
        assert!(
            fixture
                .store
                .bind(&original, cancelled.revision, &wrong_tag)
                .is_err()
        );
        assert_eq!(fixture.bytes(), bytes);

        let exact = Binding::ExistingPending {
            binding: binding.clone(),
        };
        for revision in [prepare.revision, cancelled.revision + 1] {
            assert_eq!(
                fixture
                    .store
                    .bind(&original, revision, &exact)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(fixture.bytes(), bytes);
        }
        let bound = fixture
            .store
            .bind(&original, cancelled.revision, &exact)
            .unwrap();
        assert_eq!(bound.intent, original);
        assert_eq!(bound.binding, Some(exact.clone()));
        assert_eq!(bound.desired, Desired::Cancel);
        assert_eq!(bound.revision, cancelled.revision + 1);
        let bytes = fixture.bytes();
        let mut generation = binding.clone();
        generation.generation_id = Uuid::new_v4();
        assert!(
            fixture
                .store
                .bind(
                    &original,
                    bound.revision,
                    &Binding::ExistingPending {
                        binding: generation
                    }
                )
                .is_err()
        );
        assert_eq!(fixture.bytes(), bytes);
        for revision in [cancelled.revision, bound.revision + 1] {
            assert_eq!(
                fixture
                    .store
                    .bind(&original, revision, &exact)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(fixture.bytes(), bytes);
        }
        assert_eq!(
            fixture
                .store
                .bind(&original, bound.revision, &exact)
                .unwrap(),
            bound
        );
        let mut changed = original.clone();
        let Target::ExistingPending {
            original: ExistingPendingTarget::Observed { started_at, .. },
        } = &mut changed.target
        else {
            unreachable!()
        };
        *started_at = 1;
        let mut refreshed = original.clone();
        let Target::ExistingPending {
            original:
                ExistingPendingTarget::Observed {
                    expected_installation,
                    ..
                },
        } = &mut refreshed.target
        else {
            unreachable!()
        };
        *expected_installation = Some(if installation.is_none() {
            binding.installation_id
        } else {
            Uuid::new_v4()
        });
        let mut replaced = original.clone();
        replaced.target = Target::ExistingPending {
            original: ExistingPendingTarget::Bound { binding },
        };
        for intent in [changed, refreshed, replaced] {
            assert!(fixture.store.upsert(&intent, Desired::Prepare).is_err());
            assert_eq!(
                fixture.bytes(),
                bytes,
                "the original zero-time observation stays immutable"
            );
        }
        assert_eq!(
            fixture.store.upsert(&original, Desired::Prepare).unwrap(),
            bound
        );
        assert_eq!(
            Store::at(fixture.store.directory.clone()).load().unwrap(),
            Some(vec![bound])
        );
    }
}

#[test]
fn bound_pending_initialization_and_cross_target_home_collisions_fail_closed() {
    let (mut fixture, binding) = observed_fixture(None);
    fixture.intent.target = Target::ExistingPending {
        original: ExistingPendingTarget::Bound {
            binding: binding.clone(),
        },
    };
    let initial = fixture
        .store
        .upsert(&fixture.intent, Desired::Cancel)
        .unwrap();
    assert_eq!(initial.revision, 1);
    assert_eq!(
        initial.binding,
        Some(Binding::ExistingPending {
            binding: binding.clone()
        })
    );
    let bytes = fixture.bytes();
    let mut duplicate = fixture.intent.clone();
    duplicate.request_id = Uuid::new_v4();
    assert!(fixture.store.upsert(&duplicate, Desired::Prepare).is_err());
    assert_eq!(
        fixture.bytes(),
        bytes,
        "the duplicate is refused during initial upsert"
    );
    let mut wrong = binding.clone();
    wrong.generation_id = Uuid::new_v4();
    assert!(
        fixture
            .store
            .bind(
                &fixture.intent,
                initial.revision,
                &Binding::ExistingPending { binding: wrong }
            )
            .is_err()
    );
    assert_eq!(fixture.bytes(), bytes);

    let prepared = Fixture::new();
    let prepared_record = prepared
        .store
        .upsert(&prepared.intent, Desired::Prepare)
        .unwrap();
    let mut prepared_binding = prepared.binding();
    prepared_binding_mut(&mut prepared_binding).installation_id = binding.installation_id;
    prepared_binding_mut(&mut prepared_binding).key = binding.key;
    let prepared_record = prepared
        .store
        .bind(
            &prepared.intent,
            prepared_record.revision,
            &prepared_binding,
        )
        .unwrap();
    let prepared_bytes = prepared.bytes();
    assert!(
        prepared
            .store
            .upsert(&fixture.intent, Desired::Cancel)
            .is_err()
    );
    assert_eq!(
        prepared.bytes(),
        prepared_bytes,
        "a Bound legacy target cannot alias a Prepared home at initial upsert"
    );
    assert_eq!(prepared.store.load().unwrap(), Some(vec![prepared_record]));

    let valid = Journal {
        version: VERSION,
        records: vec![initial.clone()],
    };
    let mut absent = serde_json::to_value(&valid).unwrap();
    absent["records"][0]["binding"] = serde_json::Value::Null;
    let mut wrong_tag = serde_json::to_value(&valid).unwrap();
    let wrong_tag_binding = Binding::Prepared {
        binding: PreparationBinding {
            request_id: fixture.intent.request_id,
            installation_id: binding.installation_id,
            key: binding.key,
            payload: PreparationPayload {
                provider: binding.provider,
                copy_codex_config: false,
            },
        },
    };
    wrong_tag["records"][0]["binding"] = serde_json::to_value(&wrong_tag_binding).unwrap();
    let mut mismatched = serde_json::to_value(&valid).unwrap();
    mismatched["records"][0]["binding"]["binding"]["generation_id"] =
        Uuid::new_v4().to_string().into();
    for malformed in [absent, wrong_tag, mismatched] {
        let bytes = serde_json::to_vec(&malformed).unwrap();
        fs::write(fixture.store.directory.join(JOURNAL), &bytes).unwrap();
        assert!(fixture.store.load().is_err());
        assert!(
            fixture
                .store
                .upsert(&fixture.intent, Desired::Cancel)
                .is_err(),
            "malformed persisted Bound identity cannot be repaired by replay"
        );
        assert_eq!(fixture.bytes(), bytes);
    }
}

// Literal prior wire: no V3 serializer or new phase/causation defaults build it.
const V2_DOCUMENT: &str = r#"{"version":2,"records":[{"intent":{"request_id":"00000000-0000-0000-0000-000000000001","target":{"kind":"existing_pending","original":{"kind":"observed","provider":"codex","key":"00000000-0000-0000-0000-000000000002","started_at":0,"expected_installation":null}},"original_route":{"route":{"kind":"remote","name":"Original device","host":"original-device","executable":"/usr/bin/cagents"},"route_revision":"route-v1:b58c991cd0d1be3efa21c946bccc2afeb1c7e5354d3d98c55069740004d2b9a4"},"device_code":true,"blocked_login":"00000000-0000-0000-0000-000000000005"},"desired":"cancel","binding":{"kind":"existing_pending","binding":{"installation_id":"00000000-0000-0000-0000-000000000003","provider":"codex","key":"00000000-0000-0000-0000-000000000002","started_at":0,"generation_id":"00000000-0000-0000-0000-000000000004"}},"revision":9}]}"#;

fn linked_fixture() -> (Fixture, Record, Intent, ExistingPendingBinding) {
    let (fixture, binding) = observed_fixture(None);
    let parent = fixture
        .store
        .upsert(&fixture.intent, Desired::Prepare)
        .unwrap();
    let mut child = fixture.intent.clone();
    child.request_id = Uuid::new_v4();
    let Target::ExistingPending {
        original:
            ExistingPendingTarget::Observed {
                expected_installation,
                ..
            },
    } = &mut child.target
    else {
        unreachable!()
    };
    *expected_installation = Some(binding.installation_id);
    (fixture, parent, child, binding)
}

fn parent_link(parent: &Record) -> ParentIntent {
    ParentIntent {
        request_id: parent.intent.request_id,
        revision_at_consent: parent.revision,
    }
}

fn attempt(action: AttemptAction) -> Attempt {
    Attempt {
        id: Uuid::new_v4(),
        action,
    }
}

#[test]
fn v3_literal_v2_migration_retains_every_identity_and_bound_none_history() {
    let fixture = Fixture::new();
    write_v1(&fixture, V2_DOCUMENT.as_bytes());
    let initial = fixture.store.load().unwrap().unwrap();
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].phase, Phase::Retained);
    assert_eq!(initial[0].caused_by, None);
    let raw: serde_json::Value = serde_json::from_str(V2_DOCUMENT).unwrap();
    let mut normalized = serde_json::to_value(&initial[0]).unwrap();
    normalized.as_object_mut().unwrap().remove("phase");
    normalized.as_object_mut().unwrap().remove("caused_by");
    assert_eq!(
        normalized, raw["records"][0],
        "all old fields are conserved"
    );
    assert_eq!(
        fixture.bytes(),
        V2_DOCUMENT.as_bytes(),
        "load cannot migrate or acknowledge"
    );
    let record = &initial[0];
    assert_eq!(
        fixture
            .store
            .upsert(&record.intent, Desired::Prepare)
            .unwrap(),
        *record
    );
    let migrated = fixture.bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&migrated).unwrap()["version"],
        VERSION
    );
    assert_eq!(fixture.store.load().unwrap(), Some(initial.clone()));
    let mut child = record.intent.clone();
    child.request_id = Uuid::new_v4();
    let Target::ExistingPending {
        original:
            ExistingPendingTarget::Observed {
                expected_installation,
                ..
            },
    } = &mut child.target
    else {
        unreachable!()
    };
    *expected_installation = Some(Uuid::from_u128(3));
    assert!(
        fixture
            .store
            .reaffirm(&parent_link(record), &child, Desired::Cancel)
            .is_err()
    );
    assert!(
        fixture
            .store
            .begin(
                &record.intent,
                record.revision,
                &attempt(AttemptAction::Cancel)
            )
            .is_err()
    );
    assert_eq!(
        fixture.bytes(),
        migrated,
        "already-bound None is retained HOLD, not transferred"
    );
    let mut rejected = Vec::new();
    for field in ["phase", "caused_by"] {
        let mut malformed = raw.clone();
        malformed["records"][0][field] = serde_json::Value::Null;
        rejected.push((field, serde_json::to_vec(&malformed).unwrap()));
    }
    let mut unknown = raw.clone();
    unknown["records"][0]["intent"]["target"]["original"]["unsupported_policy"] = true.into();
    rejected.push(("nested_unknown", serde_json::to_vec(&unknown).unwrap()));
    let mut omitted = raw.clone();
    omitted["records"][0]["intent"]["target"]["original"]
        .as_object_mut()
        .unwrap()
        .remove("expected_installation");
    rejected.push(("nested_omitted", serde_json::to_vec(&omitted).unwrap()));
    let duplicate = V2_DOCUMENT.replacen(
        "\"provider\":\"codex\"",
        "\"provider\":\"codex\",\"provider\":\"codex\"",
        1,
    );
    assert_ne!(duplicate, V2_DOCUMENT);
    rejected.push(("nested_duplicate", duplicate.into_bytes()));
    for (kind, bytes) in rejected {
        write_v1(&fixture, &bytes);
        let error = fixture.store.load().unwrap_err();
        if kind == "nested_duplicate" {
            assert!(error.to_string().contains("repeats a field"));
        }
        assert!(
            fixture
                .store
                .upsert(&record.intent, Desired::Cancel)
                .is_err(),
            "{kind}: rejected V2 cannot be normalized by a mutation"
        );
        assert_eq!(
            fixture.bytes(),
            bytes,
            "{kind}: strict V2 rejection must retain the exact old bytes"
        );
    }
}

#[test]
fn v3_migration_and_phase_growth_refuse_before_replacing_bounded_bytes() {
    let fixture = Fixture::new();
    let mut template =
        serde_json::from_str::<serde_json::Value>(V2_DOCUMENT).unwrap()["records"][0].clone();
    template["binding"] = serde_json::Value::Null;
    let empty_size = serde_json::to_vec(&serde_json::json!({"version":2,"records":[]}))
        .unwrap()
        .len();
    let record_size = serde_json::to_vec(&template).unwrap().len();
    let count = (MAX_BYTES as usize - empty_size + 1) / (record_size + 1);
    let records: Vec<_> = (0..count)
        .map(|index| {
            let mut record = template.clone();
            record["intent"]["request_id"] = Uuid::from_u128(index as u128 + 1).to_string().into();
            record
        })
        .collect();
    let bytes = serde_json::to_vec(&serde_json::json!({"version":2,"records":records})).unwrap();
    assert!(bytes.len() as u64 <= MAX_BYTES);
    write_v1(&fixture, &bytes);
    let visible = fixture.store.load().unwrap().unwrap();
    assert!(
        serde_json::to_vec(&Journal {
            version: VERSION,
            records: visible.clone()
        })
        .unwrap()
        .len() as u64
            > MAX_BYTES
    );
    let inode = fs::metadata(fixture.store.directory.join(JOURNAL))
        .unwrap()
        .ino();
    let error = fixture
        .store
        .upsert(&visible[0].intent, Desired::Cancel)
        .unwrap_err();
    assert_eq!(error.to_string(), "The account setup outbox is too large");
    assert_eq!(fixture.bytes(), bytes);
    assert_eq!(
        fs::metadata(fixture.store.directory.join(JOURNAL))
            .unwrap()
            .ino(),
        inode
    );
    assert_eq!(fixture.store.load().unwrap(), Some(visible));

    // A legal current journal can also be too full for an admitted phase.
    let (fixture, _) = observed_fixture(Some(Uuid::new_v4()));
    let first = fixture
        .store
        .upsert(&fixture.intent, Desired::Prepare)
        .unwrap();
    let mut journal = Journal {
        version: VERSION,
        records: vec![first.clone()],
    };
    let overhead = serde_json::to_vec(&Phase::InFlight {
        attempt: attempt(AttemptAction::Resume),
    })
    .unwrap()
    .len()
        - serde_json::to_vec(&Phase::Retained).unwrap().len();
    // Route public metadata can be padded within its existing validation cap.
    let empty_size = serde_json::to_vec(&Journal {
        version: VERSION,
        records: vec![],
    })
    .unwrap()
    .len();
    let record_size = serde_json::to_vec(&first).unwrap().len();
    let count = (MAX_BYTES as usize - 4096 - empty_size + 1).div_ceil(record_size + 1);
    journal.records = (0..count)
        .map(|index| {
            let mut record = first.clone();
            if index != 0 {
                record.intent.request_id = Uuid::new_v4();
            }
            record
        })
        .collect();
    let current_size = serde_json::to_vec(&journal).unwrap().len();
    let target_size = MAX_BYTES as usize - overhead / 2;
    let added = target_size.checked_sub(current_size).unwrap();
    let last = journal.records.last_mut().unwrap();
    let Route::Remote { name, .. } = &mut last.intent.original_route.route else {
        unreachable!()
    };
    assert!(name.len() + added <= 4096);
    name.push_str(&"x".repeat(added));
    last.intent.original_route.route_revision =
        route_revision(&last.intent.original_route.route).unwrap();
    let bytes = serde_json::to_vec(&journal).unwrap();
    assert!(
        bytes.len() as u64 <= MAX_BYTES && bytes.len() + overhead > MAX_BYTES as usize,
        "the fixture must straddle the real phase-growth boundary"
    );
    journal.validate().unwrap();
    write_v1(&fixture, &bytes);
    assert_eq!(
        fixture
            .store
            .begin(
                &first.intent,
                first.revision,
                &attempt(AttemptAction::Resume)
            )
            .unwrap_err()
            .to_string(),
        "The account setup outbox is too large"
    );
    assert_eq!(fixture.bytes(), bytes);
}

#[test]
fn v3_explicit_consent_has_one_canonical_child_and_preserves_historical_parent_revision() {
    let (fixture, parent, child, _) = linked_fixture();
    let original_bytes = fixture.bytes();
    assert_eq!(fixture.store.replay(&parent).unwrap(), parent);
    assert_eq!(fixture.bytes(), original_bytes);
    for revision in [0, parent.revision + 1] {
        let link = ParentIntent {
            request_id: parent.intent.request_id,
            revision_at_consent: revision,
        };
        assert!(
            fixture
                .store
                .reaffirm(&link, &child, Desired::Prepare)
                .is_err()
        );
        assert_eq!(fixture.bytes(), original_bytes);
    }
    let inserted = fixture
        .store
        .reaffirm(&parent_link(&parent), &child, Desired::Prepare)
        .unwrap();
    assert_eq!(inserted.caused_by, Some(parent_link(&parent)));
    assert_eq!(inserted.phase, Phase::Retained);
    assert_eq!(inserted.binding, None);
    let persisted = fixture.bytes();
    assert!(
        fixture.store.replay(&parent).is_err(),
        "the unchanged Retained parent loses acknowledgement ownership once its child exists"
    );
    assert_eq!(fixture.bytes(), persisted);
    assert_eq!(
        fixture
            .store
            .reaffirm(&parent_link(&parent), &child, Desired::Prepare)
            .unwrap(),
        inserted
    );
    assert_eq!(fixture.bytes(), persisted);
    assert_eq!(
        fixture
            .store
            .load()
            .unwrap()
            .unwrap()
            .into_iter()
            .find(|r| r.intent == parent.intent)
            .unwrap(),
        parent
    );
    let mut competing = child.clone();
    competing.request_id = Uuid::new_v4();
    assert!(
        fixture
            .store
            .reaffirm(&parent_link(&parent), &competing, Desired::Prepare)
            .is_err()
    );
    assert_eq!(
        fixture.bytes(),
        persisted,
        "a competing click cannot redirect to the first child"
    );
    assert!(
        fixture
            .store
            .reaffirm(&parent_link(&inserted), &competing, Desired::Prepare)
            .is_err()
    );
    let mut self_link = parent_link(&parent);
    self_link.request_id = child.request_id;
    assert!(
        fixture
            .store
            .reaffirm(&self_link, &child, Desired::Prepare)
            .is_err()
    );
    assert_eq!(fixture.bytes(), persisted);

    let cancelled_parent = fixture
        .store
        .upsert(&parent.intent, Desired::Cancel)
        .unwrap();
    let cancelled_bytes = fixture.bytes();
    assert!(
        fixture
            .store
            .reaffirm(&parent_link(&parent), &child, Desired::Prepare)
            .is_err(),
        "an exact initial child retry must still recheck the current parent Cancel"
    );
    assert_eq!(fixture.bytes(), cancelled_bytes);
    let history = fixture.store.load().unwrap().unwrap();
    assert!(history.contains(&inserted));
    assert!(history.contains(&cancelled_parent));
    let cancelled_child = fixture.store.upsert(&child, Desired::Cancel).unwrap();
    assert_eq!(cancelled_child.intent, inserted.intent);
    assert_eq!(cancelled_child.caused_by, inserted.caused_by);
    assert_eq!(cancelled_child.phase, Phase::Retained);
    assert_eq!(cancelled_child.desired, Desired::Cancel);

    let (fixture, parent, child, _) = linked_fixture();
    let cancelled = fixture
        .store
        .upsert(&parent.intent, Desired::Cancel)
        .unwrap();
    assert!(cancelled.revision > parent.revision);
    assert!(
        fixture
            .store
            .reaffirm(&parent_link(&parent), &child, Desired::Prepare)
            .is_err()
    );
    let accepted = fixture
        .store
        .reaffirm(&parent_link(&parent), &child, Desired::Cancel)
        .unwrap();
    assert_eq!(accepted.desired, Desired::Cancel);
    assert_eq!(
        accepted.caused_by.unwrap().revision_at_consent,
        parent.revision,
        "historical consent revision need not equal the later parent revision"
    );
    assert_eq!(
        fixture
            .store
            .load()
            .unwrap()
            .unwrap()
            .into_iter()
            .find(|r| r.intent == parent.intent)
            .unwrap(),
        cancelled
    );
}

#[test]
fn v3_parent_cancellation_fences_child_resume_even_without_a_child_revision_change() {
    let (fixture, parent, child, binding) = linked_fixture();
    let inserted = fixture
        .store
        .reaffirm(&parent_link(&parent), &child, Desired::Prepare)
        .unwrap();
    let typed_binding = Binding::ExistingPending {
        binding: binding.clone(),
    };
    let bytes = fixture.bytes();
    assert!(
        fixture
            .store
            .bind(&parent.intent, parent.revision, &typed_binding)
            .is_err()
    );
    assert!(
        fixture
            .store
            .begin(
                &parent.intent,
                parent.revision,
                &attempt(AttemptAction::Resume)
            )
            .is_err()
    );
    let parent_attempt = attempt(AttemptAction::Resume);
    let unowned_parent = AttemptEnvelope {
        intent: parent.intent.clone(),
        desired: parent.desired,
        binding: parent.binding.clone(),
        revision: parent.revision,
        attempt: parent_attempt.clone(),
    };
    assert!(
        fixture
            .store
            .settle(
                &unowned_parent,
                Some(&typed_binding),
                &Phase::Refused {
                    attempt: parent_attempt
                }
            )
            .is_err(),
        "a captured parent revision is not an owned attempt or a successor result"
    );
    assert_eq!(fixture.bytes(), bytes);
    let resume = attempt(AttemptAction::Resume);
    let owner = fixture
        .store
        .begin(&child, inserted.revision, &resume)
        .unwrap();
    let in_flight = fixture
        .store
        .load()
        .unwrap()
        .unwrap()
        .into_iter()
        .find(|r| r.intent == child)
        .unwrap();
    fixture
        .store
        .upsert(&parent.intent, Desired::Cancel)
        .unwrap();
    let unchanged_child = fixture
        .store
        .load()
        .unwrap()
        .unwrap()
        .into_iter()
        .find(|r| r.intent == child)
        .unwrap();
    assert_eq!(
        in_flight, unchanged_child,
        "parent Cancel cannot silently mutate the child"
    );
    let bytes = fixture.bytes();
    assert!(
        fixture
            .store
            .settle(
                &owner,
                Some(&typed_binding),
                &Phase::MetadataConfirmed {
                    attempt: resume.clone(),
                    state: ExistingPendingState::Pending,
                }
            )
            .is_err()
    );
    assert!(
        fixture.store.replay(&in_flight).is_err(),
        "exact child revision does not waive parent Cancel"
    );
    assert!(
        fixture
            .store
            .bind(&child, in_flight.revision, &typed_binding)
            .is_err(),
        "the legacy binding API cannot bypass the atomic result owner"
    );
    assert_eq!(fixture.bytes(), bytes);
    let hold = fixture
        .store
        .hold(&child, in_flight.revision, &resume)
        .unwrap();
    assert_eq!(
        hold.phase,
        Phase::NeedsReconcile {
            attempt: resume.clone()
        }
    );
    assert_eq!(
        fixture.store.replay(&hold).unwrap(),
        hold,
        "HOLD replay grants no dispatch or ACK"
    );
    assert!(
        fixture.store.replay_attempt(&hold).is_err(),
        "durable HOLD cannot yield a fresh Resume dispatch envelope"
    );
    assert!(
        fixture
            .store
            .begin(&child, hold.revision, &attempt(AttemptAction::Resume))
            .is_err()
    );
    let cancelled = fixture.store.upsert(&child, Desired::Cancel).unwrap();
    assert_eq!(cancelled.phase.attempt(), Some(&resume));
    assert!(
        fixture
            .store
            .settle(&owner, None, &Phase::Refused { attempt: resume })
            .is_err()
    );
    let cancellation = attempt(AttemptAction::Cancel);
    let cancel_owner = fixture
        .store
        .begin(&child, cancelled.revision, &cancellation)
        .unwrap();
    assert_eq!(
        cancel_owner.intent.request_id, child.request_id,
        "wire Cancel uses this stable local operation id"
    );
    let settled = fixture
        .store
        .settle(
            &cancel_owner,
            Some(&typed_binding),
            &Phase::MetadataConfirmed {
                attempt: cancellation.clone(),
                state: ExistingPendingState::CancelledRetained,
            },
        )
        .unwrap();
    assert_eq!(settled.desired, Desired::Cancel);
    assert_eq!(settled.phase.attempt(), Some(&cancellation));
    assert!(
        fixture
            .store
            .upsert(&child, Desired::Prepare)
            .unwrap()
            .desired
            == Desired::Cancel
    );
}

#[test]
fn v3_exact_attempt_and_atomic_binding_settlement_require_the_actual_durability_barrier() {
    let (fixture, binding) = observed_fixture(Some(Uuid::new_v4()));
    let first = fixture
        .store
        .upsert(&fixture.intent, Desired::Prepare)
        .unwrap();
    let resume = attempt(AttemptAction::Resume);
    let failed = fixture
        .store
        .begin_with_sync(&fixture.intent, first.revision, &resume, |_| {
            Err(io::Error::from_raw_os_error(libc::EIO))
        });
    assert_eq!(failed.unwrap_err().raw_os_error(), Some(libc::EIO));
    let visible = fixture.store.load().unwrap().unwrap().remove(0);
    assert_eq!(visible.phase.attempt(), Some(&resume));
    let bytes = fixture.bytes();
    let mut calls = 0;
    assert!(
        fixture
            .store
            .replay_with_sync(&visible, |_| {
                calls += 1;
                Err(io::Error::from_raw_os_error(libc::EIO))
            })
            .is_err()
    );
    assert_eq!(calls, 1);
    assert_eq!(fixture.bytes(), bytes);
    let durable = fixture
        .store
        .replay_with_sync(&visible, |namespace| {
            calls += 1;
            namespace.sync()
        })
        .unwrap();
    assert_eq!(calls, 2);
    let owner = fixture.store.replay_attempt(&durable).unwrap();
    let mut stale = owner.clone();
    stale.revision -= 1;
    let phase = Phase::MetadataConfirmed {
        attempt: resume.clone(),
        state: ExistingPendingState::Pending,
    };
    let typed = Binding::ExistingPending { binding };
    for wrong in [
        stale,
        AttemptEnvelope {
            attempt: attempt(AttemptAction::Resume),
            ..owner.clone()
        },
        AttemptEnvelope {
            desired: Desired::Cancel,
            ..owner.clone()
        },
    ] {
        assert!(fixture.store.settle(&wrong, Some(&typed), &phase).is_err());
        assert_eq!(fixture.bytes(), bytes);
    }
    assert!(
        fixture.store.settle(&owner, None, &phase).is_err(),
        "metadata confirmation requires a real exact binding"
    );
    assert!(
        fixture
            .store
            .settle(&owner, Some(&typed), &Phase::Retained)
            .is_err()
    );
    let failed = fixture
        .store
        .settle_with_sync(&owner, Some(&typed), &phase, |_| {
            Err(io::Error::from_raw_os_error(libc::EIO))
        });
    assert_eq!(failed.unwrap_err().raw_os_error(), Some(libc::EIO));
    let post = fixture.store.load().unwrap().unwrap().remove(0);
    assert_eq!(post.binding, Some(typed));
    assert_eq!(post.phase, phase);
    assert_eq!(
        post.revision,
        owner.revision + 1,
        "binding and phase share one revision/barrier"
    );
    assert!(
        fixture
            .store
            .settle(
                &owner,
                None,
                &Phase::Refused {
                    attempt: resume.clone()
                }
            )
            .is_err()
    );
    let replay = fixture.store.replay(&post).unwrap();
    assert_eq!(replay, post);
    let next = attempt(AttemptAction::Resume);
    let owner = fixture
        .store
        .begin(&fixture.intent, post.revision, &next)
        .unwrap();
    let refused = fixture
        .store
        .settle(
            &owner,
            None,
            &Phase::Refused {
                attempt: next.clone(),
            },
        )
        .unwrap();
    assert_eq!(refused.phase.attempt(), Some(&next));
    assert!(
        fixture.store.replay(&post).is_err(),
        "late post-state cannot acknowledge the newer owner"
    );
    assert!(
        fixture
            .store
            .begin(&fixture.intent, refused.revision, &next)
            .is_err(),
        "an explicit retry needs a distinct attempt id"
    );
    assert!(
        fixture.store.replay_attempt(&refused).is_err(),
        "a settled record cannot yield a dispatch envelope"
    );
}

#[test]
fn v3_malformed_consent_and_phase_documents_are_held_without_repair() {
    let (fixture, parent, child, _) = linked_fixture();
    let inserted = fixture
        .store
        .reaffirm(&parent_link(&parent), &child, Desired::Prepare)
        .unwrap();
    let good = fixture.bytes();
    let raw: serde_json::Value = serde_json::from_slice(&good).unwrap();
    let child_index = raw["records"]
        .as_array()
        .unwrap()
        .iter()
        .position(|r| r["intent"]["request_id"] == child.request_id.to_string())
        .unwrap();
    let parent_index = 1 - child_index;
    let mut cases = Vec::new();
    let mut bad = raw.clone();
    bad["records"][child_index]["caused_by"]["revision_at_consent"] = 0.into();
    cases.push(bad);
    let mut bad = raw.clone();
    bad["records"][child_index]["caused_by"]["revision_at_consent"] = (parent.revision + 1).into();
    cases.push(bad);
    let mut bad = raw.clone();
    bad["records"][parent_index]["caused_by"] =
        serde_json::to_value(parent_link(&inserted)).unwrap();
    cases.push(bad);
    let mut bad = raw.clone();
    bad["records"][child_index]["intent"]["target"]["original"]["started_at"] = 1.into();
    cases.push(bad);
    let mut bad = raw.clone();
    bad["records"][child_index]["phase"] = serde_json::json!({"kind":"in_flight"});
    cases.push(bad);
    let mut bad = raw.clone();
    bad["records"][child_index]["phase"] =
        serde_json::json!({"kind":"refused","attempt":{"id":Uuid::nil(),"action":"resume"}});
    cases.push(bad);
    let mut bad = raw.clone();
    bad["records"][child_index]["phase"] = serde_json::json!({"kind":"metadata_confirmed","attempt":attempt(AttemptAction::Resume),"state":"pending"});
    cases.push(bad);
    for malformed in cases {
        let bytes = serde_json::to_vec(&malformed).unwrap();
        write_v1(&fixture, &bytes);
        assert!(fixture.store.load().is_err());
        assert!(fixture.store.upsert(&child, Desired::Cancel).is_err());
        assert_eq!(
            fixture.bytes(),
            bytes,
            "invalid history cannot be normalized or replaced"
        );
    }
    write_v1(&fixture, &good);
    assert_eq!(fixture.store.load().unwrap().unwrap().len(), 2);
}
