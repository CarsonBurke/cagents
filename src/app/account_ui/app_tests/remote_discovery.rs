use super::*;

fn remote_snapshot(
    app: &Cagents,
    installed: BTreeMap<Provider, bool>,
) -> crate::device_service::AccountSnapshot {
    crate::device_service::AccountSnapshot {
        registry: app.account_ui.registry.clone(),
        homes: None,
        system_homes: app.account_ui.remote_system_homes.clone(),
        entries: BTreeMap::new(),
        cache_keys: BTreeMap::new(),
        catalogs: BTreeMap::new(),
        installed,
    }
}

fn cached_account(app: &mut Cagents, provider: Provider) -> AccountId {
    let id = Account::system(provider).id();
    let state = app.account_ui.account_mut(&id).unwrap();
    state.present = true;
    state.identity = Some(Identity {
        email: Some("remembered@fixture.invalid".into()),
        ..Default::default()
    });
    state.snapshot = Some(UsageSnapshot::state(
        UsageStatus::Ok,
        "retained history",
        now_ms(),
    ));
    state.schedule.observe(now_ms());
    state.schedule.fail(now_ms());
    id
}

#[test]
fn remote_current_presence_changes_without_replacing_existing_usage_or_identity() {
    let mut app = context_inspector_test_app(false);
    app.refresh_ui.focused = false;
    let id = cached_account(&mut app, Provider::Codex);
    app.account_ui.installed.insert(Provider::Codex, true);
    app.account_ui.popover = Some(id.clone());
    let state = app.account_ui.account(&id).unwrap();
    let snapshot = state.snapshot.clone();
    let identity = state.identity.clone();
    let schedule = state.schedule.clone();
    let generation = app.account_ui.discovery_generation;
    let absent = remote_snapshot(&app, BTreeMap::from([(Provider::Codex, false)]));
    let task = app.update(Message::AccountUi(Action::RemoteLoaded(
        generation,
        Ok(absent),
    )));
    assert_eq!(task.units(), 0);
    drop(task);
    let state = app.account_ui.account(&id).unwrap();
    assert!(
        !state.present,
        "current remote absence must override presence despite retained usage"
    );
    assert_eq!(state.snapshot, snapshot);
    assert_eq!(state.identity, identity);
    assert_eq!(state.schedule, schedule);
    assert_eq!(state.failure.as_deref(), Some("Codex is not installed"));
    assert!(app.account_ui.popover.is_none());
    assert_eq!(app.account_ui.installed.get(&Provider::Codex), Some(&false));

    let mut returned = remote_snapshot(&app, BTreeMap::from([(Provider::Codex, true)]));
    returned
        .cache_keys
        .insert(id.clone(), "remote-cache-key".into());
    returned.entries.insert(
        "remote-cache-key".into(),
        Entry {
            identity: Some(Identity {
                email: Some("cached@fixture.invalid".into()),
                ..Default::default()
            }),
            snapshot: UsageSnapshot::state(
                UsageStatus::NeedsLogin,
                "cache must not replace live history",
                now_ms(),
            ),
        },
    );
    drop(app.update(Message::AccountUi(Action::RemoteLoaded(
        generation,
        Ok(returned),
    ))));
    let state = app.account_ui.account(&id).unwrap();
    assert!(state.present);
    assert!(state.failure.is_none());
    assert_eq!(state.snapshot, snapshot);
    assert_eq!(state.identity, identity);
    assert_eq!(state.schedule.failures(), 0);
    assert!(
        state.schedule.should_probe(Trigger::Focus, now_ms(), true),
        "returning CLI invalidates old observation and backoff"
    );
    assert!(!state.schedule.in_flight(now_ms()));

    // Discovery must clear only its own missing-CLI failure.
    app.account_ui.account_mut(&id).unwrap().failure = Some("a different probe problem".into());
    let current = remote_snapshot(&app, BTreeMap::from([(Provider::Codex, true)]));
    drop(app.update(Message::AccountUi(Action::RemoteLoaded(
        generation,
        Ok(current),
    ))));
    assert_eq!(
        app.account_ui.account(&id).unwrap().failure.as_deref(),
        Some("a different probe problem")
    );
}

