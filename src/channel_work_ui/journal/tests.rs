use super::*;
use crate::{
    channel_work::{Change, Home, Role, Target},
    github_identity::{GitHubRepoKey, PullRequestKey},
    model::{BootId, ChannelId, HostIncarnation, HostSessionId},
};
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    sync::{Arc, Barrier},
};

fn record() -> Record {
    Record::new(
        Mutation {
            home: Home {
                registry: Uuid::new_v4(),
                channel: ChannelId::new(),
                creation_ordinal: 2,
            },
            expected_revision: u64::MAX - 1,
            operation: Uuid::new_v4(),
            change: Change::Claim {
                target: Target::PullRequest(
                    PullRequestKey::new(
                        GitHubRepoKey::new("github.com", None, "owner", "repository").unwrap(),
                        42,
                    )
                    .unwrap(),
                ),
                role: Role::Contribute,
            },
        },
        None,
        Destination {
            profile: Some(Profile {
                name: "laptop".into(),
                host: "my-laptop".into(),
                executable: "cagents".into(),
            }),
            socket: "/tmp/original-bridge/control.sock".into(),
            proof: HostSessionProof {
                host_session_id: HostSessionId::new("original-host"),
                boot_id: BootId::new("original-boot"),
                host_incarnation: HostIncarnation::new("original-incarnation"),
            },
        },
        "Original channel".into(),
        1234,
    )
    .unwrap()
}

fn selected_record() -> Record {
    let original = record();
    Record::new(
        Mutation {
            change: Change::Claim {
                target: Target::Local(Uuid::new_v4()),
                role: Role::Contribute,
            },
            ..original.mutation
        },
        Some(Uuid::new_v4()),
        original.destination,
        original.title,
        original.created_ms,
    )
    .unwrap()
}

fn journal(directory: &tempfile::TempDir) -> Journal {
    Journal::at(directory.path().join("pending")).unwrap()
}

fn receipt(record: &Record) -> Receipt {
    Receipt {
        home: record.mutation.home,
        operation: record.operation(),
        revision: record.mutation.expected_revision + 1,
        changed: true,
        sequence: 99,
    }
}

#[test]
fn restart_recovers_exact_user_request_route_and_full_u64_cas_until_matching_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let original = record();
    let first = journal(&directory);
    assert_eq!(first.reserve(&original), Ok(Reservation::Published));
    let path = first.record_path(original.operation());
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(&first.path).unwrap().mode() & 0o777, 0o700);
    let bytes = fs::read(&path).unwrap();
    drop(first);

    let reopened = journal(&directory);
    let loaded = reopened.load().unwrap();
    assert_eq!(loaded.records, vec![original.clone()]);
    assert!(loaded.problems.is_empty());
    assert_eq!(reopened.reserve(&original), Ok(Reservation::AlreadyPresent));
    assert_eq!(
        fs::read(&path).unwrap(),
        bytes,
        "no route/proof/request rewrite on reopen"
    );
    let Request::Mutate { mutation, caller } = loaded.records[0].wire_request().unwrap() else {
        panic!()
    };
    assert_eq!(mutation, original.mutation);
    assert!(caller.is_none());
    let other_window = journal(&directory);
    let also_pending = other_window.load().unwrap().records.remove(0);
    reopened.settle(&original, &receipt(&original)).unwrap();
    assert!(!path.exists());
    other_window
        .settle(&also_pending, &receipt(&original))
        .unwrap();
    assert!(
        !path.exists(),
        "a late matching receipt never recreates its record"
    );
    let mut wrong = receipt(&original);
    wrong.operation = Uuid::new_v4();
    assert_eq!(
        other_window.settle(&also_pending, &wrong),
        Err(Error::Conflict),
        "missing file alone cannot settle a different operation"
    );
    assert!(reopened.load().unwrap().records.is_empty());
}

