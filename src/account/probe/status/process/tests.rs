use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::process::Command;

use super::{Hooks, Point, Process, Shared};
use crate::{
    account::{home::Homes, recovery::Candidate, registry::Store},
    agent::Provider,
    daemon::child_reaper::TestService,
    host::{AccountRecovery, AccountReservation, AccountTestHost, RecoveryStatusPermit},
};

pub(super) struct TestHooks {
    fail: Option<Point>,
    panic: Option<Point>,
    busy: AtomicUsize,
    events: Mutex<Vec<Point>>,
    progress: Condvar,
    gate: Option<Point>,
    released: Mutex<bool>,
    release: Condvar,
    shared: Mutex<Option<Weak<Shared>>>,
    discharged: AtomicBool,
    pub(super) cleanup_deadline: bool,
}

impl Default for TestHooks {
    fn default() -> Self {
        Self {
            fail: None,
            panic: None,
            busy: AtomicUsize::new(0),
            events: Mutex::new(Vec::new()),
            progress: Condvar::new(),
            gate: None,
            released: Mutex::new(false),
            release: Condvar::new(),
            shared: Mutex::new(None),
            discharged: AtomicBool::new(false),
            cleanup_deadline: false,
        }
    }
}

impl TestHooks {
    pub(super) fn at(&self, point: Point) -> io::Result<()> {
        if self.gate == Some(point) {
            let released = self.released.lock().unwrap();
            let (released, _) = self
                .release
                .wait_timeout_while(released, Duration::from_secs(5), |released| !*released)
                .unwrap();
            if !*released {
                return Err(io::Error::other("private test gate was not released"));
            }
        }
        assert_ne!(self.panic, Some(point), "private injected worker panic");
        if self.fail == Some(point) {
            return Err(io::Error::other("private injected status-process fault"));
        }
        Ok(())
    }

    pub(super) fn record(&self, point: Point) {
        self.events.lock().unwrap().push(point);
        self.progress.notify_all();
    }

    pub(super) fn observe(&self, shared: &Arc<Shared>) {
        *self.shared.lock().unwrap() = Some(Arc::downgrade(shared));
    }

    pub(super) fn executable_busy(&self) -> bool {
        self.busy
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(1)
            })
            .is_ok()
    }

    fn wait(&self, point: Point) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut events = self.events.lock().unwrap();
        while !events.contains(&point) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "private status process did not reach {point:?}: {events:?}"
            );
            events = self.progress.wait_timeout(events, remaining).unwrap().0;
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }

    fn snapshot(&self) -> Vec<Point> {
        self.events.lock().unwrap().clone()
    }

    fn shared(&self) -> Arc<Shared> {
        self.shared
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .expect("retained original custody remains anchored")
    }

    /// Discharge only this test's explicitly retained original. Production
    /// exposes no numeric-PID repair or automatic release. The private fake
    /// program creates no descendants; its original Child remains the sole
    /// wait authority, including the deliberate pidfd-capture failure case.
    fn discharge_private_original(&self) {
        self.wait(Point::Retained);
        assert!(!self.discharged.swap(true, Ordering::AcqRel));
        let shared = self.shared();
        let mut custody = shared.custody.lock().unwrap_or_else(|p| p.into_inner());
        assert!(custody.fault.is_some());
        assert!(custody.credit.is_some());
        let child = custody
            .child
            .as_mut()
            .expect("fault retained the original Child");
        let _ = child.kill();
        child.wait().unwrap();
        custody.child = None;
        drop(custody.pidfd.take());
        let credit = custody.credit.take();
        drop(custody);
        drop(credit);
        // SAFETY: Retained is emitted only after Anchor deliberately retains
        // exactly one process-owned Arc. The test now owns proof that its
        // original was reaped and uses this one-shot flag to release it once.
        unsafe { Arc::decrement_strong_count(Arc::as_ptr(&shared)) };
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    host: AccountTestHost,
    recovery: Option<AccountRecovery>,
    permit: Option<RecoveryStatusPermit>,
    homes: Homes,
    store: Store,
    candidate: Candidate,
    started: PathBuf,
}

