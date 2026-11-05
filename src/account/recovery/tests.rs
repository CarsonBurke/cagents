use std::{
    fs, io,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
};

use uuid::Uuid;

use super::{Candidate, HomeStatus, discover, inspect};
use crate::{
    account::{
        home::Homes,
        registry::{Registered, Registry, Store},
    },
    agent::Provider,
};

fn metadata_identity(path: &Path) -> (u64, u64, u32, u32) {
    let metadata = fs::symlink_metadata(path).unwrap();
    (
        metadata.dev(),
        metadata.ino(),
        metadata.mode(),
        metadata.uid(),
    )
}

fn make_home(homes: &Homes, provider: Provider, key: Uuid) -> PathBuf {
    let path = homes.path(provider, key);
    fs::create_dir_all(&path).unwrap();
    path
}

fn registration(provider: Provider, key: Uuid) -> Registered {
    Registered {
        provider,
        key,
        linked_home: None,
        label: None,
        email: Some(format!("{}-{key}@example.invalid", provider.executable())),
        organization: None,
        added_at: 1,
    }
}

#[test]
fn a_missing_namespace_is_not_created_or_privatized() {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o751)).unwrap();
    let sentinel = directory.path().join("history.fixture");
    fs::write(&sentinel, b"synthetic conversation\n").unwrap();
    fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o640)).unwrap();
    let parent_before = metadata_identity(directory.path());
    let sentinel_before = metadata_identity(&sentinel);
    let root = directory.path().join("absent-accounts");
    let homes = Homes::at(&root);
    let registry = Registry::default();

    assert!(discover(&homes, &registry).unwrap().is_empty());
    assert_eq!(
        inspect(&homes, &registry, Provider::Claude, Uuid::from_u128(1)).unwrap(),
        HomeStatus::Missing
    );

    assert_eq!(
        fs::symlink_metadata(&root).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(metadata_identity(directory.path()), parent_before);
    assert_eq!(metadata_identity(&sentinel), sentinel_before);
    assert_eq!(fs::read(&sentinel).unwrap(), b"synthetic conversation\n");
}

#[test]
fn canonical_owned_homes_remain_hints_with_their_original_files_and_modes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("accounts");
    let homes = Homes::at(&root);
    let claude = Uuid::from_u128(2);
    let codex = Uuid::from_u128(1);
    let claude_home = make_home(&homes, Provider::Claude, claude);
    let codex_home = make_home(&homes, Provider::Codex, codex);
    let history = claude_home.join("conversation.fixture");
    fs::write(&history, b"existing synthetic history\0retained\n").unwrap();
    fs::set_permissions(&history, fs::Permissions::from_mode(0o640)).unwrap();
    let paths = [
        root.clone(),
        claude_home.parent().unwrap().to_path_buf(),
        codex_home.parent().unwrap().to_path_buf(),
        claude_home.clone(),
        codex_home.clone(),
    ];
    for path in &paths {
        fs::set_permissions(path, fs::Permissions::from_mode(0o751)).unwrap();
    }
    let before = paths
        .iter()
        .map(|path| metadata_identity(path))
        .collect::<Vec<_>>();
    let history_before = metadata_identity(&history);
    let expected = vec![
        Candidate {
            provider: Provider::Claude,
            key: claude,
        },
        Candidate {
            provider: Provider::Codex,
            key: codex,
        },
    ];

    assert_eq!(discover(&homes, &Registry::default()).unwrap(), expected);
    for candidate in &expected {
        assert_eq!(
            inspect(
                &homes,
                &Registry::default(),
                candidate.provider,
                candidate.key
            )
            .unwrap(),
            HomeStatus::Available(candidate.clone())
        );
    }

    assert_eq!(
        paths
            .iter()
            .map(|path| metadata_identity(path))
            .collect::<Vec<_>>(),
        before
    );
    assert_eq!(metadata_identity(&history), history_before);
    assert_eq!(
        fs::read(&history).unwrap(),
        b"existing synthetic history\0retained\n"
    );
    assert!(!root.join(".lifecycle.lock").exists());
}

