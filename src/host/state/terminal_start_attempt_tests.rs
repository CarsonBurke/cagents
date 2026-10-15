//! Real post-holder failures exercise the producer; no synthetic completed bit.
use super::*;
use crate::terminal::{TerminalStartAttempt, TerminalStartDisposition};

struct Fixture {
    root: tempfile::TempDir,
    host: SharedHost,
    _lease: crate::daemon::HostLease,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new(uuid::Uuid::new_v4().to_string()),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: crate::model::HostIncarnation::new(uuid::Uuid::new_v4().to_string()),
        };
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        let mut state = HostState::new(proof);
        state.register_client(1);
        state.enable_holder_recovery();
        Self { root, host: Arc::new(HostLock::new(state)), _lease: lease }
    }
    fn session(&self, transcript: Option<PathBuf>) -> TerminalSession {
        let recipe = crate::model::LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), r#"printf '%s\n' "$$" >> "$1"; exec /bin/cat"#.into(),
                "attempt-fixture".into(), self.root.path().join("spawns").to_string_lossy().into_owned()],
            working_directory: self.root.path().into(), environment: Default::default(), unset_environment: Default::default(),
        };
        TerminalSession { id: TerminalId::new(), launch: recipe.clone(), transcript_path: transcript,
            resume_hint: None, owned_codex_listener: None, recovery_origin: None, predecessor: None,
            continuity: TerminalContinuity::RelaunchAvailable { recipe, last_exit: None } }
    }
    fn start(&self, attempt: &TerminalStartAttempt) -> Result<(TerminalDescriptor, TerminalHandle), TerminalStartFailure> {
        let context = Arc::new(crate::chat::protocol::ExecutionContext::capture(self.root.path()).unwrap());
        spawn_terminal_view_owned_at_directory_attempt(&self.host, 1, attempt.session().clone(),
            TerminalSpawnOptions { initially_visible: false, ..Default::default() }, None, false, context, attempt)
    }
    fn spawns(&self, expected: usize) -> Vec<i32> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let rows = std::fs::read_to_string(self.root.path().join("spawns")).unwrap_or_default();
            let pids: Vec<_> = rows.lines().map(|line| line.parse::<i32>().unwrap()).collect();
            if pids.len() >= expected { assert_eq!(pids.len(), expected); return pids; }
            assert!(Instant::now() < deadline, "actual PTY program did not start");
            thread::sleep(Duration::from_millis(2));
        }
    }
}
struct Close(TerminalHandle);
impl Drop for Close { fn drop(&mut self) { let _ = self.0.shutdown(); } }

#[test]
fn terminal_start_attempt_pre_effect_refusal_is_not_an_unknown_spawn() {
    const NAME: &str = "host::state::terminal_start_attempt_tests::terminal_start_attempt_pre_effect_refusal_is_not_an_unknown_spawn";
    if crate::chat::runtime::isolated_backend_contract(NAME) { return; }
    let fixture = Fixture::new();
    let attempt = TerminalStartAttempt::new(fixture.session(None));
    assert_eq!(attempt.disposition(), TerminalStartDisposition::Pending);
    let error = spawn_terminal_view_owned_at_directory_attempt(&fixture.host, 1, attempt.session().clone(),
        Default::default(), None, false, Arc::new(crate::chat::protocol::ExecutionContext::default()), &attempt).unwrap_err();
    assert!(error.error.message.contains("no verified working-directory descriptor"));
    assert_eq!(error.attempt.id(), attempt.id());
    assert_eq!(attempt.disposition(), TerminalStartDisposition::NoProvider);
    assert!(attempt.worker_instance().is_none());
    assert!(attempt.holder_record().is_none());
    assert!(!fixture.root.path().join("spawns").exists());
    assert!(fixture.start(&attempt).unwrap_err().error.message.contains("already attempted"));
    assert_eq!(attempt.disposition(), TerminalStartDisposition::NoProvider);
    assert_eq!(fixture.host.lock().unwrap().inventory_count(), 0);
}

