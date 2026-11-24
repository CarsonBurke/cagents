//! The account lifecycle's remote half, as the host's reservation carries it
//! out: [`cagents::account::remote`] drives the steps and the host's
//! [`Reservation`](super::account_lifecycle::Reservation) holds the lock.
use crate::account::remote::*;
use crate::{
    account::{
        Account,
        home::Homes,
        registry::{Registered, Store},
    },
    agent::Provider,
};
use std::os::unix::fs::MetadataExt;

#[test]
fn host_lifecycle_never_deletes_managed_or_linked_files_without_a_permit() {
    let root = tempfile::tempdir().unwrap();
    let homes = Homes::at(root.path().join("homes"));
    let store = Store::at(root.path().join("registry"));
    let system = root.path().join("system");
    std::fs::create_dir(&system).unwrap();
    let credential_file = ["auth", ".json"].concat();
    std::fs::write(system.join(&credential_file), b"synthetic-private-login").unwrap();
    let (key, home) = store
        .create_pending(&homes, Provider::Codex, 1, || {
            homes.create(Provider::Codex, Some(&system))
        })
        .unwrap();
    let recipe = login_recipe(
        &Account::managed(Provider::Codex, key, home.clone()),
        "/bin/false".into(),
    )
    .unwrap();
    assert_eq!(recipe.arguments, ["login", "--device-auth"]);
    assert_eq!(
        recipe.environment.get("CODEX_HOME"),
        Some(&home.display().to_string())
    );
    assert!(!home.join(&credential_file).exists());
    let value = serde_json::to_string(&Created {
        key,
        home: home.clone(),
        recipe,
    })
    .unwrap();
    assert!(!value.contains("synthetic-private-login"));
    std::fs::write(home.join("provider-payload"), b"keep managed history").unwrap();
    store
        .update(|registry| {
            registry.register(
                Registered {
                    key,
                    provider: Provider::Codex,
                    linked_home: None,
                    label: None,
                    email: None,
                    organization: None,
                    added_at: 1,
                },
                &[],
            )
        })
        .unwrap()
        .1
        .unwrap();
    let account = Account::managed(Provider::Codex, key, home.clone());
    let host = super::AccountReservation::test_host();
    let reservation = super::AccountReservation::test_install(
        &host,
        std::slice::from_ref(&account),
        store.clone(),
        &homes,
    )
    .unwrap();
    let settled: Settled = serde_json::from_value(
        execute_at(
            Operation::Register {
                provider: Provider::Codex,
                key,
            },
            &reservation,
            homes.clone(),
            store.clone(),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(matches!(settled, Settled::Registered { .. }));
    let before = store.load().unwrap();
    let inode = home.metadata().unwrap().ino();
    let error = execute_at(
        Operation::Remove { id: account.id() },
        &reservation,
        homes.clone(),
        store.clone(),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Your account and its files were kept")
    );
    assert_eq!(before, store.load().unwrap());
    assert_eq!(inode, home.metadata().unwrap().ino());
    assert_eq!(
        std::fs::read(home.join("provider-payload")).unwrap(),
        b"keep managed history"
    );
    drop(reservation);
    let (pending_key, pending_home) = store
        .create_pending(&homes, Provider::Codex, 2, || {
            homes.create(Provider::Codex, None)
        })
        .unwrap();
    let pending = Account::managed(Provider::Codex, pending_key, pending_home.clone());
    let pending_reservation =
        super::AccountReservation::test_install(&host, &[pending], store.clone(), &homes).unwrap();
    execute_at(
        Operation::Abandon {
            provider: Provider::Codex,
            key: pending_key,
        },
        &pending_reservation,
        homes.clone(),
        store.clone(),
    )
    .unwrap();
    assert!(
        pending_home.is_dir(),
        "abandoned sign-in metadata grants no file deletion proof"
    );
    assert!(
        store
            .load()
            .unwrap()
            .pending()
            .iter()
            .all(|pending| pending.key != pending_key)
    );
    drop(pending_reservation);
    let linked = root.path().join("linked");
    std::fs::create_dir(&linked).unwrap();
    std::fs::write(linked.join("history"), b"keep linked history").unwrap();
    let linked_key = uuid::Uuid::new_v4();
    store
        .update(|registry| {
            registry.link(
                Provider::Codex,
                linked_key,
                linked.clone(),
                None,
                Some(&homes),
                1,
            )
        })
        .unwrap()
        .1
        .unwrap();
    let account = Account::linked(Provider::Codex, linked_key, linked.clone());
    let reservation = super::AccountReservation::test_install(
        &host,
        std::slice::from_ref(&account),
        store.clone(),
        &homes,
    )
    .unwrap();
    execute_at(
        Operation::Remove { id: account.id() },
        &reservation,
        homes,
        store.clone(),
    )
    .unwrap();
    assert!(store.load().unwrap().find(linked_key).is_none());
    assert_eq!(
        std::fs::read(linked.join("history")).unwrap(),
        b"keep linked history"
    );
}

/// A pending sign-in, a linked account and a home to link, each under
/// its own reservation with `patience` for a held account-list lock.
struct HeldList {
    _root: tempfile::TempDir,
    homes: Homes,
    store: Store,
    host: crate::host::SharedHost,
    pending: (uuid::Uuid, std::path::PathBuf),
    linked: Account,
    unlinked: std::path::PathBuf,
}

impl HeldList {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let pending = store
            .create_pending(&homes, Provider::Codex, 1, || {
                homes.create(Provider::Codex, None)
            })
            .unwrap();
        let linked_home = root.path().join("linked");
        let unlinked = root.path().join("unlinked");
        std::fs::create_dir(&linked_home).unwrap();
        std::fs::create_dir(&unlinked).unwrap();
        let linked_key = uuid::Uuid::new_v4();
        store
            .update(|registry| {
                registry.link(
                    Provider::Codex,
                    linked_key,
                    linked_home.clone(),
                    None,
                    Some(&homes),
                    1,
                )
            })
            .unwrap()
            .1
            .unwrap();
        Self {
            homes,
            store,
            host: super::AccountReservation::test_host(),
            pending,
            linked: Account::linked(Provider::Codex, linked_key, linked_home),
            unlinked,
            _root: root,
        }
    }

    /// Every operation that changes this list, with what it reserves.
    fn operations(&self) -> Vec<(Operation, Vec<Account>)> {
        let (key, home) = self.pending.clone();
        let pending = Account::managed(Provider::Codex, key, home);
        let link_key = uuid::Uuid::new_v4();
        vec![
            (
                Operation::Register {
                    provider: Provider::Codex,
                    key,
                },
                vec![pending.clone(), Account::system(Provider::Codex)],
            ),
            (
                Operation::Abandon {
                    provider: Provider::Codex,
                    key,
                },
                vec![pending],
            ),
            (
                Operation::Remove {
                    id: self.linked.id(),
                },
                vec![self.linked.clone()],
            ),
            (
                Operation::Link {
                    provider: Provider::Codex,
                    key: link_key,
                    path: self.unlinked.display().to_string(),
                },
                vec![Account::linked(
                    Provider::Codex,
                    link_key,
                    self.unlinked.clone(),
                )],
            ),
            (
                Operation::Restore {
                    provider: Provider::Codex,
                    key: uuid::Uuid::new_v4(),
                },
                Vec::new(),
            ),
        ]
    }

    fn reserve(
        &self,
        accounts: &[Account],
        patience: std::time::Duration,
    ) -> super::AccountReservation {
        super::AccountReservation::test_install(
            &self.host,
            accounts,
            self.store.clone(),
            &self.homes,
        )
        .unwrap()
        .with_patience(patience)
    }

    fn execute(
        &self,
        operation: Operation,
        reservation: &super::AccountReservation,
    ) -> Result<serde_json::Value, LifecycleError> {
        execute_at(
            operation,
            reservation,
            self.homes.clone(),
            self.store.clone(),
        )
    }
}

#[test]
fn a_held_account_list_refuses_every_change_retryably_and_changes_nothing() {
    use crate::account::remote::ListHeld;
    let list = HeldList::new();
    let before = list.store.load().unwrap();
    let patience = std::time::Duration::from_millis(30);
    for (operation, accounts) in list.operations() {
        let reservation = list.reserve(&accounts, patience);
        let held = list.store.lock().unwrap();
        let started = std::time::Instant::now();
        let error = list.execute(operation.clone(), &reservation).unwrap_err();
        assert_eq!(
            error,
            LifecycleError::ListHeld(ListHeld::Registry),
            "{operation:?}"
        );
        assert!(
            started.elapsed() >= patience,
            "{operation:?} asked only once"
        );
        drop(held);
        assert_eq!(list.store.load().unwrap(), before, "{operation:?}");
    }
    // The lifecycle lock is taken only by the changes that need it, after
    // their reservation was verified: linking a home here.
    let (operation, accounts) = list
        .operations()
        .into_iter()
        .find(|(operation, _)| matches!(operation, Operation::Link { .. }))
        .unwrap();
    let reservation = list.reserve(&accounts, patience);
    let held = list.homes.lifecycle_lock().unwrap();
    let error = list.execute(operation.clone(), &reservation).unwrap_err();
    assert_eq!(error, LifecycleError::ListHeld(ListHeld::Lifecycle));
    assert_eq!(list.store.load().unwrap(), before);
    drop(held);
    // Nothing was changed, so the same request succeeds once it is free.
    let Operation::Link { key, .. } = operation else {
        unreachable!()
    };
    list.execute(operation, &reservation).unwrap();
    assert!(list.store.load().unwrap().find(key).is_some());
}

#[test]
fn a_change_waits_out_an_account_list_held_for_a_moment() {
    let list = HeldList::new();
    let (operation, accounts) = list
        .operations()
        .into_iter()
        .find(|(operation, _)| matches!(operation, Operation::Abandon { .. }))
        .unwrap();
    let reservation = list.reserve(&accounts, std::time::Duration::from_secs(30));
    let held = list.store.lock().unwrap();
    std::thread::scope(|scope| {
        let abandoning = scope.spawn(|| list.execute(operation, &reservation));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!abandoning.is_finished(), "the change did not wait");
        drop(held);
        abandoning.join().unwrap().unwrap();
    });
    assert!(list.store.load().unwrap().pending().is_empty());
}