#[test]
fn discovery_ignores_noncanonical_parked_file_and_symlink_entries() {
    let directory = tempfile::tempdir().unwrap();
    let homes = Homes::at(directory.path().join("accounts"));
    let key = Uuid::from_u128(1);
    let home = make_home(&homes, Provider::Claude, key);
    let provider_directory = home.parent().unwrap();
    let other = Uuid::parse_str("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee").unwrap();
    for name in [
        other.simple().to_string(),
        other.hyphenated().to_string().to_uppercase(),
        format!("{{{other}}}"),
        format!(".removing-{other}"),
        "unfinished-sign-in".into(),
    ] {
        fs::create_dir(provider_directory.join(name)).unwrap();
    }
    let parked_history = provider_directory.join(format!(".removing-{other}/history.fixture"));
    fs::write(&parked_history, b"parked history\n").unwrap();
    fs::set_permissions(&parked_history, fs::Permissions::from_mode(0o640)).unwrap();
    let parked_before = metadata_identity(&parked_history);
    let ignored = provider_directory.join(other.simple().to_string());
    fs::set_permissions(&ignored, fs::Permissions::from_mode(0o751)).unwrap();
    let ignored_before = metadata_identity(&ignored);
    let outside = directory.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let outside_history = outside.join("history.fixture");
    fs::write(&outside_history, b"outside history\n").unwrap();
    fs::set_permissions(&outside_history, fs::Permissions::from_mode(0o640)).unwrap();
    let outside_before = metadata_identity(&outside_history);
    let linked_key = Uuid::from_u128(3);
    let link = homes.path(Provider::Claude, linked_key);
    symlink(&outside, &link).unwrap();
    let link_before = metadata_identity(&link);
    let file_key = Uuid::from_u128(4);
    fs::write(homes.path(Provider::Claude, file_key), b"not a home\n").unwrap();

    assert_eq!(
        discover(&homes, &Registry::default()).unwrap(),
        vec![Candidate {
            provider: Provider::Claude,
            key
        }]
    );
    assert!(inspect(&homes, &Registry::default(), Provider::Claude, linked_key).is_err());
    assert!(inspect(&homes, &Registry::default(), Provider::Claude, file_key).is_err());
    assert_eq!(
        inspect(&homes, &Registry::default(), Provider::Claude, other).unwrap(),
        HomeStatus::Missing
    );
    assert_eq!(metadata_identity(&link), link_before);
    assert_eq!(fs::read_link(&link).unwrap(), outside);
    assert_eq!(metadata_identity(&outside_history), outside_before);
    assert_eq!(fs::read(&outside_history).unwrap(), b"outside history\n");
    assert_eq!(metadata_identity(&parked_history), parked_before);
    assert_eq!(metadata_identity(&ignored), ignored_before);
    assert_eq!(fs::read(&parked_history).unwrap(), b"parked history\n");
}

#[test]
fn symlinked_namespace_ancestors_are_refused_without_changing_the_target() {
    let directory = tempfile::tempdir().unwrap();
    let real_root = directory.path().join("real-accounts");
    let real_homes = Homes::at(&real_root);
    let key = Uuid::from_u128(1);
    let home = make_home(&real_homes, Provider::Claude, key);
    let sentinel = home.join("history.fixture");
    fs::write(&sentinel, b"retained outside namespace\n").unwrap();
    let sentinel_before = metadata_identity(&sentinel);
    let root_link = directory.path().join("linked-accounts");
    symlink(&real_root, &root_link).unwrap();
    let linked_homes = Homes::at(&root_link);
    assert!(discover(&linked_homes, &Registry::default()).is_err());
    assert!(inspect(&linked_homes, &Registry::default(), Provider::Claude, key).is_err());

    let root = directory.path().join("accounts");
    fs::create_dir(&root).unwrap();
    let provider_link = root.join(Provider::Claude.executable());
    symlink(home.parent().unwrap(), &provider_link).unwrap();
    let homes = Homes::at(&root);
    assert!(discover(&homes, &Registry::default()).is_err());
    assert!(inspect(&homes, &Registry::default(), Provider::Claude, key).is_err());
    assert_eq!(fs::read_link(&root_link).unwrap(), real_root);
    assert_eq!(
        fs::read_link(&provider_link).unwrap(),
        home.parent().unwrap()
    );
    assert_eq!(metadata_identity(&sentinel), sentinel_before);
    assert_eq!(
        fs::read(&sentinel).unwrap(),
        b"retained outside namespace\n"
    );
}

