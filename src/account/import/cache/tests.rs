use super::*;
use crate::account::home::{CodexConfig, Homes};
use std::{
    ffi::CString,
    fs,
    os::unix::{ffi::OsStrExt, fs::symlink},
    time::{Duration, UNIX_EPOCH},
};
use uuid::Uuid;

const SENTINEL: &str = "synthetic-cache-secret-never-log-this";
fn payload() -> Vec<u8> {
    format!(
        "{{\n \"auth_mode\":\"chatgpt\",\"OPENAI_API_KEY\":null,\"tokens\":{{\"refresh_token\":\"{SENTINEL}\",\"access_token\":\"fake-access\",\"id_token\":\"fake-id\"}},\"future_metadata\":true\n}}\n"
    ).into_bytes()
}
fn staged(directory: &Path) -> PathBuf {
    fs::create_dir_all(directory).unwrap();
    let path = directory.join(AUTH_FILE);
    private_write(&path, &payload());
    path
}
fn private_write(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, Permissions::from_mode(0o600)).unwrap();
}
fn take_snapshot(path: &Path) -> Snapshot {
    snapshot(path, &inspect(path).unwrap()).unwrap()
}
fn prepare_new(base: &Path, snapshot: Snapshot) -> Prepared {
    let homes = Homes::at(base);
    let mut prepared = None;
    homes
        .create_planned_with_config_initialized(
            Provider::Codex,
            Uuid::new_v4(),
            None,
            CodexConfig::Shared,
            |created| {
                prepared = Some(prepare(snapshot, created).map_err(std::io::Error::other)?);
                Ok(())
            },
        )
        .unwrap();
    prepared.unwrap()
}
fn temporary() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn fresh_home_publication_preserves_exact_cache_and_only_exposes_metadata() {
    let directory = temporary();
    let source = staged(&directory.path().join("source"));
    fs::set_permissions(&source, Permissions::from_mode(0o400)).unwrap();
    let before = inspect(&source).unwrap();
    let snapshot = snapshot(&source, &before).unwrap();
    assert!(!format!("{snapshot:?}").contains(SENTINEL));
    let prepared = prepare_new(&directory.path().join("homes"), snapshot);
    // Prepared owns the original descriptors after the initializer returned.
    let root = prepared.root.try_clone().unwrap();
    let home = prepared.home.clone();
    let receipt = prepared.receipt();
    assert_eq!(
        publication_state(&root, &receipt).unwrap(),
        PublicationState::Missing
    );
    assert!(!format!("{prepared:?}").contains(SENTINEL));
    let metadata = serde_json::to_string(&(&before, &receipt)).unwrap();
    assert!(!metadata.contains(SENTINEL));
    assert!(!metadata.contains("refresh_token"));
    assert_eq!(publish(prepared).unwrap(), receipt);
    assert_eq!(fs::read(home.join(AUTH_FILE)).unwrap(), payload());
    let published = fs::metadata(home.join(AUTH_FILE)).unwrap();
    assert_eq!(published.permissions().mode() & 0o777, 0o600);
    assert_eq!(published.nlink(), 1);
    assert_eq!(identity(&published), receipt.file);
    assert_eq!(
        publication_state(&root, &receipt).unwrap(),
        PublicationState::Present
    );
    assert_eq!(fs::read_dir(&home).unwrap().count(), 1);
    assert_eq!(inspect(&source).unwrap(), before);
    assert_eq!(
        fs::metadata(&source).unwrap().permissions().mode() & 0o777,
        0o400
    );
    // Test correction of an actual mode000 inode without changing the
    // process-global umask of other tests.
    let homes = Homes::at(directory.path().join("initially-unreadable"));
    let mut restrictive = None;
    homes
        .create_planned_with_config_initialized(
            Provider::Codex,
            Uuid::new_v4(),
            None,
            CodexConfig::Shared,
            |created| {
                restrictive = Some(prepare_with_mode(take_snapshot(&source), created, 0).unwrap());
                Ok(())
            },
        )
        .unwrap();
    let restrictive = restrictive.unwrap();
    assert_eq!(
        restrictive.anonymous.metadata().unwrap().mode() & 0o7777,
        0o600
    );
    assert!(publish(restrictive).is_ok());
}

