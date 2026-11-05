//! Reducer regressions for callbacks that outlive a checked registry reset.
//! Every returned Task is dropped: these fixtures never poll a provider,
//! contact a daemon, or execute a registration/removal request.

use super::super::{Login, LoginPhase, RegistryRequest, RegistryWrite, State};
use super::*;
use crate::account::{
    UsageStatus, home::Homes, probe::Outcome as ProbeOutcome, registry::Registered,
};

fn fixture() -> (Cagents, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.account_ui = State::new(
        None,
        None,
        Some(Homes::at(directory.path().join("accounts"))),
    );
    (app, directory)
}

fn registry(revision: u64) -> Registry {
    serde_json::from_value(serde_json::json!({ "revision": revision })).unwrap()
}

fn registered_registry(revision: u64, key: Uuid) -> Registry {
    let mut registry = registry(revision);
    registry
        .register(
            Registered {
                key,
                provider: Provider::Codex,
                linked_home: None,
                label: None,
                email: Some("preserved@example.invalid".into()),
                organization: None,
                added_at: 1,
            },
            &[],
        )
        .unwrap();
    registry
}

fn login(app: &mut Cagents, key: Uuid) {
    let home = app
        .account_ui
        .homes
        .as_ref()
        .unwrap()
        .path(Provider::Codex, key);
    app.account_ui.registry.begin(Provider::Codex, key, 1);
    app.account_ui.logins.push(Login {
        provider: Provider::Codex,
        key,
        home,
        phase: LoginPhase::Verifying,
        attempts: 1,
        next_check: 17,
        started_at: 1,
        recovered: false,
    });
}

fn start_verification(app: &mut Cagents, key: Uuid) -> RegistryWrite {
    drop(app.verify_login(key));
    let operation = RegistryRequest::Login(key);
    let request = *app
        .account_ui
        .registry_requests
        .get(&operation)
        .expect("verify_login must capture its callback before its Task is polled");
    RegistryWrite {
        request: Some((operation, request)),
        ..app.account_ui.registry_write()
    }
}

fn login_results(key: Uuid) -> Vec<Action> {
    vec![
        Action::LoginVerified {
            key,
            outcome: ProbeOutcome::Answered {
                identity: Some(Identity {
                    email: Some("confirmed@example.invalid".into()),
                    ..Identity::default()
                }),
                snapshot: UsageSnapshot::state(UsageStatus::Ok, "synthetic login", now_ms()),
                catalog: None,
            },
        },
        Action::LoginVerified {
            key,
            outcome: ProbeOutcome::Answered {
                identity: None,
                snapshot: UsageSnapshot::state(
                    UsageStatus::NeedsLogin,
                    "synthetic signed-out result",
                    now_ms(),
                ),
                catalog: None,
            },
        },
        Action::LoginCallbackBlocked(key),
    ]
}

#[test]
fn checked_lower_recovery_rejects_old_reducer_reads_and_writes() {
    let (mut app, _directory) = fixture();
    let key = Uuid::from_u128(2118);
    app.account_ui.set_registry(registered_registry(50, key));
    let old_read = app.account_ui.registry_callbacks.issue_read();
    let old_write = app.account_ui.registry_write();
    let old_owner = app.account_ui.registry_callbacks.capture_owner();
    let candidate = Candidate {
        provider: Provider::Codex,
        key,
    };
    let owner = app.account_ui.recovery.owner();
    assert!(
        app.account_ui
            .recovery
            .replace_candidates(owner, vec![candidate.clone()])
    );
    let attempt = app
        .account_ui
        .recovery
        .next_pending(&BTreeSet::new())
        .unwrap();
    app.account_ui.last_registry_scan = Some(registry(0));
    app.account_ui.registry_reset_floor = Some(50);
    let request = Request::capture(
        attempt,
        &app.account_ui.registry_callbacks,
        app.account_ui.registry_reset_floor,
    );
    assert_eq!(request.reset_floor, Some(50));
    let restored = registered_registry(1, key);
    drop(app.update_account_ui(Action::PreservedRestored(
        request,
        Ok(Settled::Registered {
            registry: restored.clone(),
            identity: None,
            snapshot: None,
        }),
    )));
    assert_eq!(app.account_ui.registry, restored);
    assert!(app.account_ui.registry.find(key).is_some());
    assert!(!app.account_ui.registry_callbacks.accept_owner(old_owner));
    assert_eq!(
        app.account_ui.recovery.status(&candidate),
        Some(&Status::Restored)
    );

    drop(app.update_account_ui(Action::RegistryRead(
        old_read,
        Box::new(Action::RegistryLoaded(Ok(registered_registry(99, key)))),
    )));
    drop(app.update_account_ui(Action::RegistryWrite(
        old_write,
        Box::new(Action::Saved(Ok(registered_registry(100, key)))),
    )));
    assert_eq!(app.account_ui.registry, restored);
    assert!(app.cards.is_empty());

    let current = app.account_ui.registry_callbacks.issue_read();
    drop(app.update_account_ui(Action::RegistryRead(
        current,
        Box::new(Action::RegistryLoaded(Ok(registered_registry(2, key)))),
    )));
    assert_eq!(app.account_ui.registry.revision(), 2);
}