#[test]
fn legacy_selection_records_remain_visible_and_cannot_be_reinterpreted_as_user_mutations() {
    for selection in [Uuid::new_v4(), Uuid::nil()] {
        let directory = tempfile::tempdir().unwrap();
        let storage = journal(&directory);
        let original = record();
        storage.reserve(&original).unwrap();
        let path = storage.record_path(original.operation());
        let mut legacy = original.clone();
        legacy.mutation.change = Change::Claim {
            target: Target::Local(Uuid::new_v4()),
            role: Role::Steward,
        };
        legacy.selection = Some(selection);
        let bytes = serde_json::to_vec(&legacy).unwrap();
        fs::write(&path, &bytes).unwrap();
        let before = fs::symlink_metadata(&path).unwrap();
        assert_eq!(legacy.validate(), Err(Error::InvalidRecord));
        assert_eq!(legacy.wire_request(), Err(Error::InvalidRecord));
        assert_eq!(storage.reserve(&legacy), Err(Error::InvalidRecord));
        let loaded = storage.load().unwrap();
        assert!(loaded.records.is_empty());
        assert_eq!(loaded.occupied, 1);
        assert_eq!(loaded.problems.len(), 1);
        assert_eq!(loaded.problems[0].operation, Some(original.operation()));
        assert_eq!(loaded.problems[0].kind, ProblemKind::InvalidRecord);
        assert_eq!(storage.reserve(&record()), Err(Error::Full));
        assert_eq!(
            storage.settle(&legacy, &receipt(&legacy)),
            Err(Error::Conflict)
        );
        assert_eq!(
            storage.settle(&original, &receipt(&original)),
            Err(Error::Conflict)
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let after = fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            (before.dev(), before.ino(), before.mode(), before.len()),
            (after.dev(), after.ino(), after.mode(), after.len()),
            "unsupported recovery evidence is neither rewritten nor removed"
        );
    }
}

#[test]
fn selected_local_publication_uncertainty_reopens_only_the_exact_v2_user_request() {
    let directory = tempfile::tempdir().unwrap();
    let original = selected_record();
    let mut storage = journal(&directory);
    storage.fail_directory_sync = true;
    assert_eq!(storage.reserve(&original), Err(Error::PublicationUncertain));
    let path = storage.record_path(original.operation());
    let bytes = fs::read(&path).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["version"],
        2
    );
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    drop(storage);

    let reopened = journal(&directory);
    let loaded = reopened.load().unwrap();
    assert_eq!(loaded.records, [original.clone()]);
    assert!(loaded.problems.is_empty());
    let Request::ApplySelection {
        selection,
        mutation,
        caller,
    } = loaded.records[0].wire_request().unwrap()
    else {
        panic!("v2 retains the original selected-Local shape")
    };
    assert_eq!(Some(selection), original.selection());
    assert_eq!(mutation, *original.mutation());
    assert!(
        caller.is_none(),
        "the GUI record never restores Actor authority"
    );
    let mut rebound = original.clone();
    rebound.selection = Some(Uuid::new_v4());
    assert_eq!(reopened.reserve(&rebound), Err(Error::Conflict));
    assert_eq!(
        reopened.settle(&rebound, &receipt(&original)),
        Err(Error::Conflict)
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(reopened.reserve(&original), Ok(Reservation::AlreadyPresent));
    assert_eq!(
        fs::read(&path).unwrap(),
        bytes,
        "sync repair cannot reseed the original"
    );
    reopened.settle(&original, &receipt(&original)).unwrap();
    assert!(reopened.load().unwrap().records.is_empty());
}

