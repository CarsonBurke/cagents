use std::{
    fs, io,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
};

use uuid::Uuid;

use super::{plan, restore};
use crate::{
    account::{
        Account, Identity,
        home::Homes,
        recovery::Candidate,
        registry::{Registered, Registry},
    },
    agent::Provider,
};

struct Fixture {
    directory: tempfile::TempDir,
    homes: Homes,
    candidate: Candidate,
    history: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let homes = Homes::at(directory.path().join("homes"));
        let candidate = Candidate {
            provider: Provider::Claude,
            key: Uuid::new_v4(),
        };
        let home = homes.path(candidate.provider, candidate.key);
        fs::create_dir_all(&home).unwrap();
        let history = home.join("history.fixture");
        fs::write(
            &history,
            b"synthetic history retained under its original key\n",
        )
        .unwrap();
        fs::set_permissions(&history, fs::Permissions::from_mode(0o640)).unwrap();
        Self {
            directory,
            homes,
            candidate,
            history,
        }
    }

    fn account(&self) -> Account {
        Account::managed(
            self.candidate.provider,
            self.candidate.key,
            self.homes.path(self.candidate.provider, self.candidate.key),
        )
    }
}

fn identity(email: &str) -> Identity {
    Identity {
        email: Some(email.into()),
        organization: Some("fixture-organization".into()),
        plan: None,
    }
}

fn registered(provider: Provider, key: Uuid, email: &str) -> Registered {
    Registered {
        key,
        provider,
        linked_home: None,
        label: None,
        email: Some(email.into()),
        organization: Some("fixture-organization".into()),
        added_at: 1,
    }
}

fn metadata_identity(path: &Path) -> (u64, u64, u32, u32) {
    let metadata = fs::symlink_metadata(path).unwrap();
    (
        metadata.dev(),
        metadata.ino(),
        metadata.mode(),
        metadata.uid(),
    )
}

fn error_kind<T>(result: io::Result<T>) -> io::ErrorKind {
    match result {
        Err(error) => error.kind(),
        Ok(_) => panic!("expected recovery refusal"),
    }
}

#[test]
fn an_old_preference_closure_cannot_republish_choices_into_a_reset_registry() {
    let fixture = Fixture::new();
    let store = crate::account::registry::Store::at(fixture.directory.path().join("registry"));
    let system = Account::system(Provider::Claude);
    let old = store
        .update(|registry| registry.set_label(&system, Some("Before reset".into())))
        .unwrap()
        .0;
    assert!(old.revision() > 0);
    fs::remove_file(fixture.directory.path().join("registry/accounts.toml")).unwrap();
    let mut called = false;
    assert!(
        store
            .update_after_revision(old.revision(), |_| called = true)
            .is_err()
    );
    assert!(!called);
    assert_eq!(store.load().unwrap(), Registry::default());
    assert!(
        !fixture
            .directory
            .path()
            .join("registry/accounts.toml")
            .exists()
    );
}

#[test]
fn preference_revision_floor_preserves_newer_unrelated_changes_under_the_same_lease() {
    let fixture = Fixture::new();
    let store = crate::account::registry::Store::at(fixture.directory.path().join("registry"));
    let claude = Account::system(Provider::Claude);
    let codex = Account::system(Provider::Codex);
    let first = store
        .update(|registry| registry.set_label(&claude, Some("Original".into())))
        .unwrap()
        .0;
    let concurrent = store
        .update(|registry| registry.set_label(&codex, Some("Concurrent".into())))
        .unwrap()
        .0;
    let current = store
        .update_after_revision(first.revision(), |registry| {
            registry.set_label(&claude, Some("Chosen".into()))
        })
        .unwrap();
    assert_eq!(current.label(&claude), Some("Chosen"));
    assert_eq!(current.label(&codex), Some("Concurrent"));
    assert_eq!(current.revision(), concurrent.revision() + 1);
    assert_eq!(store.load().unwrap(), current);
}