#[test]
fn only_documented_managed_chatgpt_cache_is_accepted_without_secret_diagnostics() {
    let directory = temporary();
    let source = staged(directory.path());
    let invalid = [
        format!("{{\"auth_mode\":\"apikey\",\"OPENAI_API_KEY\":\"{SENTINEL}\"}}"),
        format!("{{\"auth_mode\":\"chatgptAuthTokens\",\"tokens\":{{\"refresh_token\":\"{SENTINEL}\"}}}}"),
        format!("{{\"auth_mode\":\"chatgpt\",\"OPENAI_API_KEY\":\"{SENTINEL}\",\"tokens\":{{\"refresh_token\":\"fake\"}}}}"),
        "{\"tokens\":{\"refresh_token\":\"fake\"}}".into(),
        "{\"auth_mode\":\"chatgpt\",\"tokens\":{\"refresh_token\":\"  \"}}".into(),
        "{\"auth_mode\":\"chatgpt\",\"tokens\":{\"refresh_token\":42}}".into(),
        "{\"auth_mode\":\"chatgpt\",\"auth_mode\":\"chatgpt\",\"tokens\":{\"refresh_token\":\"fake\"}}".into(),
        "{\"auth_mode\":\"chatgpt\",\"tokens\":{\"refresh_token\":\"fake\",\"refresh_token\":\"other\"}}".into(),
        format!("{{\"{SENTINEL}\":broken"),
        format!("{} trailing-{SENTINEL}", String::from_utf8(payload()).unwrap()),
    ];
    for bytes in invalid {
        private_write(&source, bytes.as_bytes());
        let error = inspect(&source).unwrap_err();
        assert!(!format!("{error:?} {error}").contains(SENTINEL));
        assert_eq!(fs::read(&source).unwrap(), bytes.as_bytes());
    }
    // The official minimum is not a claim that a provider accepted the cache.
    private_write(
        &source,
        br#"{"auth_mode":"chatgpt","tokens":{"refresh_token":"fake"}}"#,
    );
    assert!(inspect(&source).is_ok());
}

#[test]
fn source_components_and_private_file_rules_refuse_unsafe_operands_without_repairs() {
    let directory = temporary();
    let source = staged(&directory.path().join("stage"));
    fs::set_permissions(&source, Permissions::from_mode(0o644)).unwrap();
    assert_eq!(inspect(&source).unwrap_err(), Error::UnsafeSource);
    assert_eq!(
        fs::metadata(&source).unwrap().permissions().mode() & 0o777,
        0o644
    );
    fs::set_permissions(&source, Permissions::from_mode(0o600)).unwrap();
    let linked = directory.path().join("linked");
    fs::hard_link(&source, &linked).unwrap();
    assert_eq!(inspect(&source).unwrap_err(), Error::UnsafeSource);
    fs::remove_file(&linked).unwrap();
    symlink(source.parent().unwrap(), &linked).unwrap();
    assert_eq!(
        inspect(&linked.join(AUTH_FILE)).unwrap_err(),
        Error::UnsafeSource
    );
    fs::remove_file(&linked).unwrap();
    symlink(&source, &linked).unwrap();
    assert_eq!(inspect(&linked).unwrap_err(), Error::UnsafeSource);
    fs::remove_file(&linked).unwrap();
    let name = CString::new(linked.as_os_str().as_bytes()).unwrap();
    // Nonblocking open and regular-file validation must refuse this FIFO.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert_eq!(inspect(&linked).unwrap_err(), Error::UnsafeSource);
    assert!(inspect(source.parent().unwrap()).is_err());
    assert_eq!(
        inspect(Path::new("relative/auth.json")).unwrap_err(),
        Error::InvalidPath
    );
    assert_eq!(fs::read(&source).unwrap(), payload());
}

