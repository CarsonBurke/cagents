use super::*;
use crate::account::import::{CodingProfile, CopyConsent, PhysicalIdentity, SourceObservation};
use std::os::unix::fs::symlink;

fn fixture() -> (tempfile::TempDir, Journal) {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let journal = Journal::at(directory.path().join("imports"));
    (directory, journal)
}

fn plan(operation: u128) -> ImportPlan {
    ImportPlan {
        operation: Uuid::from_u128(operation),
        destination: crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("fixture-host"),
            boot_id: crate::model::BootId::new("fixture-boot"),
            host_incarnation: crate::model::HostIncarnation::new("fixture-incarnation"),
        },
        observation: SourceObservation {
            path: "/private/staged/auth.json".into(),
            parent: PhysicalIdentity {
                device: 1,
                inode: 10,
            },
            file: PhysicalIdentity {
                device: 1,
                inode: 11,
            },
            size: 128,
            mtime_seconds: 1,
            mtime_nanoseconds: 2,
            ctime_seconds: 3,
            ctime_nanoseconds: 4,
        },
        expected_email: "fixture@example.test".into(),
        consent: CopyConsent {
            staged_file_only: true,
            source_session_limit_acknowledged: true,
        },
        profile: CodingProfile::Unconfigured,
    }
}

fn receipt(plan: &ImportPlan) -> PublicationReceipt {
    PublicationReceipt {
        operation: plan.operation,
        home: PhysicalIdentity {
            device: 2,
            inode: 20,
        },
        file: PhysicalIdentity {
            device: 2,
            inode: 21,
        },
        size: plan.observation.size(),
    }
}

fn bytes(journal: &Journal, operation: Uuid) -> Vec<u8> {
    fs::read(journal.path.join(name(operation))).unwrap()
}

fn private_write(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
}

#[test]
fn intent_and_actual_receipt_survive_reopen_without_granting_or_repeating_external_stages() {
    let (_directory, journal) = fixture();
    let original = plan(1);
    let reserved = journal.begin(&original).unwrap();
    assert_eq!(reserved.phase(), Phase::Reserved);
    assert_eq!(reserved.receipt(), None);
    assert!(!reserved.uncertain());
    assert_eq!(journal.mark_published(&original), Err(Error::Conflict));
    assert_eq!(journal.mark_registered(&original), Err(Error::Conflict));
    let prepared = journal
        .record_prepared(&original, &receipt(&original))
        .unwrap();
    assert_eq!(prepared.phase(), Phase::Prepared);
    assert_eq!(prepared.plan(), &original);
    let encoded = bytes(&journal, original.operation);
    let metadata = fs::metadata(journal.path.join(name(original.operation))).unwrap();
    assert_eq!(metadata.mode() & 0o7777, 0o600);
    assert_eq!(metadata.nlink(), 1);
    assert_eq!(fs::metadata(&journal.path).unwrap().mode() & 0o7777, 0o700);
    let reopened = Journal::at(journal.path.clone());
    assert_eq!(
        reopened.status(original.operation).unwrap(),
        Some(prepared.clone())
    );
    assert_eq!(reopened.load().unwrap(), vec![prepared.clone()]);
    assert_eq!(reopened.begin(&original).unwrap(), prepared);
    assert_eq!(bytes(&journal, original.operation), encoded);
    assert_eq!(
        fs::metadata(journal.path.join(name(original.operation)))
            .unwrap()
            .ino(),
        metadata.ino()
    );
    let published = reopened.mark_published(&original).unwrap();
    assert_eq!(published.phase(), Phase::Published);
    let uncertain = reopened.mark_uncertain(&original).unwrap();
    assert!(uncertain.uncertain());
    assert_eq!(uncertain.phase(), Phase::Published);
    let registered = reopened.mark_registered(&original).unwrap();
    assert_eq!(registered.phase(), Phase::Registered);
    assert!(
        registered.uncertain(),
        "a later stage does not erase uncertainty evidence"
    );
    assert_eq!(reopened.mark_published(&original), Err(Error::Conflict));
    assert_eq!(
        reopened
            .record_prepared(&original, &receipt(&original))
            .unwrap(),
        registered
    );
}