#[test]
fn a_stale_release_writes_no_marker_after_another_window_registers_the_sign_in() {
    let list = HeldList::new();
    let (key, home) = list.pending.clone();
    let provider = Provider::Codex;
    let account = Account::managed(provider, key, home.clone());
    let reservation = list.reserve(&[account], std::time::Duration::from_secs(2));
    let registered = Registered {
        key,
        provider,
        linked_home: None,
        label: None,
        email: Some("registered@example.com".into()),
        organization: None,
        added_at: 1,
    };
    let (store, homes, registration) = (list.store.clone(), list.homes.clone(), registered.clone());
    // Another window wins after Abandon's prechecks and commit's earlier
    // verification, immediately before the locked release update.
    BEFORE_RELEASE_UPDATE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            store
                .register_pending(&homes, registration, &[])
                .unwrap()
                .1
                .unwrap();
        }));
    });

    let error = list
        .execute(Operation::Abandon { provider, key }, &reservation)
        .unwrap_err();
    assert_eq!(
        error,
        LifecycleError::Failed(
            "The affected account registration changed; reload accounts before retrying".into()
        )
    );
    assert!(!list.homes.is_released(provider, key));
    let registry = list.store.load().unwrap();
    assert_eq!(registry.find(key), Some(&registered));
    assert!(registry.pending().is_empty() && !registry.is_released(key));
    drop(reservation);

    std::fs::remove_file(list._root.path().join("registry/accounts.toml")).unwrap();
    let lost = list.store.load().unwrap();
    assert!(
        list.homes
            .unregistered(&lost)
            .iter()
            .any(|(found_provider, found, _)| (*found_provider, *found) == (provider, key))
    );
    assert!(home.is_dir());
}