#[test]
fn selected_format_does_not_upgrade_legacy_or_unsupported_shapes_and_preserves_evidence() {
    for variant in 0..5 {
        let directory = tempfile::tempdir().unwrap();
        let storage = journal(&directory);
        let original = selected_record();
        storage.reserve(&original).unwrap();
        let path = storage.record_path(original.operation());
        let mut invalid = original.clone();
        match variant {
            0 => invalid.version = 1,
            1 => invalid.version = 3,
            2 => invalid.selection = None,
            3 => invalid.selection = Some(Uuid::nil()),
            _ => invalid.mutation.change = record().mutation.change,
        }
        let bytes = serde_json::to_vec(&invalid).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert_eq!(invalid.validate(), Err(Error::InvalidRecord));
        let loaded = storage.load().unwrap();
        assert!(loaded.records.is_empty());
        assert_eq!(loaded.occupied, 1);
        assert_eq!(loaded.problems[0].kind, ProblemKind::InvalidRecord);
        assert_eq!(storage.reserve(&original), Err(Error::Conflict));
        assert_eq!(
            storage.settle(&original, &receipt(&original)),
            Err(Error::Conflict)
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn another_window_cannot_rebind_existing_operation_to_a_new_request_or_destination() {
    let directory = tempfile::tempdir().unwrap();
    let first = journal(&directory);
    let second = journal(&directory);
    let original = record();
    first.reserve(&original).unwrap();
    let path = first.record_path(original.operation());
    let before = fs::read(&path).unwrap();
    for variant in 0..4 {
        let mut changed = original.clone();
        match variant {
            0 => changed.mutation.expected_revision -= 1,
            1 => changed.mutation.home.creation_ordinal += 1,
            2 => changed.destination.socket = "/tmp/other/control.sock".into(),
            _ => changed.destination.proof.host_incarnation = HostIncarnation::new("replacement"),
        }
        assert_eq!(second.reserve(&changed), Err(Error::Conflict));
        assert_eq!(
            second.settle(&changed, &receipt(&changed)),
            Err(Error::Conflict)
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    let mut wrong = receipt(&original);
    wrong.operation = Uuid::new_v4();
    assert_eq!(second.settle(&original, &wrong), Err(Error::Conflict));
    wrong = receipt(&original);
    wrong.revision -= 1;
    assert_eq!(second.settle(&original, &wrong), Err(Error::Conflict));
    assert_eq!(second.load().unwrap().records, vec![original.clone()]);
    assert!(original.confirms_receipt(&receipt(&original)));
    let mut exhausted = original.clone();
    exhausted.mutation.expected_revision = u64::MAX;
    let mut overflow = receipt(&original);
    overflow.revision = 0;
    assert!(
        !exhausted.confirms_receipt(&overflow),
        "checked revision arithmetic"
    );
}

#[test]
fn simultaneous_windows_cannot_publish_past_global_capacity_or_evict_existing_unknowns() {
    let directory = tempfile::tempdir().unwrap();
    let storage = journal(&directory);
    let originals = (0..MAX_RECORDS - 1).map(|_| record()).collect::<Vec<_>>();
    for original in &originals {
        storage.reserve(original).unwrap();
    }
    let barrier = Arc::new(Barrier::new(3));
    let contenders = (0..2)
        .map(|_| {
            let storage = storage.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let candidate = record();
                barrier.wait();
                storage.reserve(&candidate)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let outcomes = contenders
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == Ok(Reservation::Published))
            .count(),
        1
    );
    assert!(outcomes.iter().all(|outcome| matches!(
        outcome,
        Ok(Reservation::Published) | Err(Error::Busy | Error::Full)
    )));
    let loaded = storage.load().unwrap();
    assert_eq!(loaded.occupied, MAX_RECORDS);
    assert_eq!(loaded.records.len(), MAX_RECORDS);
    for original in originals {
        assert!(loaded.records.contains(&original));
    }
    assert_eq!(storage.reserve(&record()), Err(Error::Full));
}

#[test]
fn owned_lock_contention_is_nonblocking_and_never_changes_saved_records() {
    let directory = tempfile::tempdir().unwrap();
    let storage = journal(&directory);
    let original = record();
    storage.reserve(&original).unwrap();
    let (held, _directory) = storage.lock().unwrap();
    let another = journal(&directory);
    assert_eq!(another.reserve(&record()), Err(Error::Busy));
    assert!(matches!(another.load(), Err(Error::Busy)));
    assert_eq!(
        another.settle(&original, &receipt(&original)),
        Err(Error::Busy)
    );
    drop(held);
    assert_eq!(another.load().unwrap().records, vec![original]);
}

#[test]
fn malformed_unknown_fields_and_mismatched_filenames_remain_visible_and_block_publication() {
    for variant in 0..4 {
        let directory = tempfile::tempdir().unwrap();
        let storage = journal(&directory);
        let original = record();
        storage.reserve(&original).unwrap();
        let path = storage.record_path(original.operation());
        match variant {
            0 => fs::write(&path, b"not JSON").unwrap(),
            1 => {
                let mut value = serde_json::to_value(&original).unwrap();
                value["caller"] = serde_json::json!({"session":"not-user"});
                fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            2 => {
                let mut value = serde_json::to_value(&original).unwrap();
                value["mutation"]["home"]["extra"] = serde_json::json!(true);
                fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            _ => fs::rename(&path, storage.record_path(Uuid::new_v4())).unwrap(),
        }
        let entries = fs::read_dir(&storage.path)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != LOCK)
            .map(|entry| (entry.path(), fs::read(entry.path()).unwrap()))
            .collect::<Vec<_>>();
        let loaded = storage.load().unwrap();
        assert_eq!(loaded.occupied, 1);
        assert!(loaded.records.is_empty());
        assert_eq!(loaded.problems.len(), 1);
        assert_eq!(loaded.problems[0].kind, ProblemKind::InvalidRecord);
        assert_eq!(storage.reserve(&record()), Err(Error::Full));
        assert_eq!(
            storage.settle(&original, &receipt(&original)),
            if variant == 3 {
                Ok(())
            } else {
                Err(Error::Conflict)
            },
            "a known receipt may settle its absent filename, leaving every other file intact"
        );
        for (path, bytes) in entries {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }
}

#[test]
fn symlink_fifo_hardlink_permissions_and_oversize_are_refused_without_repair_or_deletion() {
    for variant in 0..5 {
        let directory = tempfile::tempdir().unwrap();
        let storage = journal(&directory);
        let original = record();
        storage.reserve(&original).unwrap();
        let path = storage.record_path(original.operation());
        match variant {
            0 => {
                let outside = directory.path().join("original");
                fs::rename(&path, &outside).unwrap();
                symlink(&outside, &path).unwrap();
            }
            1 => {
                fs::remove_file(&path).unwrap();
                let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
                // SAFETY: a private task-owned absent path and fixed mode.
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }
            2 => fs::hard_link(&path, directory.path().join("alias")).unwrap(),
            3 => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
            _ => OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(MAX_BYTES as u64 + 1)
                .unwrap(),
        }
        let before = fs::symlink_metadata(&path).unwrap();
        let loaded = storage.load().unwrap();
        assert!(loaded.records.is_empty());
        assert_eq!(loaded.problems.len(), 1);
        assert_eq!(storage.reserve(&record()), Err(Error::Full));
        assert_eq!(
            storage.settle(&original, &receipt(&original)),
            Err(Error::Conflict)
        );
        let after = fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            (before.dev(), before.ino(), before.mode(), before.len()),
            (after.dev(), after.ino(), after.mode(), after.len())
        );
    }
}

#[test]
fn unsafe_journal_directory_or_lock_is_refused_and_directory_overflow_is_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let outside = directory.path().join("other");
    fs::create_dir(&outside).unwrap();
    let path = directory.path().join("pending");
    symlink(&outside, &path).unwrap();
    let storage = Journal::at(path.clone()).unwrap();
    assert_eq!(storage.reserve(&record()), Err(Error::UnsafeStorage));
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    symlink(directory.path().join("missing"), path.join(LOCK)).unwrap();
    assert_eq!(storage.reserve(&record()), Err(Error::UnsafeStorage));
    fs::remove_file(path.join(LOCK)).unwrap();
    for index in 0..64 {
        fs::write(path.join(format!("unknown-{index}")), b"retained").unwrap();
    }
    let loaded = storage.load().unwrap();
    assert_eq!(loaded.occupied, MAX_RECORDS + 1);
    assert_eq!(loaded.problems.len(), MAX_RECORDS + 1);
    assert_eq!(loaded.problems.last().unwrap().kind, ProblemKind::Overflow);
    assert_eq!(storage.reserve(&record()), Err(Error::Full));
    assert_eq!(
        fs::read_dir(&path).unwrap().count(),
        65,
        "no unknown entry was evicted"
    );
}

#[test]
fn directory_sync_failure_preserves_published_request_and_reports_settlement_uncertainty() {
    let directory = tempfile::tempdir().unwrap();
    let mut storage = journal(&directory);
    let original = record();
    // A test-only IO fault after the real file write/sync/publication. This is
    // not an executed crash/power-loss test or a claim about filesystem media.
    storage.fail_directory_sync = true;
    assert_eq!(storage.reserve(&original), Err(Error::PublicationUncertain));
    assert_eq!(storage.load().unwrap().records, vec![original.clone()]);
    let before = fs::read(storage.record_path(original.operation())).unwrap();
    assert_eq!(storage.reserve(&original), Err(Error::PublicationUncertain));
    assert_eq!(
        fs::read(storage.record_path(original.operation())).unwrap(),
        before
    );
    storage.fail_directory_sync = false;
    assert_eq!(storage.reserve(&original), Ok(Reservation::AlreadyPresent));
    storage.fail_directory_sync = true;
    assert_eq!(
        storage.settle(&original, &receipt(&original)),
        Err(Error::SettlementUncertain)
    );
    assert!(
        !storage.record_path(original.operation()).exists(),
        "removal visible does not imply synchronized cleanup"
    );
}

#[test]
fn restrictive_umask_child_still_publishes_private_reopenable_request_and_lock() {
    const CHILD: &str = "CAGENTS_TEST_CHANNEL_JOURNAL_UMASK_CHILD";
    if std::env::var_os(CHILD).is_some() {
        // This process was invoked with this one exact libtest parent. Never
        // change the umask in a shared parallel test process.
        // SAFETY: umask takes a scalar mask; this isolated process exits next.
        unsafe {
            libc::umask(0o777);
        }
        let directory = tempfile::tempdir().unwrap();
        // tempfile's directory is also subject to umask. Fix only our fixture.
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let storage = journal(&directory);
        let original = record();
        assert_eq!(storage.reserve(&original), Ok(Reservation::Published));
        let path = storage.record_path(original.operation());
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(storage.path.join(LOCK)).unwrap().mode() & 0o777,
            0o600
        );
        drop(storage);
        assert_eq!(journal(&directory).load().unwrap().records, vec![original]);
        return;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "channel_work_ui::journal::tests::restrictive_umask_child_still_publishes_private_reopenable_request_and_lock", "--test-threads=1"])
        .env(CHILD, "1")
        .status().unwrap();
    assert!(
        status.success(),
        "isolated restrictive-umask journal contract"
    );
}

#[test]
fn publication_verification_refuses_retained_staging_link_and_changed_destination() {
    let directory = tempfile::tempdir().unwrap();
    let storage = journal(&directory);
    let original = record();
    storage.reserve(&original).unwrap();
    let path = storage.record_path(original.operation());
    let file = OpenOptions::new().read(true).open(&path).unwrap();
    verify_publication(&file, &path).unwrap();
    let interrupted = directory.path().join("interrupted-staging-name");
    fs::hard_link(&path, &interrupted).unwrap();
    assert_eq!(
        verify_publication(&file, &path),
        Err(Error::PublicationUncertain)
    );
    assert_eq!(storage.reserve(&original), Err(Error::Conflict));
    assert!(
        interrupted.exists() && path.exists(),
        "unknown retained link must not be cleaned up"
    );
    fs::remove_file(&interrupted).unwrap();
    fs::rename(&path, &interrupted).unwrap();
    fs::write(&path, b"unknown replacement").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        verify_publication(&file, &path),
        Err(Error::PublicationUncertain)
    );
    assert_eq!(fs::read(path).unwrap(), b"unknown replacement");
    assert_eq!(
        serde_json::from_slice::<Record>(&fs::read(interrupted).unwrap()).unwrap(),
        original
    );
}

#[test]
fn recordless_positive_settlement_proves_only_absence_and_preserves_every_existing_entry() {
    let private = tempfile::tempdir().unwrap();
    let mut storage = Journal::at(private.path().join("pending")).unwrap();
    let original = selected_record();
    let receipt = Receipt {
        home: original.mutation.home,
        operation: original.operation(),
        revision: original.mutation.expected_revision + 1,
        sequence: 1,
        changed: true,
    };
    let mut wrong = receipt.clone();
    wrong.revision -= 1;
    assert_eq!(
        storage.settle_absent(original.mutation(), &wrong),
        Err(Error::Conflict)
    );
    assert!(
        !storage.path.exists(),
        "invalid positive metadata does not initialize storage"
    );
    assert!(storage.settle_absent(original.mutation(), &receipt).is_ok());
    storage.fail_directory_sync = true;
    assert_eq!(
        storage.settle_absent(original.mutation(), &receipt),
        Err(Error::SettlementUncertain)
    );
    storage.fail_directory_sync = false;
    storage.reserve(&original).unwrap();
    let path = storage.record_path(original.operation());
    let exact = fs::read(&path).unwrap();
    assert_eq!(
        storage.settle_absent(original.mutation(), &receipt),
        Err(Error::Conflict)
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        exact,
        "even the exact existing record is not removed by missing-only reconciliation"
    );
    for bytes in [b"malformed".as_slice(), b"{\"version\":9000}"] {
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            storage.settle_absent(original.mutation(), &receipt),
            Err(Error::Conflict)
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    fs::remove_file(&path).unwrap();
    let foreign = private.path().join("foreign");
    fs::write(&foreign, b"preserved").unwrap();
    symlink(&foreign, &path).unwrap();
    assert_eq!(
        storage.settle_absent(original.mutation(), &receipt),
        Err(Error::Conflict)
    );
    assert!(
        fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&foreign).unwrap(), b"preserved");
}