#[test]
fn immutable_plan_and_prepared_inode_collisions_preserve_the_original_bytes() {
    let (_directory, journal) = fixture();
    let original = plan(1);
    journal.begin(&original).unwrap();
    journal
        .record_prepared(&original, &receipt(&original))
        .unwrap();
    let before = bytes(&journal, original.operation);
    for mode in 0..8 {
        let mut changed = original.clone();
        match mode {
            0 => changed.observation.path = "/private/other/auth.json".into(),
            1 => changed.expected_email = "other@example.test".into(),
            2 => changed.observation.file.inode += 1,
            3 => changed.observation.ctime_seconds += 1,
            4 => {
                changed.profile = CodingProfile::WorkspaceWrite {
                    network_access: false,
                }
            }
            5 => changed.observation.parent.inode += 1,
            6 => changed.observation.size += 1,
            _ => {
                changed.destination.host_incarnation =
                    crate::model::HostIncarnation::new("restarted-host")
            }
        }
        assert_eq!(journal.begin(&changed), Err(Error::Conflict), "case {mode}");
        assert_eq!(journal.mark_uncertain(&changed), Err(Error::Conflict));
        assert_eq!(bytes(&journal, original.operation), before);
    }
    let mut other_inode = receipt(&original);
    other_inode.file.inode += 1;
    assert_eq!(
        journal.record_prepared(&original, &other_inode),
        Err(Error::Conflict)
    );
    let mut other_home = receipt(&original);
    other_home.home.inode += 1;
    assert_eq!(
        journal.record_prepared(&original, &other_home),
        Err(Error::Conflict)
    );
    let mut wrong_operation = receipt(&original);
    wrong_operation.operation = Uuid::new_v4();
    assert_eq!(
        journal.record_prepared(&original, &wrong_operation),
        Err(Error::Invalid)
    );
    assert_eq!(bytes(&journal, original.operation), before);
}

#[test]
fn read_only_status_never_creates_repairs_or_adopts_corrupt_newer_and_unknown_evidence() {
    let (_directory, journal) = fixture();
    let original = plan(1);
    assert_eq!(journal.status(original.operation).unwrap(), None);
    assert!(journal.load().unwrap().is_empty());
    assert!(!journal.path.exists());
    journal.begin(&original).unwrap();
    let before = bytes(&journal, original.operation);
    let lock_path = journal.path.join(LOCK);
    fs::remove_file(&lock_path).unwrap();
    let meta = fs::metadata(&journal.path).unwrap();
    assert_eq!(
        journal.status(original.operation).unwrap().unwrap().plan(),
        &original
    );
    assert!(!lock_path.exists());
    assert_eq!(
        fs::metadata(&journal.path).unwrap().mtime_nsec(),
        meta.mtime_nsec()
    );
    let unknown = journal.path.join("preserve-unknown-evidence");
    private_write(&unknown, b"unknown");
    assert_eq!(journal.load(), Err(Error::Corrupt));
    assert_eq!(journal.begin(&plan(2)), Err(Error::Corrupt));
    assert_eq!(fs::read(&unknown).unwrap(), b"unknown");
    fs::remove_file(&unknown).unwrap();
    let record_path = journal.path.join(name(original.operation));
    let mut newer: serde_json::Value = serde_json::from_slice(&before).unwrap();
    newer["version"] = (VERSION + 1).into();
    newer["future_field"] = true.into();
    fs::write(&record_path, serde_json::to_vec(&newer).unwrap()).unwrap();
    let future = fs::read(&record_path).unwrap();
    assert_eq!(journal.status(original.operation), Err(Error::Newer));
    assert_eq!(journal.begin(&plan(2)), Err(Error::Newer));
    assert_eq!(fs::read(&record_path).unwrap(), future);
    let mut unknown_nested: serde_json::Value = serde_json::from_slice(&before).unwrap();
    unknown_nested["plan"]["destination"]["future_proof"] = true.into();
    let unknown_nested = serde_json::to_vec(&unknown_nested).unwrap();
    fs::write(&record_path, &unknown_nested).unwrap();
    assert_eq!(journal.status(original.operation), Err(Error::Corrupt));
    assert_eq!(journal.mark_uncertain(&original), Err(Error::Corrupt));
    assert_eq!(fs::read(&record_path).unwrap(), unknown_nested);
    fs::write(&record_path, b"{\"version\":1,\"secret\":\"not-a-record\"}").unwrap();
    let malformed = fs::read(&record_path).unwrap();
    assert_eq!(journal.status(original.operation), Err(Error::Corrupt));
    assert_eq!(journal.begin(&plan(2)), Err(Error::Corrupt));
    assert_eq!(fs::read(&record_path).unwrap(), malformed);
    fs::set_permissions(&journal.path, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        journal.status(original.operation),
        Err(Error::UnsafeStorage)
    );
    assert_eq!(journal.begin(&plan(2)), Err(Error::UnsafeStorage));
    assert_eq!(fs::metadata(&journal.path).unwrap().mode() & 0o7777, 0o755);
}

#[test]
fn record_capacity_and_size_limits_refuse_without_evicting_prior_operations() {
    let (_directory, journal) = fixture();
    for operation in 1..=MAX_RECORDS as u128 {
        journal.begin(&plan(operation)).unwrap();
    }
    let first = bytes(&journal, Uuid::from_u128(1));
    assert_eq!(journal.load().unwrap().len(), MAX_RECORDS);
    assert_eq!(
        journal.begin(&plan(MAX_RECORDS as u128 + 1)),
        Err(Error::Full)
    );
    assert_eq!(bytes(&journal, Uuid::from_u128(1)), first);
    assert_eq!(journal.begin(&plan(1)).unwrap().phase(), Phase::Reserved);
    let path = journal.path.join(name(Uuid::from_u128(1)));
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(MAX_RECORD_BYTES + 1)
        .unwrap();
    assert_eq!(journal.status(Uuid::from_u128(1)), Err(Error::Corrupt));
    assert_eq!(journal.begin(&plan(1)), Err(Error::Corrupt));
    assert_eq!(fs::metadata(&path).unwrap().len(), MAX_RECORD_BYTES + 1);
}

