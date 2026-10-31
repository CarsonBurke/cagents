//! Actual settings-writer contention through the broker's existing admission
//! and recovered-precheck routes. The mixed-owner regression also retires one
//! real offline provider; durable precheck receipts remain no-process fixtures.
use super::*;
use crate::{
    agent_execution::{ConcurrencyLimits, Destination, Origin, Spec},
    automations::{Automation, Definition, Precheck, Schedule, SchedulePattern, SkipReason},
    host::{agent_execution::tests::isolated, automations::precheck::Outcome},
};
use std::{
    fs::OpenOptions,
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt},
    },
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

fn create(host: &SharedHost, spec: &Spec, name: &str, precheck: bool) -> Automation {
    service::create(
        host,
        Definition {
            name: name.into(),
            prompt: "Read the owned contention fixture".into(),
            agent: spec.clone(),
            schedule: Schedule {
                pattern: SchedulePattern::Cron {
                    expression: "* * * * *".into(),
                },
                timezone: "UTC".into(),
            },
            session_mode: SessionMode::Fresh,
            precheck: precheck.then(|| Precheck {
                command: "true".into(),
                timeout_seconds: 1,
            }),
        },
    )
    .unwrap()
}

fn claim(handle: &Handle, automation: &Automation, request: Uuid) -> Claimed {
    claim_at(handle, automation, request, service::now().unwrap())
}

fn claim_at(handle: &Handle, automation: &Automation, request: Uuid, now: u64) -> Claimed {
    let limits = agent_execution::concurrency_limits();
    handle
        .0
        .registry
        .lock()
        .unwrap()
        .claim_automation(
            automation.id,
            automation.execution_revision,
            None,
            Trigger::Manual { request },
            None,
            true,
            limits,
            now,
        )
        .unwrap()
}

fn receipt(root: &Path, firing: &Firing, outcome: Outcome) {
    // Durable fixture receipt, read by the real Worker::recover/status path.
    // It proves this test's no-process owner; it is not shell/native evidence.
    let directory = root.join(firing.id.to_string());
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .unwrap();
    for (name, bytes) in [
        ("lease", Vec::new()),
        (
            "receipt",
            serde_json::to_vec(&serde_json::json!({
                "firing": firing.id, "revision": firing.revision, "outcome": outcome,
            }))
            .unwrap(),
        ),
    ] {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join(name))
            .unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
    }
}