#[test]
fn only_a_read_started_at_the_current_revision_proves_a_registry_reset() {
    let (mut app, _directory) = fixture();
    app.account_ui.set_registry(registry(49));
    let old = app.account_ui.registry_callbacks.issue_read_at(49);
    let _pending = app.account_ui.registry_write_for(RegistryRequest::Link);
    let pending = app.account_ui.registry_requests.clone();
    app.account_ui.set_registry(registry(50));

    drop(app.update_account_ui(Action::RegistryRead(
        old,
        Box::new(Action::RegistryScanned {
            registry: Ok(registry(49)),
            preserved: Default::default(),
        }),
    )));
    assert_eq!(app.account_ui.registry.revision(), 50);
    assert_eq!(app.account_ui.registry_reset_floor, None);
    assert_eq!(app.account_ui.registry_callbacks.accepted, old.serial);
    assert_eq!(app.account_ui.registry_requests, pending);

    let current = app.account_ui.registry_callbacks.issue_read_at(50);
    drop(app.update_account_ui(Action::RegistryRead(
        current,
        Box::new(Action::RegistryScanned {
            registry: Ok(registry(0)),
            preserved: Default::default(),
        }),
    )));
    assert_eq!(app.account_ui.registry.revision(), 50);
    assert_eq!(app.account_ui.registry_reset_floor, Some(50));
    assert_eq!(app.account_ui.registry_requests, pending);
}

#[test]
fn a_later_read_started_before_a_ui_advance_cannot_restore_an_old_reset_floor() {
    let (mut app, _directory) = fixture();
    app.account_ui.set_registry(registry(50));
    let proof = app.account_ui.registry_callbacks.issue_read_at(50);
    drop(app.update_account_ui(Action::RegistryRead(
        proof,
        Box::new(Action::RegistryScanned {
            registry: Ok(registry(0)),
            preserved: Default::default(),
        }),
    )));
    assert_eq!(app.account_ui.registry_reset_floor, Some(50));
    let _pending = app.account_ui.registry_write_for(RegistryRequest::Link);
    let pending = app.account_ui.registry_requests.clone();

    // Both reads began while the UI knew revision 50. Deliver the newer
    // issued read last, so read ordering accepts it: the revision floor,
    // rather than an old serial, must keep it from reviving stale proof.
    let advance = app.account_ui.registry_callbacks.issue_read_at(50);
    let delayed = app.account_ui.registry_callbacks.issue_read_at(50);
    drop(app.update_account_ui(Action::RegistryRead(
        advance,
        Box::new(Action::RegistryScanned {
            registry: Ok(registry(60)),
            preserved: Default::default(),
        }),
    )));
    assert_eq!(app.account_ui.registry.revision(), 60);
    assert_eq!(app.account_ui.registry_reset_floor, None);

    drop(app.update_account_ui(Action::RegistryRead(
        delayed,
        Box::new(Action::RegistryScanned {
            registry: Ok(registry(0)),
            preserved: Default::default(),
        }),
    )));
    assert_eq!(app.account_ui.registry_callbacks.accepted, delayed.serial);
    assert_eq!(app.account_ui.registry.revision(), 60);
    assert_eq!(app.account_ui.registry_reset_floor, None);
    assert_eq!(app.account_ui.registry_requests, pending);
}