#[test]
fn unsafe_links_nonregular_entries_and_foreign_replacements_are_not_read_or_overwritten() {
    let (directory, journal) = fixture();
    let original = plan(1);
    journal.begin(&original).unwrap();
    let path = journal.path.join(name(original.operation));
    let source = bytes(&journal, original.operation);
    let outside = directory.path().join("outside");
    private_write(&outside, &source);
    fs::remove_file(&path).unwrap();
    symlink(&outside, &path).unwrap();
    assert_eq!(
        journal.status(original.operation),
        Err(Error::UnsafeStorage)
    );
    assert_eq!(fs::read(&outside).unwrap(), source);
    fs::remove_file(&path).unwrap();
    fs::hard_link(&outside, &path).unwrap();
    assert_eq!(
        journal.status(original.operation),
        Err(Error::UnsafeStorage)
    );
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(
        journal.status(original.operation),
        Err(Error::UnsafeStorage)
    );
    fs::remove_dir(&path).unwrap();
    let fifo = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert_eq!(
        journal.status(original.operation),
        Err(Error::UnsafeStorage)
    );
    fs::remove_file(&path).unwrap();
    private_write(&path, &source);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        journal.status(original.operation),
        Err(Error::UnsafeStorage)
    );
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o644);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let root = journal.directory(false).unwrap().unwrap();
    let (record, before) = read_record(&root, original.operation).unwrap().unwrap();
    let replacement = journal.path.join("replacement");
    private_write(&replacement, &source);
    fs::rename(&replacement, &path).unwrap();
    let replacement_inode = fs::metadata(&path).unwrap().ino();
    assert_eq!(persist(&root, &record, Some(&before)), Err(Error::Conflict));
    assert_eq!(fs::metadata(&path).unwrap().ino(), replacement_inode);
    assert_eq!(fs::read(&path).unwrap(), source);
}

#[test]
fn post_publication_sync_failure_keeps_the_original_request_and_actual_receipt_observable() {
    let (_directory, journal) = fixture();
    let original = plan(1);
    journal.begin(&original).unwrap();
    let root = journal.directory(false).unwrap().unwrap();
    let _guard = lock(&root).unwrap();
    let other = plan(2);
    let reserved = Record {
        version: VERSION,
        plan: other.clone(),
        receipt: None,
        phase: Phase::Reserved,
        uncertain: false,
    };
    assert_eq!(
        persist_with_sync(&root, &reserved, None, |_| Err(io::Error::other(
            "fixture initial dir sync"
        ))),
        Err(Error::PublicationUncertain)
    );
    assert_eq!(journal.status(other.operation).unwrap(), Some(reserved));
    let (mut record, before) = read_record(&root, original.operation).unwrap().unwrap();
    record.receipt = Some(receipt(&original));
    record.phase = Phase::Prepared;
    assert_eq!(
        persist_with_sync(&root, &record, Some(&before), |_| Err(io::Error::other(
            "fixture sync failure"
        ))),
        Err(Error::PublicationUncertain)
    );
    // This is an observation, not replay permission or a fabricated home owner.
    let reopened = Journal::at(journal.path.clone());
    assert_eq!(
        reopened.status(original.operation).unwrap(),
        Some(record.clone())
    );
    assert_eq!(reopened.begin(&original), Err(Error::Busy));
    drop(_guard);
    assert_eq!(reopened.begin(&original).unwrap(), record);
    let uncertain = reopened.mark_uncertain(&original).unwrap();
    assert!(uncertain.uncertain());
    assert_eq!(uncertain.receipt(), Some(&receipt(&original)));
    assert_eq!(uncertain.plan(), &original);
    assert_eq!(reopened.load().unwrap().len(), 2);
}

#[test]
fn journal_lock_is_nonblocking_and_status_does_not_take_or_replace_it() {
    let (_directory, journal) = fixture();
    let original = plan(1);
    journal.begin(&original).unwrap();
    let root = journal.directory(false).unwrap().unwrap();
    let guard = lock(&root).unwrap();
    let lock_before = fs::metadata(journal.path.join(LOCK)).unwrap();
    assert_eq!(journal.begin(&plan(2)), Err(Error::Busy));
    assert_eq!(
        journal.record_prepared(&original, &receipt(&original)),
        Err(Error::Busy)
    );
    assert_eq!(
        journal.status(original.operation).unwrap().unwrap().plan(),
        &original
    );
    assert_eq!(
        fs::metadata(journal.path.join(LOCK)).unwrap().ino(),
        lock_before.ino()
    );
    drop(guard);
    journal
        .record_prepared(&original, &receipt(&original))
        .unwrap();
    assert_eq!(journal.load().unwrap().len(), 1);
}