#[test]
fn planning_refuses_newer_registries_and_unverified_identities_without_changes() {
    let fixture = Fixture::new();
    let confirmed = identity("restored@example.invalid");
    let newer: Registry = toml::from_str("version = 999\nrevision = 7\n").unwrap();
    assert_eq!(
        error_kind(plan(
            &newer,
            &fixture.homes,
            &fixture.candidate,
            &confirmed,
            None,
            20
        )),
        io::ErrorKind::Unsupported
    );
    for unverified in [
        Identity::default(),
        identity(""),
        identity(" \t "),
        identity("broken\nemail"),
    ] {
        let mut registry = Registry::default();
        let before = registry.clone();
        assert_eq!(
            error_kind(restore(
                &mut registry,
                &fixture.homes,
                &fixture.candidate,
                &unverified,
                None,
                20
            )),
            io::ErrorKind::InvalidData
        );
        assert_eq!(registry, before);
    }
    let mut registry = Registry::default();
    let before = registry.clone();
    assert_eq!(
        error_kind(restore(
            &mut registry,
            &fixture.homes,
            &fixture.candidate,
            &confirmed,
            Some(&Identity::default()),
            20,
        )),
        io::ErrorKind::InvalidData
    );
    assert_eq!(registry, before);
}

#[test]
fn planning_refuses_registry_wide_keys_pending_links_and_orphan_archive_metadata() {
    let fixture = Fixture::new();
    let confirmed = identity("restored@example.invalid");
    for conflict in ["registered", "pending", "linked", "archived-key"] {
        let mut registry = Registry::default();
        match conflict {
            "registered" => registry
                .register(
                    registered(
                        Provider::Codex,
                        fixture.candidate.key,
                        "other@example.invalid",
                    ),
                    &[],
                )
                .unwrap(),
            "pending" => registry.begin(Provider::Codex, fixture.candidate.key, 1),
            "linked" => {
                let mut row =
                    registered(Provider::Claude, Uuid::new_v4(), "linked@example.invalid");
                row.linked_home = Some(
                    fixture
                        .homes
                        .path(fixture.candidate.provider, fixture.candidate.key),
                );
                registry.register(row, &[]).unwrap();
            }
            "archived-key" => {
                registry = toml::from_str(&format!("archived = [\"{}\"]\n", fixture.candidate.key))
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let before = registry.clone();
        assert_eq!(
            error_kind(restore(
                &mut registry,
                &fixture.homes,
                &fixture.candidate,
                &confirmed,
                None,
                20
            )),
            io::ErrorKind::AlreadyExists,
            "{conflict}"
        );
        assert_eq!(registry, before, "{conflict}");
    }
}

#[test]
fn a_system_duplicate_keeps_the_original_row_and_saved_session_identity() {
    // Pure metadata planning: this does not stand in for a System status probe.
    let fixture = Fixture::new();
    let before = metadata_identity(&fixture.history);
    let confirmed = identity("restored@example.invalid");
    let system = identity(" RESTORED@example.invalid ");
    let mut registry = Registry::default();
    registry.set_default(&fixture.account());
    let project = fixture.directory.path().join("project");
    registry.set_project_default(&project, &fixture.account());

    let outcome = restore(
        &mut registry,
        &fixture.homes,
        &fixture.candidate,
        &confirmed,
        Some(&system),
        20,
    )
    .unwrap();

    assert_eq!(outcome.candidate, fixture.candidate);
    assert_eq!(
        outcome.duplicate_of,
        Some(Account::system(Provider::Claude).id())
    );
    assert!(registry.is_archived(fixture.candidate.key));
    let row = registry.find(fixture.candidate.key).unwrap();
    assert_eq!(row.key, fixture.candidate.key);
    assert!(row.linked_home.is_none());
    assert_eq!(row.email, confirmed.email);
    assert!(
        registry
            .all_accounts(Some(&fixture.homes))
            .contains(&fixture.account())
    );
    assert_eq!(
        registry.default_account(Provider::Claude, Some(&fixture.homes)),
        Account::system(Provider::Claude)
    );
    assert_eq!(
        registry.project_default_account(&project, Provider::Claude, Some(&fixture.homes)),
        Account::system(Provider::Claude)
    );
    assert_eq!(metadata_identity(&fixture.history), before);
    assert_eq!(
        fs::read(&fixture.history).unwrap(),
        b"synthetic history retained under its original key\n"
    );
}

#[test]
fn an_active_duplicate_is_retained_while_the_recovered_row_is_archived() {
    let fixture = Fixture::new();
    let retained_key = Uuid::new_v4();
    let row = registered(Provider::Claude, retained_key, "restored@example.invalid");
    let retained = Account::managed(
        Provider::Claude,
        retained_key,
        fixture.homes.path(Provider::Claude, retained_key),
    );
    let mut registry = Registry::default();
    registry.register(row.clone(), &[]).unwrap();
    registry.set_default(&fixture.account());

    let outcome = restore(
        &mut registry,
        &fixture.homes,
        &fixture.candidate,
        &identity("restored@example.invalid"),
        None,
        20,
    )
    .unwrap();

    assert_eq!(outcome.duplicate_of, Some(retained.id()));
    assert_eq!(registry.find(retained_key), Some(&row));
    assert!(!registry.is_archived(retained_key));
    assert!(registry.is_archived(fixture.candidate.key));
    assert_eq!(
        registry.default_account(Provider::Claude, Some(&fixture.homes)),
        retained
    );
    assert!(
        registry
            .all_accounts(Some(&fixture.homes))
            .contains(&fixture.account())
    );
}

#[test]
fn an_archived_existing_login_does_not_hide_a_newly_recovered_row() {
    let fixture = Fixture::new();
    let archived_key = Uuid::new_v4();
    let mut registry = Registry::default();
    registry
        .register(
            registered(Provider::Claude, archived_key, "restored@example.invalid"),
            &[],
        )
        .unwrap();
    assert!(registry.archive_duplicate(archived_key, &Account::system(Provider::Claude)));
    registry.set_default(&fixture.account());

    let outcome = restore(
        &mut registry,
        &fixture.homes,
        &fixture.candidate,
        &identity("restored@example.invalid"),
        None,
        20,
    )
    .unwrap();

    assert!(outcome.duplicate_of.is_none());
    assert!(registry.is_archived(archived_key));
    assert!(!registry.is_archived(fixture.candidate.key));
    assert_eq!(
        registry.default_account(Provider::Claude, Some(&fixture.homes)),
        fixture.account()
    );
}

#[cfg(feature = "host")]
mod checked_store {
    use super::*;
    use crate::{
        account::registry::Store,
        host::{AccountRecovery, AccountReservation},
    };

    fn with_recovery(run: impl FnOnce(&Fixture, &Store, &AccountRecovery)) {
        let fixture = Fixture::new();
        let store = Store::at(fixture.directory.path().join("registry"));
        store
            .update(|registry| {
                registry.set_label(&Account::system(Provider::Codex), Some("fixture".into()))
            })
            .unwrap();
        let host = AccountReservation::test_host();
        let recovery = AccountRecovery::test_install(
            &host,
            store.clone(),
            &fixture.homes,
            fixture.candidate.clone(),
        )
        .unwrap();
        run(&fixture, &store, &recovery);
    }

    #[test]
    fn checked_restore_preserves_unrelated_edits_and_advances_revision_once() {
        with_recovery(|fixture, store, recovery| {
            let history_before = metadata_identity(&fixture.history);
            store
                .update(|registry| {
                    registry.set_label(
                        &Account::system(Provider::Codex),
                        Some("unrelated newer edit".into()),
                    )
                })
                .unwrap();
            let before = store.load().unwrap();
            let confirmed = identity("restored@example.invalid");

            let (saved, outcome) = store
                .try_restore_managed_reserved(recovery, &confirmed, None, 20)
                .unwrap();

            assert_eq!(saved.revision(), before.revision() + 1);
            assert_eq!(
                saved.label(&Account::system(Provider::Codex)),
                Some("unrelated newer edit")
            );
            assert_eq!(outcome.candidate, fixture.candidate);
            assert!(outcome.duplicate_of.is_none());
            let row = saved.find(fixture.candidate.key).unwrap();
            assert_eq!(row.provider, fixture.candidate.provider);
            assert_eq!(row.email, confirmed.email);
            assert_eq!(row.organization, confirmed.organization);
            assert_eq!(row.added_at, 20);
            assert!(row.linked_home.is_none());
            assert_eq!(store.load().unwrap(), saved);
            assert_eq!(metadata_identity(&fixture.history), history_before);
            assert_eq!(
                fs::read(&fixture.history).unwrap(),
                b"synthetic history retained under its original key\n"
            );
        });
    }

    #[test]
    fn checked_duplicate_decisions_insert_and_archive_in_one_revision() {
        for existing_is_archived in [false, true] {
            with_recovery(|fixture, store, recovery| {
                let retained_key = Uuid::new_v4();
                let (before, ()) = store
                    .update(|registry| {
                        registry
                            .register(
                                registered(
                                    Provider::Claude,
                                    retained_key,
                                    "restored@example.invalid",
                                ),
                                &[],
                            )
                            .unwrap();
                        if existing_is_archived {
                            assert!(registry.archive_duplicate(
                                retained_key,
                                &Account::system(Provider::Claude)
                            ));
                        }
                    })
                    .unwrap();

                let (saved, outcome) = store
                    .try_restore_managed_reserved(
                        recovery,
                        &identity("restored@example.invalid"),
                        None,
                        20,
                    )
                    .unwrap();

                assert_eq!(saved.revision(), before.revision() + 1);
                assert_eq!(
                    saved.is_archived(fixture.candidate.key),
                    !existing_is_archived
                );
                assert_eq!(saved.is_archived(retained_key), existing_is_archived);
                assert_eq!(outcome.duplicate_of.is_some(), !existing_is_archived);
                assert!(saved.find(fixture.candidate.key).is_some());
                assert!(saved.find(retained_key).is_some());
                assert_eq!(store.load().unwrap(), saved);
            });
        }
    }

    #[test]
    fn lifecycle_and_registry_contention_refuse_without_a_write_or_leaked_lock() {
        with_recovery(|fixture, store, recovery| {
            let before = fs::read(store.path()).unwrap();
            let lifecycle = fixture.homes.lifecycle_lock().unwrap();
            assert_eq!(
                error_kind(store.try_restore_managed_reserved(
                    recovery,
                    &identity("restored@example.invalid"),
                    None,
                    20
                )),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(
                error_kind(store.try_recovery_status_lease(recovery)),
                io::ErrorKind::WouldBlock
            );
            drop(lifecycle);
            let registry_lock = store.lock().unwrap();
            assert_eq!(
                error_kind(store.try_restore_managed_reserved(
                    recovery,
                    &identity("restored@example.invalid"),
                    None,
                    20
                )),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(
                error_kind(store.try_recovery_status_lease(recovery)),
                io::ErrorKind::WouldBlock
            );
            drop(registry_lock);
            assert_eq!(fs::read(store.path()).unwrap(), before);
            drop(fixture.homes.try_lifecycle_lock().unwrap());
            drop(store.try_lock().unwrap());
        });
    }

    #[test]
    fn status_lease_retains_both_locks_until_dropped() {
        with_recovery(|fixture, store, recovery| {
            let before = fs::read(store.path()).unwrap();
            let lease = store.try_recovery_status_lease(recovery).unwrap();
            assert_eq!(
                error_kind(fixture.homes.try_lifecycle_lock()),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(error_kind(store.try_lock()), io::ErrorKind::WouldBlock);
            drop(lease);
            drop(fixture.homes.try_lifecycle_lock().unwrap());
            drop(store.try_lock().unwrap());
            assert_eq!(fs::read(store.path()).unwrap(), before);
        });
    }

    #[test]
    fn changed_registration_pending_link_or_version_refuses_before_publication() {
        for race in ["registered", "pending", "linked-alias", "newer"] {
            with_recovery(|fixture, store, recovery| {
                match race {
                    "registered" => {
                        store
                            .update(|registry| {
                                registry
                                    .register(
                                        registered(
                                            Provider::Codex,
                                            fixture.candidate.key,
                                            "racing@example.invalid",
                                        ),
                                        &[],
                                    )
                                    .unwrap()
                            })
                            .unwrap();
                    }
                    "pending" => {
                        store
                            .update(|registry| {
                                registry.begin(Provider::Codex, fixture.candidate.key, 2)
                            })
                            .unwrap();
                    }
                    "linked-alias" => {
                        let alias = fixture.directory.path().join("linked-alias");
                        symlink(
                            fixture
                                .homes
                                .path(fixture.candidate.provider, fixture.candidate.key),
                            &alias,
                        )
                        .unwrap();
                        let mut row =
                            registered(Provider::Claude, Uuid::new_v4(), "linked@example.invalid");
                        row.linked_home = Some(alias);
                        store
                            .update(|registry| registry.register(row, &[]).unwrap())
                            .unwrap();
                    }
                    "newer" => {
                        fs::write(store.path(), b"version = 999\nrevision = 41\n").unwrap();
                    }
                    _ => unreachable!(),
                }
                let before = fs::read(store.path()).unwrap();
                assert!(
                    store
                        .try_restore_managed_reserved(
                            recovery,
                            &identity("restored@example.invalid"),
                            None,
                            20
                        )
                        .is_err(),
                    "{race}"
                );
                assert!(store.try_recovery_status_lease(recovery).is_err(), "{race}");
                assert_eq!(fs::read(store.path()).unwrap(), before, "{race}");
            });
        }
    }

    #[test]
    fn a_parked_or_symlinked_home_refuses_after_its_original_scope_was_captured() {
        for replace_with_link in [false, true] {
            with_recovery(|fixture, store, recovery| {
                let history_before = metadata_identity(&fixture.history);
                let home = fixture
                    .homes
                    .path(fixture.candidate.provider, fixture.candidate.key);
                let parked = home.with_file_name(format!(".removing-{}", fixture.candidate.key));
                fs::rename(&home, &parked).unwrap();
                if replace_with_link {
                    symlink(&parked, &home).unwrap();
                }
                let before = fs::read(store.path()).unwrap();
                assert!(
                    store
                        .try_restore_managed_reserved(
                            recovery,
                            &identity("restored@example.invalid"),
                            None,
                            20
                        )
                        .is_err()
                );
                assert!(store.try_recovery_status_lease(recovery).is_err());
                assert_eq!(fs::read(store.path()).unwrap(), before);
                let history = parked.join("history.fixture");
                assert_eq!(metadata_identity(&history), history_before);
                assert_eq!(
                    fs::read(history).unwrap(),
                    b"synthetic history retained under its original key\n"
                );
            });
        }
    }

    #[test]
    fn a_scope_from_another_store_is_refused_before_creating_that_store() {
        with_recovery(|fixture, _store, recovery| {
            let root = fixture.directory.path().join("uncreated-other-registry");
            let other = Store::at(&root);
            assert_eq!(
                error_kind(other.try_restore_managed_reserved(
                    recovery,
                    &identity("restored@example.invalid"),
                    None,
                    20
                )),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                error_kind(other.try_recovery_status_lease(recovery)),
                io::ErrorKind::PermissionDenied
            );
            assert!(!root.exists());
        });
    }
}