#[test]
fn observations_and_bounded_reads_refuse_replacements_mutation_and_growth() {
    let directory = temporary();
    let source = staged(directory.path());
    let before = inspect(&source).unwrap();
    let replacement = directory.path().join("replacement");
    private_write(&replacement, &payload());
    fs::rename(&replacement, &source).unwrap();
    assert_eq!(
        snapshot(&source, &before).unwrap_err(),
        Error::SourceChanged
    );
    let before = inspect(&source).unwrap();
    assert_eq!(
        read_snapshot(&source, Some(&before), || {
            let file = fs::OpenOptions::new().write(true).open(&source).unwrap();
            file.set_modified(UNIX_EPOCH + Duration::from_secs(1))
                .unwrap();
        })
        .unwrap_err(),
        Error::SourceChanged
    );
    private_write(&source, &payload());
    let mut at_limit = payload();
    at_limit.resize(MAX_AUTH_BYTES, b' ');
    private_write(&source, &at_limit);
    assert_eq!(inspect(&source).unwrap().size, MAX_AUTH_BYTES as u64);
    at_limit.push(b' ');
    private_write(&source, &at_limit);
    assert_eq!(inspect(&source).unwrap_err(), Error::TooLarge);
    private_write(&source, &payload());
    let before = inspect(&source).unwrap();
    assert_eq!(
        read_snapshot(&source, Some(&before), || private_write(&source, &at_limit)).unwrap_err(),
        Error::TooLarge
    );
    let parent = directory.path().join("parent-case");
    let nested = staged(&parent);
    let before = inspect(&nested).unwrap();
    assert_eq!(
        read_snapshot(&nested, Some(&before), || {
            fs::rename(&parent, directory.path().join("retained-parent")).unwrap();
            staged(&parent);
        })
        .unwrap_err(),
        Error::SourceChanged
    );
}