#[test]
fn registry_wide_registered_and_pending_keys_are_not_candidates() {
    let directory = tempfile::tempdir().unwrap();
    let homes = Homes::at(directory.path().join("accounts"));
    let registered = Uuid::from_u128(1);
    let pending = Uuid::from_u128(2);
    let available = Uuid::from_u128(3);
    for provider in [Provider::Claude, Provider::Codex] {
        make_home(&homes, provider, registered);
        make_home(&homes, provider, pending);
    }
    make_home(&homes, Provider::Claude, available);
    let mut registry = Registry::default();
    registry
        .register(registration(Provider::Codex, registered), &[])
        .unwrap();
    registry.begin(Provider::Claude, pending, 1);

    for provider in [Provider::Claude, Provider::Codex] {
        assert_eq!(
            inspect(&homes, &registry, provider, registered).unwrap(),
            HomeStatus::Registered
        );
        assert_eq!(
            inspect(&homes, &registry, provider, pending).unwrap(),
            HomeStatus::Pending
        );
    }
    assert_eq!(
        discover(&homes, &registry).unwrap(),
        vec![Candidate {
            provider: Provider::Claude,
            key: available
        }]
    );
}

#[test]
fn two_orphan_provider_homes_with_the_same_key_are_ambiguous() {
    let directory = tempfile::tempdir().unwrap();
    let homes = Homes::at(directory.path().join("accounts"));
    let key = Uuid::from_u128(1);
    let paths =
        [Provider::Claude, Provider::Codex].map(|provider| make_home(&homes, provider, key));
    let before = paths.each_ref().map(|path| metadata_identity(path));
    for path in &paths {
        fs::write(
            path.join("history.fixture"),
            b"retained ambiguous history\n",
        )
        .unwrap();
    }
    let registry = Registry::default();

    assert!(discover(&homes, &registry).unwrap().is_empty());
    for provider in [Provider::Claude, Provider::Codex] {
        assert_eq!(
            inspect(&homes, &registry, provider, key).unwrap(),
            HomeStatus::Ambiguous
        );
    }
    assert_eq!(paths.each_ref().map(|path| metadata_identity(path)), before);
    for path in &paths {
        assert_eq!(
            fs::read(path.join("history.fixture")).unwrap(),
            b"retained ambiguous history\n"
        );
    }
    // Reinspection never retains whichever provider happened to be enumerated first.
    fs::rename(
        &paths[1],
        paths[1].with_file_name(format!(".removing-{key}")),
    )
    .unwrap();
    assert_eq!(
        inspect(&homes, &registry, Provider::Claude, key).unwrap(),
        HomeStatus::Available(Candidate {
            provider: Provider::Claude,
            key
        })
    );
}

#[test]
fn a_registered_physical_linked_alias_prevents_duplicate_discovery() {
    let directory = tempfile::tempdir().unwrap();
    let homes = Homes::at(directory.path().join("accounts"));
    let key = Uuid::from_u128(1);
    let home = make_home(&homes, Provider::Claude, key);
    let history = home.join("history.fixture");
    fs::write(&history, b"same physical conversation home\n").unwrap();
    let alias = directory.path().join("registered-alias");
    symlink(&home, &alias).unwrap();
    let alias_before = metadata_identity(&alias);
    let mut row = registration(Provider::Claude, Uuid::from_u128(2));
    row.linked_home = Some(alias.clone());
    let mut registry = Registry::default();
    registry.register(row, &[]).unwrap();

    assert!(!registry.contains_managed_home(&homes, Provider::Claude, key));
    assert_eq!(
        inspect(&homes, &registry, Provider::Claude, key).unwrap(),
        HomeStatus::Represented
    );
    assert!(discover(&homes, &registry).unwrap().is_empty());
    assert_eq!(metadata_identity(&alias), alias_before);
    assert_eq!(fs::read_link(&alias).unwrap(), home);
    assert_eq!(
        fs::read(&history).unwrap(),
        b"same physical conversation home\n"
    );
}