#[test]
fn held_settings_writer_keeps_broker_cleanup_retry_and_registry_available_without_new_acceptance() {
    if isolated("host::automations::broker::policy_contention_tests::held_settings_writer_keeps_broker_cleanup_retry_and_registry_available_without_new_acceptance").is_some() {
        return;
    }
    let (host, request, database) = agent_execution::tests::fixture();
    let Destination::New(spec) = request.destination else {
        unreachable!()
    };
    let fixture_root = database.parent().unwrap();
    let settings = crate::settings::default_path().unwrap();
    assert!(settings.starts_with(fixture_root));
    let configured = ConcurrencyLimits {
        caller: 16,
        global: 16,
    };
    crate::settings::save(
        &settings,
        &crate::settings::AppSettings {
            agent_concurrency: configured,
            ..Default::default()
        },
    )
    .unwrap();
    let registry = agent_execution::registry(&host).unwrap();
    let root = fixture_root.join("prechecks");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .unwrap();
    let handle = Handle(Arc::new(Inner {
        host: Arc::downgrade(&host),
        registry: registry.clone(),
        root: root.clone(),
        wake: Wake::default(),
        workers: Mutex::default(),
        cleanup: Mutex::new(None),
        attention: Mutex::default(),
        notifications: Mutex::default(),
    }));
    let original = create(&host, &spec, "Original reservation", false);
    let original_request = Uuid::new_v4();
    let original_claim = claim(&handle, &original, original_request);
    let original_accepted = original_claim.accepted.clone().unwrap();
    let original_firing_id = original_claim.firing.id;
    let original_agent_id = original_accepted.agent.id;
    let original_run_id = original_accepted.run.id;
    let stopped = create(&host, &spec, "Stopped owner", true);
    let stopped_claim = claim(&handle, &stopped, Uuid::new_v4());
    receipt(
        &root,
        &stopped_claim.firing,
        Outcome::Stopped {
            state: FiringState::Cancelled,
            skip: None,
            diagnostic: "proved fixture cleanup".into(),
        },
    );
    let interrupted = create(&host, &spec, "Interrupted owner", true);
    let interrupted_claim = claim(&handle, &interrupted, Uuid::new_v4());
    registry
        .lock()
        .unwrap()
        .interrupt_automation_precheck(
            interrupted_claim.firing.id,
            "exact cleanup receipt pending".into(),
            service::now().unwrap(),
        )
        .unwrap();
    let interrupted_firing = registry
        .lock()
        .unwrap()
        .automation_firing(interrupted_claim.firing.id)
        .unwrap();
    receipt(&root, &interrupted_firing, Outcome::Passed);
    let fresh = create(&host, &spec, "New manual admission", false);
    let fresh_request = Uuid::new_v4();
    let scheduled = create(&host, &spec, "New scheduled admission", false);
    let now = service::now().unwrap();
    let due = Compiled::new(&scheduled.definition.schedule)
        .unwrap()
        .next_after(now.saturating_sub(60_001))
        .unwrap();
    assert!(due.due_ms <= now);
    let scheduled = registry
        .lock()
        .unwrap()
        .set_automation_enabled(scheduled.id, scheduled.revision, Some(due), true, now)
        .unwrap();

    // This is the actual lock retained by settings::save/save_merged, already
    // created and validated by the real save above. Different open file
    // descriptions contend even within this isolated process.
    let writer = OpenOptions::new()
        .read(true)
        .write(true)
        .open(settings.with_extension("toml.lock"))
        .unwrap();
    // SAFETY: writer owns this live fixture descriptor until explicitly dropped.
    assert_eq!(
        unsafe { libc::flock(writer.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let held = Arc::new(AtomicBool::new(true));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker_handle = handle.clone();
    let worker_host = host.clone();
    let worker_held = held.clone();
    let worker_stopped = stopped_claim.firing.clone();
    let worker_original = original.clone();
    let worker_fresh = fresh.clone();
    let worker_scheduled = scheduled.clone();
    let worker_spec = spec.clone();
    let worker = std::thread::spawn(move || {
        let control = worker_host.lock().unwrap().control_gate.clone();
        {
            let _admission = control.read().unwrap();
            entered_tx.send(()).unwrap();
            worker_handle
                .observe_precheck(&worker_host, &worker_stopped, None, service::now().unwrap())
                .unwrap();
            // On a blocking old-code control, the main thread releases the
            // writer before joining. Never continue into new work afterward.
            if !worker_held.load(Ordering::Acquire) {
                return;
            }
            worker_handle
                .observe_precheck(
                    &worker_host,
                    &interrupted_firing,
                    None,
                    service::now().unwrap(),
                )
                .unwrap();
            if !worker_held.load(Ordering::Acquire) {
                return;
            }
            let retry = worker_handle
                .run_now(
                    worker_original.id,
                    worker_original.execution_revision,
                    original_request,
                    true,
                )
                .unwrap();
            assert_eq!(retry.id, original_firing_id);
            assert_eq!(retry.agent, Some(original_agent_id));
            assert_eq!(retry.run, Some(original_run_id));
            let error = worker_handle
                .run_now(
                    worker_fresh.id,
                    worker_fresh.execution_revision,
                    fresh_request,
                    true,
                )
                .unwrap_err();
            assert!(
                error.contains("Cannot read agent concurrency settings"),
                "{error}"
            );
        }
        if !worker_held.load(Ordering::Acquire) {
            return;
        }
        // Cleanup has settled first, so the real tick reaches its due-claim
        // path rather than an unrelated pending precheck's admission branch.
        let error = worker_handle.tick_under_control(&worker_host).unwrap_err();
        assert!(
            error.contains("Cannot read agent concurrency settings"),
            "{error}"
        );
        assert!(
            worker_handle
                .0
                .registry
                .lock()
                .unwrap()
                .automation(worker_scheduled.id)
                .unwrap()
                .active_firing
                .is_none()
        );
        if !worker_held.load(Ordering::Acquire) {
            return;
        }
        let passing = create(
            &worker_host,
            &worker_spec,
            "Passing precheck admission",
            true,
        );
        let passing_claim = claim(&worker_handle, &passing, Uuid::new_v4());
        assert!(
            passing_claim.accepted.is_none(),
            "a bounded precheck claim is not an agent acceptance"
        );
        receipt(
            &worker_handle.0.root,
            &passing_claim.firing,
            Outcome::Passed,
        );
        let _admission = control.read().unwrap();
        let error = worker_handle
            .observe_precheck(
                &worker_host,
                &passing_claim.firing,
                None,
                service::now().unwrap(),
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Cannot read agent concurrency settings"),
            "{error}"
        );
        let store = worker_handle.0.registry.lock().unwrap();
        assert_eq!(
            store.automation_firing(passing_claim.firing.id).unwrap(),
            passing_claim.firing,
            "unaccepted success receipt must retain its exact original owner"
        );
        let counts = store
            .agent_capacity(
                &Origin::Automation {
                    id: worker_original.id,
                },
                configured,
            )
            .unwrap();
        assert_eq!(
            counts.global.running, 1,
            "busy policy cannot fall back to permissive defaults"
        );
        assert_eq!(counts.global.holders[0].run, original_run_id);
        done_tx.send(()).unwrap();
    });
    let entered = entered_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    let (read_tx, read_rx) = mpsc::channel();
    let read_registry = registry.clone();
    let reader = std::thread::spawn(move || {
        let result = read_registry.lock().unwrap().model();
        let _ = read_tx.send(result);
    });
    let read_while_held = read_rx.recv_timeout(Duration::from_secs(2));
    let completed_while_held = done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    // Always unblock and join both threads before asserting timing. This test
    // deliberately fails a blocking control without leaving an orphan worker.
    held.store(false, Ordering::Release);
    drop(writer);
    let worker_result = worker.join();
    let reader_result = reader.join();
    assert!(entered, "broker worker did not enter");
    assert!(
        read_while_held.is_ok(),
        "shared Registry was held behind the settings writer"
    );
    assert!(read_while_held.unwrap().is_ok());
    assert!(
        completed_while_held,
        "broker cleanup/admission waited for the held settings writer"
    );
    worker_result.unwrap();
    reader_result.unwrap();
    let store = registry.lock().unwrap();
    let stopped = store.automation_firing(stopped_claim.firing.id).unwrap();
    assert_eq!(stopped.state, FiringState::Cancelled);
    assert!(stopped.agent.is_none() && stopped.run.is_none());
    assert!(
        store
            .automation(stopped.automation)
            .unwrap()
            .active_firing
            .is_none()
    );
    let recovered = store.automation(interrupted.id).unwrap();
    assert!(recovered.active_firing.is_none() && recovered.blocked.is_none());
    assert!(!recovered.enabled);
    let interrupted = store
        .automation_firing(interrupted_claim.firing.id)
        .unwrap();
    assert_eq!(interrupted.state, FiringState::Interrupted);
    assert!(interrupted.agent.is_none() && interrupted.run.is_none());
    assert_eq!(store.automation(fresh.id).unwrap(), fresh);
    assert!(
        store
            .previous_manual_automation_firing(fresh.id, fresh.execution_revision, fresh_request)
            .unwrap()
            .is_none()
    );
    drop(store);

    // After release, an explicitly retried request uses the saved16 policy.
    // These are durable reservations, not16 providers or a genuine User50 run.
    assert_eq!(agent_execution::concurrency_limits().unwrap(), configured);
    for index in 0..15 {
        let (automation, request) = if index == 0 {
            (fresh.clone(), fresh_request)
        } else {
            (
                create(&host, &spec, &format!("Saved16 reservation {index}"), false),
                Uuid::new_v4(),
            )
        };
        assert!(claim(&handle, &automation, request).accepted.is_some());
    }
    let extra = create(&host, &spec, "Beyond saved16", false);
    let refused = claim(&handle, &extra, Uuid::new_v4());
    assert!(refused.accepted.is_none());
    assert_eq!(refused.firing.skip, Some(SkipReason::Capacity));
    let counts = registry
        .lock()
        .unwrap()
        .agent_capacity(&Origin::Automation { id: original.id }, configured)
        .unwrap();
    assert_eq!(counts.global.running, 16);
    assert_eq!(counts.global.limit, 16);
    assert_eq!(counts.caller.limit, 16);
    assert!(
        counts
            .global
            .holders
            .iter()
            .any(|holder| holder.run == original_run_id)
    );
    assert!(
        !fixture_root.join("spawns").exists(),
        "no provider was dispatched"
    );
}

#[test]
fn oldest_passed_policy_refusal_still_settles_later_owners_and_retires_fresh_provider_in_same_tick()
{
    if isolated("host::automations::broker::policy_contention_tests::oldest_passed_policy_refusal_still_settles_later_owners_and_retires_fresh_provider_in_same_tick").is_some() {
        return;
    }
    mixed_owner_tick(false);
}

#[test]
fn later_cleanup_storage_failure_remains_fatal_after_oldest_policy_refusal() {
    if isolated("host::automations::broker::policy_contention_tests::later_cleanup_storage_failure_remains_fatal_after_oldest_policy_refusal").is_some() {
        return;
    }
    mixed_owner_tick(true);
}

/// Both cases use one actual tick. The second injects a real transaction
/// failure, so deferring policy cannot accidentally hide an unrelated error.
fn mixed_owner_tick(refuse_cleanup: bool) {
    let (host, request, database) = agent_execution::tests::fixture();
    let Destination::New(spec) = request.destination else {
        unreachable!()
    };
    let fixture_root = database.parent().unwrap();
    let settings = crate::settings::default_path().unwrap();
    assert!(settings.starts_with(fixture_root));
    let configured = ConcurrencyLimits {
        caller: 16,
        global: 16,
    };
    crate::settings::save(
        &settings,
        &crate::settings::AppSettings {
            agent_concurrency: configured,
            ..Default::default()
        },
    )
    .unwrap();
    let registry = agent_execution::registry(&host).unwrap();
    let root = fixture_root.join("prechecks");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .unwrap();
    let handle = Handle(Arc::new(Inner {
        host: Arc::downgrade(&host),
        registry: registry.clone(),
        root: root.clone(),
        wake: Wake::default(),
        workers: Mutex::default(),
        cleanup: Mutex::new(None),
        attention: Mutex::default(),
        notifications: Mutex::default(),
    }));
    let now = service::now().unwrap();
    let first = now.saturating_sub(10_000);
    let passed = create(&host, &spec, "Oldest passed owner", true);
    let passed_claim = claim_at(&handle, &passed, Uuid::new_v4(), first);
    receipt(&root, &passed_claim.firing, Outcome::Passed);
    let stopped = create(&host, &spec, "Later proved stopped owner", true);
    let stopped_claim = claim_at(&handle, &stopped, Uuid::new_v4(), first + 1);
    receipt(
        &root,
        &stopped_claim.firing,
        Outcome::Stopped {
            state: FiringState::Cancelled,
            skip: None,
            diagnostic: "proved fixture cleanup".into(),
        },
    );
    let interrupted = create(&host, &spec, "Later interrupted owner", true);
    let interrupted_claim = claim_at(&handle, &interrupted, Uuid::new_v4(), first + 2);
    registry
        .lock()
        .unwrap()
        .interrupt_automation_precheck(
            interrupted_claim.firing.id,
            "exact cleanup receipt pending".into(),
            now,
        )
        .unwrap();
    let interrupted_before = registry.lock().unwrap().automation(interrupted.id).unwrap();
    let interrupted_firing = registry
        .lock()
        .unwrap()
        .automation_firing(interrupted_claim.firing.id)
        .unwrap();
    receipt(&root, &interrupted_firing, Outcome::Passed);
    let fresh = create(&host, &spec, "Later fresh offline provider", false);
    let fresh_claim = claim_at(&handle, &fresh, Uuid::new_v4(), first + 3);
    let accepted = fresh_claim.accepted.clone().unwrap();
    agent_execution::dispatch_accepted(&host, accepted.clone()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let (finished, provider) = loop {
        let (run, session) = {
            let store = registry.lock().unwrap();
            (
                store.agent_run(accepted.run.id).unwrap(),
                store.agent(accepted.agent.id).unwrap().session,
            )
        };
        if let Some(session) = session {
            assert!(matches!(session, crate::model::HostedSession::Chat(_)));
            let provider = host.lock().unwrap().chats[&session.id()].handle.clone();
            if run.state == crate::agent_execution::State::Succeeded
                && provider.snapshot(None).is_some_and(|snapshot| {
                    snapshot.ready
                        && !snapshot.busy
                        && !snapshot.waiting
                        && snapshot.requests.is_empty()
                })
            {
                assert!(
                    !provider.is_completed(),
                    "the tick must retire a live exact provider"
                );
                break (run, provider);
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "offline provider did not settle"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let later_passed = create(&host, &spec, "Last passed owner", true);
    let later_passed_claim = claim_at(&handle, &later_passed, Uuid::new_v4(), first + 4);
    receipt(&root, &later_passed_claim.firing, Outcome::Passed);
    let due = create(&host, &spec, "Due work must not be claimed", false);
    let occurrence = Compiled::new(&due.definition.schedule)
        .unwrap()
        .next_after(now.saturating_sub(60_001))
        .unwrap();
    assert!(occurrence.due_ms <= now);
    let due = registry
        .lock()
        .unwrap()
        .set_automation_enabled(due.id, due.revision, Some(occurrence), true, now)
        .unwrap();
    let ids = [
        passed_claim.firing.id,
        stopped_claim.firing.id,
        interrupted_claim.firing.id,
        fresh_claim.firing.id,
        later_passed_claim.firing.id,
    ];
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .active_automation_firings()
            .unwrap()
            .iter()
            .map(|firing| firing.id)
            .collect::<Vec<_>>(),
        ids,
        "the refused Passed owner must precede every cleanup and provider release"
    );
    let fault = rusqlite::Connection::open(&database).unwrap();
    if refuse_cleanup {
        // UUID is generated fixture data, not a SQL identifier or user string.
        fault.execute_batch(&format!("CREATE TRIGGER refuse_fixture_stopped_cleanup BEFORE UPDATE ON automation_firings WHEN OLD.id='{}' BEGIN SELECT RAISE(ABORT, 'fixture stopped cleanup storage failure'); END;", stopped_claim.firing.id)).unwrap();
    }
    let writer = OpenOptions::new()
        .read(true)
        .write(true)
        .open(settings.with_extension("toml.lock"))
        .unwrap();
    // SAFETY: this private fixture descriptor owns the writer lease until drop.
    assert_eq!(
        unsafe { libc::flock(writer.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let (done_tx, done_rx) = mpsc::channel();
    let worker_handle = handle.clone();
    let worker_host = host.clone();
    let worker_registry = registry.clone();
    let automation_ids = [
        passed.id,
        stopped.id,
        interrupted.id,
        fresh.id,
        later_passed.id,
        due.id,
    ];
    let agent_id = accepted.agent.id;
    let worker = std::thread::spawn(move || {
        let result = worker_handle.tick_under_control(&worker_host);
        // Snapshot all assertions while the actual writer is still retained.
        // No second tick or explicit release can manufacture these results.
        let store = worker_registry.lock().unwrap();
        let firings = ids
            .iter()
            .map(|id| store.automation_firing(*id).unwrap())
            .collect::<Vec<_>>();
        let automations = automation_ids
            .iter()
            .map(|id| store.automation(*id).unwrap())
            .collect::<Vec<_>>();
        let agent_closed = store.agent(agent_id).unwrap().closed;
        drop(store);
        let provider_absent = worker_host.lock().unwrap().chats.is_empty();
        done_tx
            .send((result, firings, automations, agent_closed, provider_absent))
            .unwrap();
    });
    let observed = done_rx.recv_timeout(Duration::from_secs(5));
    // On failure always release and join before asserting; never orphan a
    // blocked broker thread or leave its fixture provider running.
    drop(writer);
    let joined = worker.join();
    fault
        .execute_batch("DROP TRIGGER IF EXISTS refuse_fixture_stopped_cleanup")
        .unwrap();
    if !registry
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap()
        .closed
    {
        // Teardown only, AFTER the held-writer snapshot. This is not evidence
        // that the tested tick successfully released the provider.
        agent_execution::reconcile_agent_run(&host, &finished, true).unwrap();
    }
    joined.unwrap();
    let (result, firings, automations, agent_closed, provider_absent) =
        observed.expect("same tick blocked behind settings writer");
    let error = result.unwrap_err();
    assert_eq!(
        firings[0], passed_claim.firing,
        "refusal retains exact oldest custody without acceptance"
    );
    assert_eq!(automations[0], passed);
    assert_eq!(
        firings[4], later_passed_claim.firing,
        "remaining census cannot accept another Passed owner"
    );
    assert_eq!(automations[4], later_passed);
    assert_eq!(automations[5], due, "refusal returns before due/new claims");
    if refuse_cleanup {
        assert!(
            error.contains("fixture stopped cleanup storage failure"),
            "unrelated storage error must remain fatal: {error}"
        );
        assert_eq!(
            firings[1], stopped_claim.firing,
            "failed cleanup transaction preserves original custody"
        );
        assert_eq!(automations[1], stopped);
        assert_eq!(firings[2], interrupted_firing);
        assert_eq!(automations[2], interrupted_before);
        assert!(
            !agent_closed && !provider_absent,
            "fatal storage failure still stops the census"
        );
    } else {
        assert!(
            error.contains("Cannot read agent concurrency settings"),
            "policy refusal must remain visible: {error}"
        );
        assert_eq!(firings[1].state, FiringState::Cancelled);
        assert!(firings[1].agent.is_none() && firings[1].run.is_none());
        assert!(automations[1].active_firing.is_none());
        assert_eq!(firings[2], interrupted_firing);
        assert!(automations[2].active_firing.is_none() && automations[2].blocked.is_none());
        assert!(!automations[2].enabled);
        assert_eq!(firings[3].state, FiringState::Succeeded);
        assert_eq!(firings[3].run, Some(accepted.run.id));
        assert_eq!(firings[3].agent, Some(accepted.agent.id));
        assert!(!firings[3].release_pending && automations[3].active_firing.is_none());
        assert!(
            agent_closed && provider_absent,
            "same tick must retire the exact Fresh provider before reporting refusal"
        );
    }
    assert!(provider.is_completed());
    assert_eq!(
        std::fs::read_to_string(fixture_root.join("spawns"))
            .unwrap()
            .lines()
            .count(),
        1,
        "no refused or due work may dispatch another provider"
    );
    assert_eq!(
        std::fs::read_to_string(fixture_root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(agent_execution::concurrency_limits().unwrap(), configured);
}