#[test]
fn existing_destination_entries_win_and_unpublished_preparations_leave_no_cache() {
    let directory = temporary();
    let source = staged(&directory.path().join("source"));
    let dropped = prepare_new(&directory.path().join("unused"), take_snapshot(&source));
    let unused_home = dropped.home.clone();
    drop(dropped);
    assert_eq!(fs::read_dir(unused_home).unwrap().count(), 0);
    for entry in ["regular", "symlink", "directory", "fifo"] {
        let prepared = prepare_new(&directory.path().join(entry), take_snapshot(&source));
        let target = prepared.home.join(AUTH_FILE);
        match entry {
            "regular" => private_write(&target, b"existing bytes must win"),
            "symlink" => symlink(&source, &target).unwrap(),
            "directory" => fs::create_dir(&target).unwrap(),
            "fifo" => {
                let name = CString::new(target.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            _ => unreachable!(),
        }
        let before = fs::symlink_metadata(&target).unwrap();
        assert_eq!(publish(prepared).unwrap_err(), PublishFailure::Uncertain);
        let after = fs::symlink_metadata(&target).unwrap();
        assert_eq!(identity(&after), identity(&before));
        assert_eq!(after.file_type(), before.file_type());
        if entry == "regular" {
            assert_eq!(fs::read(&target).unwrap(), b"existing bytes must win");
        }
    }
    assert_eq!(fs::read(&source).unwrap(), payload());
}

#[test]
fn publication_result_loss_retains_exact_original_inode_and_never_reseeds() {
    let directory = temporary();
    let source = staged(&directory.path().join("source"));
    let prepared = prepare_new(&directory.path().join("homes"), take_snapshot(&source));
    let root = prepared.root.try_clone().unwrap();
    let receipt = prepared.receipt();
    assert_eq!(
        publish_with(prepared, |_| Err(std::io::Error::other(SENTINEL))).unwrap_err(),
        PublishFailure::Uncertain
    );
    assert_eq!(
        publication_state(&root, &receipt).unwrap(),
        PublicationState::Present
    );
    assert_eq!(fs::read(root.path().join(AUTH_FILE)).unwrap(), payload());
    let mut wrong = receipt.clone();
    wrong.file.inode += 1;
    assert_eq!(
        publication_state(&root, &wrong).unwrap(),
        PublicationState::Different
    );
    let mut wrong_operation = receipt.clone();
    wrong_operation.operation = Uuid::new_v4();
    assert_eq!(
        publication_state(&root, &wrong_operation).unwrap(),
        PublicationState::Different
    );
    let other = prepare_new(&directory.path().join("other"), take_snapshot(&source));
    assert_eq!(
        publication_state(&other.root, &receipt).unwrap(),
        PublicationState::Different
    );
    drop(other);
    // Status never writes, and an inode replacement cannot be adopted by it.
    fs::rename(root.path().join(AUTH_FILE), root.path().join("original")).unwrap();
    private_write(&root.path().join(AUTH_FILE), b"refreshed-or-foreign-cache");
    assert_eq!(
        publication_state(&root, &receipt).unwrap(),
        PublicationState::Different
    );
    assert_eq!(
        fs::read(root.path().join(AUTH_FILE)).unwrap(),
        b"refreshed-or-foreign-cache"
    );
    assert_eq!(fs::read(root.path().join("original")).unwrap(), payload());
    for replace in [false, true] {
        let prepared = prepare_new(
            &directory.path().join(if replace {
                "rename-replace"
            } else {
                "rename-away"
            }),
            take_snapshot(&source),
        );
        let root = prepared.root.try_clone().unwrap();
        let receipt = publish(prepared).unwrap();
        assert_eq!(
            publication_state_with(&root, &receipt, || {
                fs::rename(root.path().join(AUTH_FILE), root.path().join("retained")).unwrap();
                if replace {
                    private_write(&root.path().join(AUTH_FILE), &payload());
                }
            })
            .unwrap(),
            PublicationState::Different
        );
        assert_eq!(fs::read(root.path().join("retained")).unwrap(), payload());
        if replace {
            assert_eq!(fs::read(root.path().join(AUTH_FILE)).unwrap(), payload());
        }
    }
}

#[test]
fn new_home_provenance_and_current_physical_directory_are_required() {
    let directory = temporary();
    let source = staged(&directory.path().join("source"));
    let prepared = prepare_new(&directory.path().join("homes"), take_snapshot(&source));
    let home = prepared.home.clone();
    let root = prepared.root.try_clone().unwrap();
    let receipt = prepared.receipt();
    let retained = home.with_extension("retained");
    fs::rename(&home, &retained).unwrap();
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        publication_state(&root, &receipt).unwrap(),
        PublicationState::Different
    );
    assert_eq!(
        publish(prepared).unwrap_err(),
        PublishFailure::NotPublished(Error::UnsafeDestination)
    );
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&retained).unwrap().count(), 0);
    let homes = Homes::at(directory.path().join("other-provider"));
    let mut refused = None;
    homes
        .create_planned_with_config_initialized(
            Provider::Omp,
            Uuid::new_v4(),
            None,
            CodexConfig::Shared,
            |created| {
                refused = Some(prepare(take_snapshot(&source), created).unwrap_err());
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(refused, Some(Error::UnsafeDestination));
    let modified = prepare_new(&directory.path().join("modified"), take_snapshot(&source));
    let home = modified.home.clone();
    modified.anonymous.set_len(1).unwrap();
    assert_eq!(
        publish(modified).unwrap_err(),
        PublishFailure::NotPublished(Error::PrepareFailed)
    );
    assert_eq!(fs::read_dir(home).unwrap().count(), 0);
}