#[test]
fn newer_registries_refuse_recovery_without_touching_homes() {
    let directory = tempfile::tempdir().unwrap();
    let homes = Homes::at(directory.path().join("accounts"));
    let key = Uuid::from_u128(1);
    let home = make_home(&homes, Provider::Claude, key);
    let history = home.join("history.fixture");
    fs::write(&history, b"newer installation history\n").unwrap();
    fs::set_permissions(&history, fs::Permissions::from_mode(0o640)).unwrap();
    let before = metadata_identity(&history);
    let registry: Registry = toml::from_str("version = 999\n").unwrap();
    assert!(registry.is_newer());

    assert_eq!(
        discover(&homes, &registry).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        inspect(&homes, &registry, Provider::Claude, key)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(metadata_identity(&history), before);
    assert_eq!(fs::read(&history).unwrap(), b"newer installation history\n");
}

#[test]
fn malformed_registry_errors_preserve_the_registry_and_surviving_home() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("accounts");
    let homes = Homes::at(&root);
    let home = make_home(&homes, Provider::Claude, Uuid::from_u128(1));
    let history = home.join("history.fixture");
    fs::write(&history, b"unregistered conversation\n").unwrap();
    fs::set_permissions(&history, fs::Permissions::from_mode(0o640)).unwrap();
    let home_before = metadata_identity(&home);
    let history_before = metadata_identity(&history);
    let config = directory.path().join("config");
    fs::create_dir(&config).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
    let registry_path = config.join("accounts.toml");
    let malformed = b"[[account]\nthis is not valid TOML\n";
    fs::write(&registry_path, malformed).unwrap();
    fs::set_permissions(&registry_path, fs::Permissions::from_mode(0o600)).unwrap();
    let registry_before = metadata_identity(&registry_path);
    let store = Store::at(&config);

    // A load error must be propagated, never replaced by an empty registry for discovery.
    assert_eq!(store.load().unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(metadata_identity(&registry_path), registry_before);
    assert_eq!(fs::read(&registry_path).unwrap(), malformed);
    assert_eq!(metadata_identity(&home), home_before);
    assert_eq!(metadata_identity(&history), history_before);
    assert_eq!(fs::read(&history).unwrap(), b"unregistered conversation\n");
    assert!(!root.join(".lifecycle.lock").exists());
}

#[test]
fn stale_candidates_are_reinspected_after_parking_links_or_registry_changes() {
    let directory = tempfile::tempdir().unwrap();
    let homes = Homes::at(directory.path().join("accounts"));
    let key = Uuid::from_u128(1);
    let home = make_home(&homes, Provider::Claude, key);
    let history = home.join("history.fixture");
    fs::write(&history, b"retained across reinspection\n").unwrap();
    fs::set_permissions(&history, fs::Permissions::from_mode(0o640)).unwrap();
    let before = metadata_identity(&history);
    let candidate = Candidate {
        provider: Provider::Claude,
        key,
    };
    assert_eq!(
        discover(&homes, &Registry::default()).unwrap(),
        vec![candidate.clone()]
    );
    let parked = home.parent().unwrap().join(format!(".removing-{key}"));
    fs::rename(&home, &parked).unwrap();
    assert_eq!(
        inspect(
            &homes,
            &Registry::default(),
            candidate.provider,
            candidate.key
        )
        .unwrap(),
        HomeStatus::Missing
    );
    assert!(discover(&homes, &Registry::default()).unwrap().is_empty());

    symlink(&parked, &home).unwrap();
    assert!(
        inspect(
            &homes,
            &Registry::default(),
            candidate.provider,
            candidate.key
        )
        .is_err()
    );
    assert!(discover(&homes, &Registry::default()).unwrap().is_empty());
    assert_eq!(fs::read_link(&home).unwrap(), parked);
    fs::remove_file(&home).unwrap();
    fs::rename(&parked, &home).unwrap();

    let mut registered = Registry::default();
    registered
        .register(registration(Provider::Claude, key), &[])
        .unwrap();
    assert_eq!(
        inspect(&homes, &registered, candidate.provider, candidate.key).unwrap(),
        HomeStatus::Registered
    );
    assert!(discover(&homes, &registered).unwrap().is_empty());
    let mut pending = Registry::default();
    pending.begin(Provider::Claude, key, 1);
    assert_eq!(
        inspect(&homes, &pending, candidate.provider, candidate.key).unwrap(),
        HomeStatus::Pending
    );
    assert!(discover(&homes, &pending).unwrap().is_empty());
    assert_eq!(metadata_identity(&history), before);
    assert_eq!(
        fs::read(&history).unwrap(),
        b"retained across reinspection\n"
    );
}