#[test]
fn remote_explicit_refresh_waits_for_current_read_and_overrides_cache_and_backoff() {
    let mut app = context_inspector_test_app(false);
    app.refresh_ui.focused = false; // A Focus continuation cannot explain Manual.
    let ids = [
        cached_account(&mut app, Provider::Codex),
        cached_account(&mut app, Provider::Claude),
    ];
    app.account_ui.installed =
        BTreeMap::from([(Provider::Codex, false), (Provider::Claude, false)]);
    for id in &ids {
        let state = app.account_ui.account(id).unwrap();
        assert!(!state.schedule.should_probe(Trigger::Focus, now_ms(), true));
        let owner = state.owner;
        // The exact helper called by remote Action::Refresh: creates the
        // discovery Task and captures intent, never an immediate old-state probe.
        let read = app.read_remote_accounts(Some((id.clone(), owner)));
        assert_eq!(read.units(), 1);
        drop(read); // No remote RPC, local provider, or device switch is executed.
    }
    assert_eq!(app.account_ui.discovery_manual.len(), 2);
    let generation = app.account_ui.discovery_generation;
    let stale = remote_snapshot(&app, BTreeMap::from([(Provider::Codex, true)]));
    let ignored = app.update(Message::AccountUi(Action::RemoteLoaded(
        generation - 1,
        Ok(stale),
    )));
    assert_eq!(ignored.units(), 0);
    drop(ignored);
    assert_eq!(app.account_ui.discovery_manual.len(), 2);
    assert!(ids.iter().all(|id| {
        !app.account_ui
            .account(id)
            .unwrap()
            .schedule
            .in_flight(now_ms())
    }));
    let current = remote_snapshot(
        &app,
        BTreeMap::from([(Provider::Codex, true), (Provider::Claude, true)]),
    );
    let continuation = app.update(Message::AccountUi(Action::RemoteLoaded(
        generation,
        Ok(current),
    )));
    assert!(continuation.units() > 0);
    drop(continuation); // Assert scheduling only; never poll provider or timer Tasks.
    assert!(app.account_ui.discovery_manual.is_empty());
    for id in &ids {
        let state = app.account_ui.account(id).unwrap();
        assert!(state.schedule.in_flight(now_ms()));
        assert!(
            !state.schedule.manual_ready(app.account_ui.now_ms),
            "admitted continuation must spend the explicit Manual, not Focus"
        );
        assert_eq!(
            state.snapshot.as_ref().unwrap().detail.as_deref(),
            Some("retained history")
        );
    }
}

#[test]
fn remote_refusal_retains_manual_intent_and_changed_owner_cannot_inherit_it() {
    for changed_owner in [false, true] {
        let mut app = context_inspector_test_app(false);
        app.refresh_ui.focused = false;
        let id = cached_account(&mut app, Provider::Codex);
        app.account_ui.installed.insert(Provider::Codex, false);
        let owner = app.account_ui.probe_owner(&id).unwrap();
        let authority = crate::agent_execution::Authority::session("fixture-scope".into());
        app.account_ui.authority = authority.clone();
        drop(app.read_remote_accounts(Some((id.clone(), owner))));
        let generation = app.account_ui.discovery_generation;
        let refused = app.update(Message::AccountUi(Action::RemoteLoaded(
            generation,
            Err("fixture scoped remote refusal".into()),
        )));
        assert_eq!(refused.units(), 0);
        drop(refused);
        assert_eq!(app.account_ui.authority, authority);
        assert_eq!(app.account_ui.discovery_manual.get(&id), Some(&owner));
        assert!(
            app.account_ui
                .account(&id)
                .unwrap()
                .schedule
                .manual_ready(now_ms())
        );
        assert!(
            !app.account_ui
                .account(&id)
                .unwrap()
                .schedule
                .in_flight(now_ms())
        );

        // An ordinary later discovery retains the pending explicit intent; a
        // refusal itself does not auto-replay or spend it.
        drop(app.read_remote_accounts(None));
        let generation = app.account_ui.discovery_generation;
        let mut changed = remote_snapshot(&app, BTreeMap::from([(Provider::Codex, true)]));
        if changed_owner {
            changed.system_homes.insert(
                Provider::Codex,
                "/cagents-fixture/replacement-remote-codex".into(),
            );
        }
        let task = app.update(Message::AccountUi(Action::RemoteLoaded(
            generation,
            Ok(changed),
        )));
        if !changed_owner {
            assert!(task.units() > 0);
            drop(task);
            let state = app.account_ui.account(&id).unwrap();
            assert_eq!(state.owner, owner);
            assert!(state.schedule.in_flight(now_ms()));
            assert!(!state.schedule.manual_ready(app.account_ui.now_ms));
            assert!(app.account_ui.discovery_manual.is_empty());
            continue;
        }
        assert_eq!(task.units(), 0);
        drop(task);
        let state = app.account_ui.account(&id).unwrap();
        assert_ne!(state.owner, owner);
        assert!(state.snapshot.is_none());
        assert!(state.schedule.manual_ready(now_ms()));
        assert!(!state.schedule.in_flight(now_ms()));
        assert!(
            app.account_ui.discovery_manual.is_empty(),
            "replacement home cannot inherit original owner's Manual"
        );

        // No queued intent means this accepted read is only automatic Focus;
        // neither a blanket Manual continuation nor failed-read replay can pass.
        let current = remote_snapshot(&app, BTreeMap::from([(Provider::Codex, true)]));
        let task = app.update(Message::AccountUi(Action::RemoteLoaded(
            generation,
            Ok(current),
        )));
        assert_eq!(task.units(), 0);
        drop(task);
        assert!(
            app.account_ui
                .account(&id)
                .unwrap()
                .schedule
                .manual_ready(now_ms())
        );
    }
}