#[test]
fn stale_matching_link_retires_its_flag_and_keeps_the_entered_path() {
    let (mut app, _directory) = fixture();
    let key = Uuid::from_u128(2119);
    let input = (
        Provider::Codex,
        "private fixture path still being entered".into(),
    );
    app.account_ui.table.link = Some(input.clone());
    app.account_ui.table.linking = true;
    let callback = app.account_ui.registry_write_for(RegistryRequest::Link);
    app.account_ui
        .registry_callbacks
        .reset_after_checked_recovery();
    let before = app.account_ui.registry.clone();

    drop(app.update_account_ui(Action::RegistryWrite(
        callback,
        Box::new(Action::LinkReady {
            key,
            result: Ok(registered_registry(99, key)),
        }),
    )));

    assert!(!app.account_ui.table.linking);
    assert_eq!(app.account_ui.table.link, Some(input));
    assert_eq!(app.account_ui.registry, before);
    assert!(app.account_ui.registry_requests.is_empty());
    assert!(app.status.is_none(), "an old link cannot announce Added");
    assert!(app.notices.recent().next().is_none());
}

#[test]
fn an_old_link_cannot_retire_its_replacement_after_a_reset() {
    let (mut app, _directory) = fixture();
    let key = Uuid::from_u128(2120);
    let old = app.account_ui.registry_write_for(RegistryRequest::Link);
    app.account_ui
        .registry_callbacks
        .reset_after_checked_recovery();
    let _replacement = app.account_ui.registry_write_for(RegistryRequest::Link);
    let expected_requests = app.account_ui.registry_requests.clone();
    let input = (Provider::Codex, "replacement path draft".into());
    app.account_ui.table.link = Some(input.clone());
    app.account_ui.table.linking = true;
    let before = app.account_ui.registry.clone();

    drop(app.update_account_ui(Action::RegistryWrite(
        old,
        Box::new(Action::LinkReady {
            key,
            result: Ok(registered_registry(99, key)),
        }),
    )));

    assert!(app.account_ui.table.linking);
    assert_eq!(app.account_ui.table.link, Some(input));
    assert_eq!(app.account_ui.registry_requests, expected_requests);
    assert_eq!(app.account_ui.registry, before);
    assert!(app.status.is_none());
}

#[test]
fn stale_matching_removal_retires_only_its_account() {
    let (mut app, _directory) = fixture();
    let removed = app.account_ui.add_managed_fixture(Provider::Codex, 2121);
    let other = app.account_ui.add_managed_fixture(Provider::Codex, 2122);
    app.account_ui
        .removing
        .extend([removed.clone(), other.clone()]);
    let callback = app
        .account_ui
        .registry_write_for(RegistryRequest::Remove(removed.clone()));
    let _other_callback = app
        .account_ui
        .registry_write_for(RegistryRequest::Remove(other.clone()));
    app.account_ui
        .registry_callbacks
        .reset_after_checked_recovery();
    let before = app.account_ui.registry.clone();

    drop(app.update_account_ui(Action::RegistryWrite(
        callback,
        Box::new(Action::Removed {
            id: removed.clone(),
            registry: Some(registry(99)),
            error: None,
        }),
    )));

    assert!(!app.account_ui.removing.contains(&removed));
    assert!(app.account_ui.removing.contains(&other));
    assert!(
        !app.account_ui
            .registry_requests
            .contains_key(&RegistryRequest::Remove(removed))
    );
    assert!(
        app.account_ui
            .registry_requests
            .contains_key(&RegistryRequest::Remove(other))
    );
    assert_eq!(app.account_ui.registry, before);
    assert!(app.status.is_none());
}