#[test]
fn terminal_start_attempt_post_holder_failure_retains_exact_owner_until_family_proof() {
    const NAME: &str = "host::state::terminal_start_attempt_tests::terminal_start_attempt_post_holder_failure_retains_exact_owner_until_family_proof";
    if crate::chat::runtime::isolated_backend_contract(NAME) { return; }
    let fixture = Fixture::new();
    let transcript = fixture.root.path().join("transcript-directory");
    std::fs::create_dir(&transcript).unwrap();
    let attempt = TerminalStartAttempt::new(fixture.session(Some(transcript.clone())));
    let mut original = None;
    TerminalHandle::with_blocked_holder_cleanup_for_test(|| {
        let error = fixture.start(&attempt).unwrap_err();
        assert!(error.error.message.contains("open transcript"), "{:?}", error.error);
        assert_eq!(error.attempt.id(), attempt.id());
        assert_eq!(attempt.disposition(), TerminalStartDisposition::Unknown);
        assert_eq!(attempt.retry_cleanup(), TerminalStartDisposition::Unknown);
        let record = attempt.holder_record().expect("actual registered holder tuple");
        assert_eq!(record.terminal, attempt.terminal_id());
        assert!(record.root.is_current() && record.holder.is_current());
        assert_eq!(fixture.spawns(1), vec![record.root.pid]);
        original = Some(record);
        assert!(fixture.start(&attempt).unwrap_err().error.message.contains("already attempted"));
        assert_eq!(attempt.disposition(), TerminalStartDisposition::Unknown);
        assert_eq!(fixture.host.lock().unwrap().inventory_count(), 0);
        assert!(!fixture.host.lock().unwrap().spawning.contains(&attempt.terminal_id()));
        assert_eq!(fixture.spawns(1).len(), 1, "unknown ownership cannot replay Resume");
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while attempt.retry_cleanup() != TerminalStartDisposition::FamilySettled {
        assert!(Instant::now() < deadline, "exact holder never proved full cleanup: {:?}", attempt);
        thread::sleep(Duration::from_millis(2));
    }
    let original = original.unwrap();
    assert!(!original.root.is_current() && !original.holder.is_current());
    assert!(attempt.published().is_none());
    // Only a positive family receipt permits the caller to allocate a fresh
    // attempt for this SAME retained terminal ID. Repair only the test's log.
    std::fs::remove_dir(&transcript).unwrap();
    let next = TerminalStartAttempt::new(attempt.session().clone());
    assert_ne!(next.id(), attempt.id());
    assert_eq!(next.terminal_id(), attempt.terminal_id());
    let (descriptor, handle) = fixture.start(&next).unwrap();
    let _close = Close(handle.clone());
    assert_eq!(next.disposition(), TerminalStartDisposition::Published);
    assert_ne!(next.worker_instance(), attempt.worker_instance());
    assert_eq!(descriptor.execution_instance, handle.execution_instance());
    assert_eq!(fixture.spawns(2).len(), 2);
    handle.shutdown().unwrap();
}

#[test]
fn terminal_start_attempt_lost_publication_reply_recovers_same_worker_without_replay() {
    const NAME: &str = "host::state::terminal_start_attempt_tests::terminal_start_attempt_lost_publication_reply_recovers_same_worker_without_replay";
    if crate::chat::runtime::isolated_backend_contract(NAME) { return; }
    let fixture = Fixture::new();
    let attempt = TerminalStartAttempt::new(fixture.session(None));
    let (original, handle) = fixture.start(&attempt).unwrap();
    let _close = Close(handle.clone());
    // The consumer loses its immediate result, but the original ticket was
    // stored before invocation and owns the exact published tuple.
    drop((original.clone(), handle.clone()));
    let (recovered, same) = attempt.published().expect("exact published result survives reply loss");
    assert_eq!(recovered, original);
    assert!(same.same_worker(&handle));
    assert_eq!(attempt.disposition(), TerminalStartDisposition::Published);
    assert!(fixture.start(&attempt).unwrap_err().error.message.contains("already attempted"));
    assert_eq!(fixture.spawns(1).len(), 1);
    assert!(fixture.host.lock().unwrap().terminals[&attempt.terminal_id()].handle.same_worker(&same));
    same.shutdown().unwrap();
    assert_eq!(attempt.disposition(), TerminalStartDisposition::Published,
        "a completed published result is still this operation, never permission to launch again");
}

#[test]
fn terminal_start_attempt_lost_worker_reply_retains_unknown_until_exact_cleanup() {
    const NAME: &str = "host::state::terminal_start_attempt_tests::terminal_start_attempt_lost_worker_reply_retains_unknown_until_exact_cleanup";
    if crate::chat::runtime::isolated_backend_contract(NAME) { return; }
    let fixture = Fixture::new();
    let attempt = TerminalStartAttempt::new(fixture.session(None));
    attempt.lose_worker_start_reply_for_test();
    TerminalHandle::with_blocked_holder_cleanup_for_test(|| {
        let error = fixture.start(&attempt).unwrap_err();
        assert!(error.error.message.contains("exited before startup completed"), "{:?}", error.error);
        assert_eq!(attempt.disposition(), TerminalStartDisposition::Unknown,
            "lost result after a real worker exists is never NoProvider");
        assert!(attempt.worker_instance().is_some());
        assert!(attempt.holder_record().is_some());
        assert!(attempt.published().is_none());
        assert_eq!(fixture.spawns(1).len(), 1);
        assert!(fixture.start(&attempt).unwrap_err().error.message.contains("already attempted"));
        assert_eq!(fixture.spawns(1).len(), 1, "response loss cannot replay the PTY launch");
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while attempt.retry_cleanup() != TerminalStartDisposition::FamilySettled {
        assert!(Instant::now() < deadline, "lost-reply worker cleanup remains unconfirmed: {:?}", attempt);
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(fixture.host.lock().unwrap().inventory_count(), 0);
}
