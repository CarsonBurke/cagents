//! Actual settings-writer contention through the broker's existing admission
//! and recovered-precheck routes. No provider or precheck process is launched.
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
            prompt: "Retained contention fixture; never dispatched".into(),
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
            service::now().unwrap(),
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
                .observe_precheck(&worker_host, &worker_stopped, service::now().unwrap())
                .unwrap();
            // On a blocking old-code control, the main thread releases the
            // writer before joining. Never continue into new work afterward.
            if !worker_held.load(Ordering::Acquire) {
                return;
            }
            worker_handle
                .observe_precheck(&worker_host, &interrupted_firing, service::now().unwrap())
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
            .observe_precheck(&worker_host, &passing_claim.firing, service::now().unwrap())
            .unwrap_err();
        assert!(
            error.contains("Cannot read agent concurrency settings"),
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
