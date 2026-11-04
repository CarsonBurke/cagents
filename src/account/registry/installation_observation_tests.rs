//! Private filesystem fixtures for the observation-only installation read.
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

use super::*;

#[derive(Debug, PartialEq, Eq)]
enum Contents {
    Directory,
    File(Vec<u8>),
    Unreadable(io::ErrorKind),
    Symlink(PathBuf),
    Other,
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    relative: PathBuf,
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
    links: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    contents: Contents,
}

fn snapshot(root: &Path) -> Vec<Entry> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<Entry>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        let contents = if metadata.file_type().is_symlink() {
            Contents::Symlink(fs::read_link(path).unwrap())
        } else if metadata.is_dir() {
            Contents::Directory
        } else if metadata.is_file() {
            match fs::read(path) {
                Ok(bytes) => Contents::File(bytes),
                Err(error) => Contents::Unreadable(error.kind()),
            }
        } else {
            Contents::Other
        };
        entries.push(Entry {
            relative: path.strip_prefix(root).unwrap().to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            uid: metadata.uid(),
            links: metadata.nlink(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            contents,
        });
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
    let mut entries = Vec::new();
    // Reading can update access times. Namespace, contents, ownership,
    // permissions, and modification/change metadata must remain identical.
    visit(root, root, &mut entries);
    entries
}

fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn fixture() -> (tempfile::TempDir, Store) {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("config");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    (root, Store::at(directory))
}

fn registry(installation: Option<Uuid>) -> Registry {
    Registry {
        version: VERSION,
        revision: 19,
        installation,
        ..Registry::default()
    }
}

fn write_registry(store: &Store, registry: &Registry) {
    write_private(&store.path(), toml::to_string_pretty(registry).unwrap());
}

fn assert_unchanged<T>(root: &Path, inspect: impl FnOnce() -> T) -> T {
    let before = snapshot(root);
    let result = inspect();
    assert_eq!(snapshot(root), before, "inspection must not mutate storage");
    result
}

fn assert_sanitized(error: &io::Error, root: &Path) {
    let message = error.to_string();
    assert!(!message.contains(&root.to_string_lossy().to_string()));
    assert!(!message.contains("private-fixture-marker"));
    assert!(!message.contains("line "));
}

#[test]
fn missing_registry_or_namespace_is_not_created() {
    let (root, store) = fixture();
    for directory in [
        store.directory.clone(),
        root.path().join("absent"),
        root.path().join("absent/nested/config"),
    ] {
        let store = Store::at(directory);
        assert_eq!(
            assert_unchanged(root.path(), || store.inspect_installation_id()).unwrap(),
            None
        );
    }
    assert!(!store.directory.join(LOCK_NAME).exists());
    assert!(!store.path().exists());
}

#[test]
fn existing_identity_is_observed_without_mutating_storage() {
    let (root, store) = fixture();
    let identity = Uuid::from_u128(0x1954);
    write_registry(&store, &registry(Some(identity)));
    // Directory permissions use the existing ownership rule; observation
    // must not tighten them as Store::load would.
    fs::set_permissions(&store.directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        assert_unchanged(root.path(), || store.inspect_installation_id()).unwrap(),
        Some(identity)
    );
    assert!(!store.directory.join(LOCK_NAME).exists());
}

#[test]
fn legacy_registry_without_identity_is_not_adopted() {
    let (root, store) = fixture();
    let legacy = Registry {
        version: VERSION - 1,
        revision: 3,
        ..Registry::default()
    };
    write_registry(&store, &legacy);
    assert_eq!(
        assert_unchanged(root.path(), || store.inspect_installation_id()).unwrap(),
        None
    );
}