#[test]
fn an_older_remote_reply_cannot_apply_home_a_cache_to_a_newer_home_b() {
    for via_saved in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let home_a = root.path().join("home-a");
        let home_b = root.path().join("home-b");
        std::fs::create_dir(&home_a).unwrap();
        std::fs::create_dir(&home_b).unwrap();
        let store = Store::at(root.path().join("registry"));
        let key = uuid::Uuid::from_u128(153366);
        let (registry_a, _) = store
            .update(|registry| {
                registry
                    .link(Provider::Codex, key, home_a.clone(), None, None, 1)
                    .unwrap()
            })
            .unwrap();
        let mut app = context_inspector_test_app(false);
        app.refresh_ui.focused = false;
        drop(app.update(Message::AccountUi(Action::RegistryLoaded(Ok(
            registry_a.clone(),
        )))));
        let id = Account::linked(Provider::Codex, key, home_a.clone()).id();
        let owner_a = app.account_ui.probe_owner(&id).unwrap();
        let system = Account::system(Provider::Codex).id();
        let system_owner = app.account_ui.probe_owner(&system).unwrap();
        let current_homes = Some(Homes::at(root.path().join("current-managed")));
        app.account_ui.homes = current_homes.clone();
        let current_system_homes = app.account_ui.remote_system_homes.clone();
        app.account_ui.installed.insert(Provider::Codex, false);
        // Two real production read admissions capture both original owners;
        // their Tasks remain unpolled, as do all discovery Tasks in this test.
        drop(app.read_remote_accounts(Some((id.clone(), owner_a))));
        drop(app.read_remote_accounts(Some((system.clone(), system_owner))));
        let held_generation = app.account_ui.discovery_generation;
        let mut held_a = remote_snapshot(&app, BTreeMap::from([(Provider::Codex, true)]));
        held_a.homes = Some(Homes::at(root.path().join("old-managed")));
        held_a
            .system_homes
            .insert(Provider::Codex, root.path().join("old-system"));
        held_a.catalogs.insert(
            Provider::Codex,
            Catalog {
                models: vec![],
                observed_at: now_ms(),
            },
        );
        held_a.cache_keys.insert(id.clone(), "home-a-key".into());
        held_a.entries.insert(
            "home-a-key".into(),
            Entry {
                identity: Some(Identity {
                    email: Some("home-a@fixture.invalid".into()),
                    ..Default::default()
                }),
                snapshot: UsageSnapshot::state(UsageStatus::Ok, "home A usage", now_ms()),
            },
        );
        let (registry_b, _) = store
            .update(|registry| {
                registry.unregister(key).unwrap();
                registry
                    .link(Provider::Codex, key, home_b.clone(), None, None, 2)
                    .unwrap()
            })
            .unwrap();
        assert!(registry_b.revision() > registry_a.revision());
        let adopted = if via_saved {
            // A whole Store save can include another window's replacement.
            // Unlike RegistryLoaded, this real callback does not rediscover.
            Action::Saved(Ok(registry_b.clone()))
        } else {
            Action::RegistryLoaded(Ok(registry_b.clone()))
        };
        drop(app.update(Message::AccountUi(adopted)));
        if via_saved {
            assert_eq!(
                app.account_ui.discovery_generation, held_generation,
                "Saved really leaves the original remote read current"
            );
        } else {
            // Existing generation protection already rejects this variant;
            // never relabel its held original reply as a current generation.
            assert!(app.account_ui.discovery_generation > held_generation);
        }
        let owner_b = app.account_ui.probe_owner(&id).unwrap();
        assert_ne!(owner_b, owner_a);
        let state = app.account_ui.account(&id).unwrap();
        assert_eq!(state.account.resolved_home(), Some(home_b.clone()));
        assert!(state.snapshot.is_none());
        assert!(state.identity.is_none());
        assert!(!app.account_ui.discovery_manual.contains_key(&id));
        assert_eq!(
            app.account_ui.discovery_manual.get(&system),
            Some(&system_owner)
        );

        let rejected = app.update(Message::AccountUi(Action::RemoteLoaded(
            held_generation,
            Ok(held_a),
        )));
        assert_eq!(rejected.units(), 0);
        drop(rejected);
        let state = app.account_ui.account(&id).unwrap();
        assert!(
            state.identity.is_none(),
            "an old remote reply must not assign home A identity to home B"
        );
        assert!(
            state.snapshot.is_none(),
            "an old remote reply must not assign home A usage to home B"
        );
        assert_eq!(state.owner, owner_b);
        assert_eq!(state.account.resolved_home(), Some(home_b));
        assert_eq!(app.account_ui.registry, registry_b);
        assert_eq!(app.account_ui.homes, current_homes);
        assert_eq!(app.account_ui.remote_system_homes, current_system_homes);
        assert_eq!(app.account_ui.installed.get(&Provider::Codex), Some(&false));
        assert!(app.account_ui.catalog(Provider::Codex).is_none());
        assert_eq!(
            app.account_ui.discovery_manual.get(&system),
            Some(&system_owner)
        );
        assert!(
            app.account_ui
                .account(&system)
                .unwrap()
                .schedule
                .manual_ready(now_ms())
        );

        // No hot retry is generated. An ordinary later read can still accept
        // equal/current registry revision and restore B's distinct cache data.
        drop(app.read_remote_accounts(None));
        let generation = app.account_ui.discovery_generation;
        let mut fresh_b = remote_snapshot(&app, BTreeMap::from([(Provider::Codex, true)]));
        fresh_b.homes = current_homes;
        fresh_b.cache_keys.insert(id.clone(), "home-b-key".into());
        fresh_b.entries.insert(
            "home-b-key".into(),
            Entry {
                identity: Some(Identity {
                    email: Some("home-b@fixture.invalid".into()),
                    ..Default::default()
                }),
                snapshot: UsageSnapshot::state(UsageStatus::Ok, "home B usage", now_ms()),
            },
        );
        let accepted = app.update(Message::AccountUi(Action::RemoteLoaded(
            generation,
            Ok(fresh_b),
        )));
        assert!(accepted.units() > 0); // Retained same-owner System Manual.
        drop(accepted);
        let state = app.account_ui.account(&id).unwrap();
        assert_eq!(state.owner, owner_b);
        assert_eq!(
            state.identity.as_ref().unwrap().email.as_deref(),
            Some("home-b@fixture.invalid")
        );
        assert_eq!(
            state.snapshot.as_ref().unwrap().detail.as_deref(),
            Some("home B usage")
        );
        assert!(app.account_ui.discovery_manual.is_empty());
        assert!(
            !app.account_ui
                .account(&system)
                .unwrap()
                .schedule
                .manual_ready(app.account_ui.now_ms)
        );
    }
}