#[test]
fn a_failed_release_save_does_not_mark_a_later_registered_home_as_released() {
    let list = HeldList::new();
    let (key, home) = list.pending.clone();
    let provider = Provider::Codex;
    let account = Account::managed(provider, key, home.clone());
    let reservation = list.reserve(&[account], std::time::Duration::from_secs(2));
    let before = list.store.load().unwrap();
    let path = list._root.path().join("registry/accounts.toml");
    let bytes = std::fs::read(&path).unwrap();
    list.store.fail_next_write_for_test();

    assert_eq!(
        list.execute(Operation::Abandon { provider, key }, &reservation)
            .unwrap_err(),
        LifecycleError::Failed("test account registry write failure".into())
    );
    assert_eq!(list.store.load().unwrap(), before);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(!list.homes.is_released(provider, key));
    drop(reservation);

    let registered = Registered {
        key,
        provider,
        linked_home: None,
        label: None,
        email: Some("registered@example.com".into()),
        organization: None,
        added_at: 1,
    };
    let (registry, result) = list
        .store
        .register_pending(&list.homes, registered.clone(), &[])
        .unwrap();
    assert_eq!(result, Ok(()));
    assert_eq!(registry.find(key), Some(&registered));
    assert!(registry.pending().is_empty() && !registry.is_released(key));

    std::fs::remove_file(path).unwrap();
    let lost = list.store.load().unwrap();
    assert!(lost.find(key).is_none() && !lost.is_released(key));
    assert!(
        list.homes
            .unregistered(&lost)
            .iter()
            .any(|(found_provider, found, _)| (*found_provider, *found) == (provider, key))
    );
    assert!(home.is_dir() && !list.homes.is_released(provider, key));

    let account = Account::managed(provider, key, home);
    let reservation = list.reserve(&[account], std::time::Duration::from_secs(2));
    let (registry, restored) = reservation
        .commit(|| {
            list.store
                .try_restore_reserved(&reservation, &list.homes, registered.clone(), &[])
        })
        .unwrap();
    assert_eq!(restored, crate::account::registry::Restored::Listed);
    assert_eq!(registry.find(key), Some(&registered));
}