#[test]
fn corrupt_input_is_sanitized_and_preserved() {
    let (root, store) = fixture();
    for bytes in [
        b"private-fixture-marker = [unterminated".as_slice(),
        b"version = \"private-fixture-marker\"".as_slice(),
        b"\xffprivate-fixture-marker".as_slice(),
    ] {
        write_private(&store.path(), bytes);
        let error = assert_unchanged(root.path(), || store.inspect_installation_id()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_sanitized(&error, root.path());
    }
}

#[test]
fn invalid_preparation_metadata_is_sanitized_and_preserved() {
    let (root, store) = fixture();
    let identity = Uuid::from_u128(0x1954);
    let mut inconsistent = registry(Some(identity));
    inconsistent.preparations.push(Preparation {
        binding: PreparationBinding {
            request_id: Uuid::from_u128(1),
            installation_id: Uuid::from_u128(2),
            key: Uuid::from_u128(3),
            payload: PreparationPayload {
                provider: Provider::Claude,
                copy_codex_config: false,
            },
        },
        state: PreparationState::Planned,
    });
    for invalid in [registry(Some(Uuid::nil())), inconsistent] {
        write_registry(&store, &invalid);
        let error = assert_unchanged(root.path(), || store.inspect_installation_id()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_sanitized(&error, root.path());
    }
}

#[test]
fn newer_registry_is_refused_without_downgrading() {
    let (root, store) = fixture();
    let mut newer = registry(Some(Uuid::from_u128(0x1954)));
    newer.version = VERSION + 1;
    write_registry(&store, &newer);
    let error = assert_unchanged(root.path(), || store.inspect_installation_id()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert_sanitized(&error, root.path());
}

#[test]
fn oversized_registry_is_refused_and_preserved() {
    let (root, store) = fixture();
    let mut bytes = toml::to_string_pretty(&registry(Some(Uuid::from_u128(0x1954))))
        .unwrap()
        .into_bytes();
    bytes.extend_from_slice(b"# private-fixture-marker");
    bytes.resize(MAX_BYTES as usize + 1, b'x');
    write_private(&store.path(), &bytes);
    let error = assert_unchanged(root.path(), || store.inspect_installation_id()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_sanitized(&error, root.path());
}

#[test]
fn insecure_file_is_refused_without_permission_repair() {
    let (root, store) = fixture();
    write_registry(&store, &registry(Some(Uuid::from_u128(0x1954))));
    fs::set_permissions(store.path(), fs::Permissions::from_mode(0o644)).unwrap();
    let error = assert_unchanged(root.path(), || store.inspect_installation_id()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_sanitized(&error, root.path());
}

#[test]
fn multiply_linked_file_is_refused_without_changing_either_name() {
    let (root, store) = fixture();
    write_registry(&store, &registry(Some(Uuid::from_u128(0x1954))));
    fs::hard_link(store.path(), root.path().join("other-private-name")).unwrap();
    let error = assert_unchanged(root.path(), || store.inspect_installation_id()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_sanitized(&error, root.path());
}

#[test]
fn final_symlinks_are_errors_even_when_the_target_is_missing() {
    let (root, store) = fixture();
    let target = root.path().join("target");
    write_private(&target, b"private-fixture-marker");
    for target in [target, root.path().join("absent-target")] {
        symlink(target, store.path()).unwrap();
        let error = assert_unchanged(root.path(), || store.inspect_installation_id()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_sanitized(&error, root.path());
        fs::remove_file(store.path()).unwrap();
    }
}

#[test]
fn ancestor_symlinks_are_errors_even_when_the_target_is_missing() {
    let (root, store) = fixture();
    write_registry(&store, &registry(Some(Uuid::from_u128(0x1954))));
    for (name, target) in [
        ("alias", store.directory.clone()),
        ("dangling", root.path().join("absent-target")),
    ] {
        let alias = root.path().join(name);
        symlink(target, &alias).unwrap();
        let alias_store = Store::at(alias.join("nested"));
        let error =
            assert_unchanged(root.path(), || alias_store.inspect_installation_id()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_sanitized(&error, root.path());
    }
}

#[test]
fn non_directory_namespace_and_nonregular_registry_are_errors() {
    let (root, store) = fixture();
    fs::create_dir(store.path()).unwrap();
    let error = assert_unchanged(root.path(), || store.inspect_installation_id()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_sanitized(&error, root.path());

    let not_directory = root.path().join("private-fixture-marker");
    write_private(&not_directory, b"private-fixture-marker");
    let invalid_store = Store::at(not_directory);
    let error =
        assert_unchanged(root.path(), || invalid_store.inspect_installation_id()).unwrap_err();
    assert_sanitized(&error, root.path());
}

#[test]
fn inaccessible_registry_is_an_error_without_chmod_recovery() {
    // Root can read mode 000 regardless of the file's access permissions.
    // The other refusal cases remain meaningful under that test identity.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let (root, store) = fixture();
    let original = toml::to_string_pretty(&registry(Some(Uuid::from_u128(0x1954)))).unwrap();
    write_private(&store.path(), &original);
    fs::set_permissions(store.path(), fs::Permissions::from_mode(0o000)).unwrap();
    let error = assert_unchanged(root.path(), || store.inspect_installation_id()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_sanitized(&error, root.path());
    fs::set_permissions(store.path(), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fs::read_to_string(store.path()).unwrap(), original);
}