impl Fixture {
    fn new(body: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        assert!(directory.path().is_absolute());
        let homes = Homes::at(directory.path().join("accounts"));
        let candidate = Candidate {
            provider: Provider::Claude,
            key: uuid::Uuid::new_v4(),
        };
        fs::create_dir_all(homes.path(candidate.provider, candidate.key)).unwrap();
        let store = Store::at(directory.path().join("registry"));
        let host = AccountReservation::test_host();
        let recovery =
            AccountRecovery::test_install(&host, store.clone(), &homes, candidate.clone()).unwrap();
        let program = directory.path().join("fake-provider");
        let started = directory.path().join("provider-started");
        let quoted = format!("'{}'", started.to_str().unwrap().replace('\'', "'\\''"));
        fs::write(
            &program,
            format!("#!/bin/sh\nset -eu\nprintf started > {quoted}\n{body}\n"),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let permit = recovery.status_permit(recovery.account(), program).unwrap();
        Self {
            _directory: directory,
            host,
            recovery: Some(recovery),
            permit: Some(permit),
            homes,
            store,
            candidate,
            started,
        }
    }

    fn command(&mut self) -> (Command, RecoveryStatusPermit) {
        let permit = self.permit.take().unwrap();
        let mut command = super::super::status_command(permit.account(), permit.program()).unwrap();
        command
            .args(["auth", "status", "--json"])
            .stdin(std::process::Stdio::piped());
        (command, permit)
    }

    fn reservation_is_held(&self) -> bool {
        AccountRecovery::test_install(
            &self.host,
            self.store.clone(),
            &self.homes,
            self.candidate.clone(),
        )
        .is_err()
    }
}

fn hooks(test: &Arc<TestHooks>) -> Hooks {
    Hooks {
        test: Some(test.clone()),
    }
}

#[tokio::test]
async fn cleanup_worker_start_failure_creates_no_provider_and_returns_credit() {
    let service = TestService::new(1);
    let test = Arc::new(TestHooks {
        fail: Some(Point::ThreadStart),
        ..Default::default()
    });
    let mut fixture = Fixture::new("exit 0");
    let (command, permit) = fixture.command();
    let result = Process::spawn_owned(
        command,
        permit,
        tokio::runtime::Handle::current(),
        service.reserve().unwrap(),
        hooks(&test),
    )
    .await;
    assert!(result.is_err());
    assert!(!fixture.started.exists());
    assert_eq!(service.free(), 1);
    assert!(!test.snapshot().contains(&Point::BeforeSpawn));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executable_busy_retries_reacquire_and_release_actual_spawn_leases() {
    let service = TestService::new(1);
    let test = Arc::new(TestHooks {
        busy: AtomicUsize::new(1),
        gate: Some(Point::RetryDelay),
        ..Default::default()
    });
    let mut fixture = Fixture::new("read -r ignored || :\nexit 0");
    let (command, permit) = fixture.command();
    let task = tokio::spawn(Process::spawn_owned(
        command,
        permit,
        tokio::runtime::Handle::current(),
        service.reserve().unwrap(),
        hooks(&test),
    ));
    test.wait(Point::RetryDelay);
    assert!(!fixture.started.exists());
    let lifecycle = fixture.homes.try_lifecycle_lock().unwrap();
    let registry = fixture.store.hold_registry_lock_for_account_test().unwrap();
    drop(registry);
    drop(lifecycle);
    test.release();
    let mut process = task.await.unwrap().unwrap();
    // Neither registry nor lifecycle leases span the provider's blocked I/O.
    let lifecycle = fixture.homes.try_lifecycle_lock().unwrap();
    let registry = fixture.store.hold_registry_lock_for_account_test().unwrap();
    drop(registry);
    drop(lifecycle);
    let events = test.snapshot();
    assert_eq!(
        events
            .iter()
            .filter(|&&point| point == Point::LeaseAcquired)
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|&&point| point == Point::LeaseReleased)
            .count(),
        2
    );
    assert!(process.finish().await.unwrap().success());
    service.await_free(1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_startup_receiver_keeps_original_custody_until_background_reap() {
    let service = TestService::new(1);
    let test = Arc::new(TestHooks {
        gate: Some(Point::Handoff),
        ..Default::default()
    });
    let mut fixture = Fixture::new("read -r ignored || :\nexit 0");
    let (command, permit) = fixture.command();
    let task = tokio::spawn(Process::spawn_owned(
        command,
        permit,
        tokio::runtime::Handle::current(),
        service.reserve().unwrap(),
        hooks(&test),
    ));
    test.wait(Point::Handoff);
    drop(fixture.recovery.take());
    assert!(fixture.reservation_is_held());
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    assert_eq!(service.free(), 0);
    test.release();
    test.wait(Point::OriginalReaped);
    service.await_free(1);
    assert!(
        test.snapshot()
            .iter()
            .position(|&point| point == Point::GroupSignalled)
            .unwrap()
            < test
                .snapshot()
                .iter()
                .position(|&point| point == Point::OriginalReaped)
                .unwrap()
    );
}

#[tokio::test]
async fn pipe_and_handoff_failures_cleanup_the_original_without_runtime_custody() {
    for point in [Point::PipeConversion, Point::Handoff] {
        let service = TestService::new(1);
        let test = Arc::new(TestHooks {
            fail: Some(point),
            ..Default::default()
        });
        let mut fixture = Fixture::new("read -r ignored || :\nexit 0");
        let (command, permit) = fixture.command();
        let result = Process::spawn_owned(
            command,
            permit,
            tokio::runtime::Handle::current(),
            service.reserve().unwrap(),
            hooks(&test),
        )
        .await;
        assert!(result.is_err());
        test.wait(Point::OriginalReaped);
        service.await_free(1);
        assert!(!test.snapshot().contains(&Point::Retained));
    }
}

#[tokio::test]
async fn pidfd_wait_and_worker_faults_retain_original_permit_and_credit_visibly() {
    for (fail, panic) in [
        (Some(Point::PidfdCapture), None),
        (Some(Point::PidfdValidation), None),
        (Some(Point::OriginalWait), None),
        (None, Some(Point::PipeConversion)),
    ] {
        let service = TestService::new(1);
        let test = Arc::new(TestHooks {
            fail,
            panic,
            ..Default::default()
        });
        let mut fixture = Fixture::new("exit 0");
        let (command, permit) = fixture.command();
        let result = Process::spawn_owned(
            command,
            permit,
            tokio::runtime::Handle::current(),
            service.reserve().unwrap(),
            hooks(&test),
        )
        .await;
        if let Ok(mut process) = result {
            assert!(process.finish().await.is_err());
        }
        test.wait(Point::Retained);
        drop(fixture.recovery.take());
        assert!(fixture.reservation_is_held());
        assert_eq!(service.free(), 0);
        let shared = test.shared();
        assert!(shared.reason().is_some());
        assert!(
            shared
                .custody
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .child
                .is_some()
        );
        assert_eq!(
            shared
                .custody
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pidfd
                .is_some(),
            fail != Some(Point::PidfdCapture),
            "an available original pidfd survives every later fault"
        );
        drop(shared);
        test.discharge_private_original();
        service.await_free(1);
    }
}

#[tokio::test]
async fn immediate_exit_is_not_reaped_before_the_last_group_signal() {
    let service = TestService::new(1);
    let test = Arc::new(TestHooks::default());
    let mut fixture = Fixture::new("exit 0");
    let (command, permit) = fixture.command();
    let mut process = Process::spawn_owned(
        command,
        permit,
        tokio::runtime::Handle::current(),
        service.reserve().unwrap(),
        hooks(&test),
    )
    .await
    .unwrap();
    let status = process.finish().await.unwrap();
    assert_eq!(status.code(), Some(0));
    assert_eq!(process.finish().await.unwrap(), status);
    let events = test.snapshot();
    let signalled = events
        .iter()
        .position(|&point| point == Point::GroupSignalled)
        .unwrap();
    let reaped = events
        .iter()
        .position(|&point| point == Point::OriginalReaped)
        .unwrap();
    let released = events
        .iter()
        .position(|&point| point == Point::CreditReleased)
        .unwrap();
    assert!(signalled < reaped && reaped < released);
    service.await_free(1);
}

#[test]
fn runtime_shutdown_after_stdspawn_does_not_drop_original_ownership() {
    let service = TestService::new(1);
    let test = Arc::new(TestHooks {
        gate: Some(Point::Handoff),
        ..Default::default()
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let mut fixture = Fixture::new("read -r ignored || :\nexit 0");
    let (command, permit) = fixture.command();
    let task = runtime.spawn(Process::spawn_owned(
        command,
        permit,
        runtime.handle().clone(),
        service.reserve().unwrap(),
        hooks(&test),
    ));
    test.wait(Point::Handoff);
    drop(fixture.recovery.take());
    assert!(fixture.reservation_is_held());
    runtime.shutdown_background();
    drop(task);
    test.release();
    test.wait(Point::OriginalReaped);
    service.await_free(1);
}

#[test]
fn missing_io_driver_cannot_unwind_away_original_child_custody() {
    let service = TestService::new(1);
    let test = Arc::new(TestHooks::default());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let mut fixture = Fixture::new("read -r ignored || :\nexit 0");
    let (command, permit) = fixture.command();
    let result = runtime.block_on(Process::spawn_owned(
        command,
        permit,
        runtime.handle().clone(),
        service.reserve().unwrap(),
        hooks(&test),
    ));
    assert!(result.is_err());
    test.wait(Point::Retained);
    drop(fixture.recovery.take());
    assert!(fixture.reservation_is_held());
    assert_eq!(service.free(), 0);
    test.discharge_private_original();
    service.await_free(1);
}

#[tokio::test]
async fn cleanup_deadline_reports_retained_custody_instead_of_waiting_forever() {
    let service = TestService::new(1);
    let test = Arc::new(TestHooks {
        cleanup_deadline: true,
        ..Default::default()
    });
    let mut fixture = Fixture::new("exit 0");
    let (command, permit) = fixture.command();
    let mut process = Process::spawn_owned(
        command,
        permit,
        tokio::runtime::Handle::current(),
        service.reserve().unwrap(),
        hooks(&test),
    )
    .await
    .unwrap();
    let error = process.finish().await.unwrap_err();
    assert!(error.contains("deadline"));
    test.wait(Point::Retained);
    assert_eq!(service.free(), 0);
    assert!(!test.snapshot().contains(&Point::OriginalReaped));
    test.discharge_private_original();
    service.await_free(1);
}

#[tokio::test]
async fn final_group_signal_closes_a_descendants_inherited_output_pipe() {
    use tokio::io::AsyncReadExt;

    let service = TestService::new(1);
    let test = Arc::new(TestHooks::default());
    // The private fake leader exits immediately; its same-group descendant
    // keeps stdout open. EOF therefore proves more than leader-only reaping.
    let mut fixture = Fixture::new("/bin/sleep 30 &\nexit 0");
    let (command, permit) = fixture.command();
    let mut process = Process::spawn_owned(
        command,
        permit,
        tokio::runtime::Handle::current(),
        service.reserve().unwrap(),
        hooks(&test),
    )
    .await
    .unwrap();
    let mut stdout = process.stdout.take().unwrap();
    assert_eq!(process.finish().await.unwrap().code(), Some(0));
    let mut output = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), stdout.read_to_end(&mut output))
        .await
        .expect("the owned group signal must close the descendant's pipe")
        .unwrap();
    service.await_free(1);
}