#[test]
fn an_abandoned_sign_in_is_not_restored_after_the_account_list_is_lost() {
    let list = HeldList::new();
    let (key, home) = list.pending.clone();
    let provider = Provider::Codex;
    let account = Account::managed(provider, key, home.clone());
    let reservation = list.reserve(
        std::slice::from_ref(&account),
        std::time::Duration::from_secs(2),
    );
    let settled: Settled = serde_json::from_value(
        list.execute(Operation::Abandon { provider, key }, &reservation)
            .unwrap(),
    )
    .unwrap();
    assert!(matches!(settled, Settled::Abandoned(Some(_))));
    assert!(list.store.load().unwrap().is_released(key));
    assert!(list.homes.is_released(provider, key));
    drop(reservation);

    std::fs::remove_file(list._root.path().join("registry/accounts.toml")).unwrap();
    let lost = list.store.load().unwrap();
    assert!(lost.pending().is_empty() && lost.find(key).is_none());
    assert!(!lost.is_released(key));
    assert!(
        list.homes
            .unregistered(&lost)
            .iter()
            .all(|(_, found, _)| *found != key)
    );
    let reservation = list.reserve(&[account], std::time::Duration::from_secs(2));
    assert_eq!(
        list.execute(Operation::Restore { provider, key }, &reservation)
            .unwrap_err(),
        LifecycleError::Failed(RELEASED.into())
    );
    assert!(home.is_dir() && list.homes.is_released(provider, key));
    assert_eq!(list.store.load().unwrap(), lost);
}

#[test]
fn a_prepared_remote_login_uses_its_selected_path_snapshot() {
    let selected = std::ffi::OsString::from("/old/node/bin:/old/cli/bin");
    let recipe = login_recipe_with_path(
        &Account::system(Provider::Codex),
        "/old/cli/bin/codex".into(),
        Some(&selected),
    )
    .unwrap();
    assert_eq!(recipe.program, "/old/cli/bin/codex");
    assert_eq!(
        recipe.environment.get("PATH").map(String::as_str),
        selected.to_str()
    );
    assert_eq!(recipe.arguments, ["login", "--device-auth"]);
    let no_path = login_recipe_with_path(
        &Account::system(Provider::Codex),
        "/old/cli/bin/codex".into(),
        None,
    )
    .unwrap();
    assert!(!no_path.environment.contains_key("PATH"));
}