#[test]
fn stale_login_results_return_only_their_own_login_to_waiting() {
    let key = Uuid::from_u128(2123);
    for response in login_results(key) {
        let (mut app, _directory) = fixture();
        login(&mut app, key);
        let other = Uuid::from_u128(2124);
        login(&mut app, other);
        let callback = start_verification(&mut app, key);
        let _other_callback = app
            .account_ui
            .registry_write_for(RegistryRequest::Login(other));
        let other_request = app.account_ui.registry_requests[&RegistryRequest::Login(other)];
        app.account_ui
            .registry_callbacks
            .reset_after_checked_recovery();
        let before = app.account_ui.registry.clone();
        let before_check = now_ms();

        drop(app.update_account_ui(Action::RegistryWrite(callback, Box::new(response))));

        let current = app
            .account_ui
            .logins
            .iter()
            .find(|login| login.key == key)
            .unwrap();
        assert_eq!(current.phase, LoginPhase::Waiting);
        assert_eq!(current.attempts, 2);
        assert!(current.next_check >= before_check + super::super::LOGIN_RETRY);
        assert_eq!(app.account_ui.logins.len(), 2);
        let unaffected = app
            .account_ui
            .logins
            .iter()
            .find(|login| login.key == other)
            .unwrap();
        assert_eq!(unaffected.phase, LoginPhase::Verifying);
        assert_eq!(unaffected.next_check, 17);
        assert_eq!(app.account_ui.registry, before);
        assert!(app.account_ui.registry.find(key).is_none());
        assert!(
            app.account_ui
                .registry
                .pending()
                .iter()
                .any(|pending| pending.key == key)
        );
        assert!(
            !app.account_ui
                .registry_requests
                .contains_key(&RegistryRequest::Login(key))
        );
        assert_eq!(
            app.account_ui.registry_requests[&RegistryRequest::Login(other)],
            other_request
        );
        assert!(app.status.is_none());
    }
}

#[test]
fn old_login_results_leave_a_replacement_verification_unchanged() {
    let key = Uuid::from_u128(2125);
    for response in login_results(key) {
        let (mut app, _directory) = fixture();
        login(&mut app, key);
        let old = start_verification(&mut app, key);
        app.account_ui
            .registry_callbacks
            .reset_after_checked_recovery();
        let _replacement = start_verification(&mut app, key);
        let expected_requests = app.account_ui.registry_requests.clone();
        let before = app.account_ui.registry.clone();

        drop(app.update_account_ui(Action::RegistryWrite(old, Box::new(response))));

        assert_eq!(app.account_ui.registry_requests, expected_requests);
        assert_eq!(app.account_ui.registry, before);
        assert_eq!(app.account_ui.logins.len(), 1);
        assert_eq!(app.account_ui.logins[0].phase, LoginPhase::Verifying);
        assert_eq!(app.account_ui.logins[0].next_check, 17);
        assert_eq!(app.account_ui.logins[0].attempts, 3);
        assert!(app.status.is_none());
    }
}

#[test]
fn a_foreign_state_cannot_retire_any_current_operation() {
    let (mut foreign, _foreign_directory) = fixture();
    let (mut app, _directory) = fixture();
    let key = Uuid::from_u128(2126);
    login(&mut app, key);
    let removed = app.account_ui.add_managed_fixture(Provider::Codex, 2127);
    let input = (Provider::Codex, "current device path draft".into());
    app.account_ui.table.link = Some(input.clone());
    app.account_ui.table.linking = true;
    app.account_ui.removing.insert(removed.clone());
    let operations = [
        (
            RegistryRequest::Link,
            Action::LinkReady {
                key,
                result: Ok(registered_registry(99, key)),
            },
        ),
        (
            RegistryRequest::Remove(removed.clone()),
            Action::Removed {
                id: removed.clone(),
                registry: Some(registry(99)),
                error: None,
            },
        ),
        (RegistryRequest::Login(key), login_results(key).remove(0)),
    ];
    for (operation, _) in &operations {
        let _current = app.account_ui.registry_write_for(operation.clone());
    }
    let expected_requests = app.account_ui.registry_requests.clone();
    let before = app.account_ui.registry.clone();
    assert_ne!(
        foreign.account_ui.recovery.owner(),
        app.account_ui.recovery.owner()
    );

    for (operation, response) in operations {
        let mut callback = foreign.account_ui.registry_write_for(operation.clone());
        // An otherwise matching request still cannot retire a current latch
        // when the callback belongs to another app/device State. Matching
        // this field makes the test detect destructively checking State
        // only after finish_registry_write has removed the current request.
        callback.request = Some((operation.clone(), expected_requests[&operation]));
        drop(app.update_account_ui(Action::RegistryWrite(callback, Box::new(response))));
    }

    assert!(app.account_ui.table.linking);
    assert_eq!(app.account_ui.table.link, Some(input));
    assert!(app.account_ui.removing.contains(&removed));
    assert_eq!(app.account_ui.registry_requests, expected_requests);
    assert_eq!(app.account_ui.registry, before);
    assert_eq!(app.account_ui.logins[0].phase, LoginPhase::Verifying);
    assert_eq!(app.account_ui.logins[0].next_check, 17);
    assert!(app.status.is_none());
}
