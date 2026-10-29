//! Real helper refusals and the exact startup owner cancellation/proof fence.
use super::*;
use crate::chat::protocol::{HelperWaitProof, LaunchCleanup, LaunchOutcome, LaunchOwnerKind};

fn owner() -> Arc<StartupOwner> {
    Arc::new(StartupOwner {
        control: Mutex::new(None),
        cancelled: AtomicBool::new(false),
        progress: Mutex::new(StartupProgress {
            outcome: LaunchOutcome::NoProvider,
            ordinary_deferred: false,
        }),
        finished: std::sync::Condvar::new(),
        unwatched_child: Mutex::new(None),
        cleanup_waiters: std::sync::atomic::AtomicUsize::new(0),
        fail_next_wait: AtomicBool::new(false),
    })
}

/// Installed before spawn so every assertion/unwind retains the actual Child.
/// Only this guard calls its original owner's sole wait; there is no PID path.
struct DirectChildGuard {
    owner: Arc<StartupOwner>,
    control: Option<UnixStream>,
    child: Option<std::process::Child>,
}

impl DirectChildGuard {
    fn new(owner: Arc<StartupOwner>, control: Option<UnixStream>) -> Self {
        Self {
            owner,
            control,
            child: None,
        }
    }

    fn spawn(&mut self, command: &mut ChildCommand) -> Result<(), String> {
        self.child = Some(self.owner.spawn_child(command)?);
        Ok(())
    }

    fn control(&self) -> &UnixStream {
        self.control.as_ref().expect("live fixture control")
    }

    fn disconnect(&mut self) {
        if let Some(control) = self.control.take() {
            let _ = control.shutdown(std::net::Shutdown::Both);
        }
        self.owner.cancel_control();
    }

    fn take_child(&mut self) -> std::process::Child {
        self.child.take().expect("fixture owns its original Child")
    }

    fn wait(&mut self) -> Result<std::process::ExitStatus, String> {
        self.disconnect();
        let child = self
            .child
            .take()
            .or_else(|| {
                self.owner
                    .unwatched_child
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take()
                    .map(|retained| retained._child)
            })
            .ok_or("The fixture no longer owns an unwaited original Child")?;
        self.owner.wait_for_child(child)
    }
}

impl Drop for DirectChildGuard {
    fn drop(&mut self) {
        self.disconnect();
        let child = self.child.take().or_else(|| {
            self.owner
                .unwatched_child
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
                .map(|retained| retained._child)
        });
        if let Some(child) = child {
            let pid = child.id();
            let waited = self.owner.wait_for_child(child);
            eprintln!("direct startup fixture original Child {pid} settled: {waited:?}");
        }
    }
}

#[test]
fn ordinary_latch_refuses_first_and_replacement_late_claims_before_spawn() {
    let context = ExecutionContext::default();
    let empty = context.request_ordinary_cleanup();
    assert!(empty.token.is_none());
    let first = owner();
    let token = context.claim_startup_owner(first.clone());
    assert!(first.begin_spawn().is_err());
    let captured = context.ownership_snapshot();
    assert_eq!(captured.token, Some(token));
    assert!(captured.ordinary_requested);
    assert_eq!(
        context.poll_cleanup(&captured).outcome,
        LaunchOutcome::NoProvider
    );

    let late = owner();
    let late_token = context.claim_startup_owner(late.clone());
    assert_ne!(late_token, token);
    assert!(late.begin_spawn().is_err());
    // Neither the initial empty snapshot nor the old generation settles it.
    assert_eq!(
        context.poll_cleanup(&empty).outcome,
        LaunchOutcome::Uncertain
    );
    assert_eq!(
        context.poll_cleanup(&captured).outcome,
        LaunchOutcome::Uncertain
    );
    assert_eq!(context.ownership_snapshot().token, Some(late_token));
    let current = context.ownership_snapshot();
    assert_eq!(
        context.poll_cleanup(&current).outcome,
        LaunchOutcome::NoProvider
    );
}

#[test]
fn spawn_transition_publishes_uncertainty_before_releasing_its_short_mutex() {
    let owner = owner();
    owner.begin_spawn().unwrap();
    owner.request_ordinary_cleanup();
    assert_eq!(owner.poll_cleanup(), LaunchOutcome::Uncertain);
    assert_eq!(owner.cleanup().unwrap(), LaunchOutcome::Uncertain);
    assert_eq!(owner.original_helper_wait_proof(), HelperWaitProof::Pending);
}

#[test]
fn failed_actual_spawn_is_no_provider_only_after_cancellation_fence() {
    let owner = owner();
    let mut command = ChildCommand::new("/nonexistent/cagents-startup-no-child");
    assert!(owner.spawn_child(&mut command).is_err());
    assert_eq!(owner.poll_cleanup(), LaunchOutcome::Uncertain);
    owner.request_ordinary_cleanup();
    assert_eq!(owner.poll_cleanup(), LaunchOutcome::NoProvider);
    assert!(owner.begin_spawn().is_err());
}

#[test]
fn ordinary_request_wakes_an_already_entered_cleanup_without_publishing_proof() {
    let context = Arc::new(ExecutionContext::default());
    let owner = owner();
    context.claim_startup_owner(owner.clone());
    owner.begin_spawn().unwrap();
    let waiting = context.clone();
    let (finished, received) = std::sync::mpsc::sync_channel(1);
    let worker = thread::spawn(move || finished.send(waiting.reconcile_cleanup()).unwrap());
    let deadline = Instant::now() + Duration::from_secs(2);
    while context.cleanup_waiters_for_test() == 0 {
        assert!(
            Instant::now() < deadline,
            "cleanup never entered its real wait"
        );
        thread::yield_now();
    }
    let captured = context.request_ordinary_cleanup();
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap(),
        LaunchOutcome::Uncertain
    );
    worker.join().unwrap();
    assert_eq!(
        context.poll_cleanup(&captured).outcome,
        LaunchOutcome::Uncertain
    );
    assert_eq!(context.ownership_snapshot().token, captured.token);
    assert_eq!(context.cleanup_waiters_for_test(), 0);
}

#[test]
fn cancellation_rejects_remote_transfer_and_remote_role_survives_installed() {
    let cancelled = ExecutionContext::default();
    let startup = owner();
    let token = cancelled.claim_startup_owner(startup.clone());
    cancelled.request_ordinary_cleanup();
    let remote = RemoteActor::fixture();
    assert!(!cancelled.transfer_startup_owner(token, Arc::new(remote.clone()), remote.identity()));
    assert_eq!(
        cancelled.ownership_snapshot().kind,
        LaunchOwnerKind::Startup
    );
    assert!(startup.begin_spawn().is_err());

    let context = ExecutionContext::default();
    let startup = owner();
    startup.begin_spawn().unwrap();
    let token = context.claim_startup_owner(startup);
    let before = context.ownership_snapshot();
    assert!(context.transfer_startup_owner(token, Arc::new(remote.clone()), remote.identity()));
    let rejected = owner();
    assert_eq!(context.claim_startup_owner(rejected.clone()), token);
    assert!(rejected.begin_spawn().is_err());
    assert!(!context.ownership_snapshot().ordinary_requested);
    assert_eq!(
        context.ownership_snapshot().kind,
        LaunchOwnerKind::RemoteActor {
            identity: remote.identity()
        }
    );
    let transferred = context.request_ordinary_cleanup();
    assert!(transferred.remote.is_some());
    assert_eq!(
        context.poll_cleanup(&before).outcome,
        LaunchOutcome::Uncertain
    );
    assert_eq!(
        context.reconcile_cleanup().unwrap(),
        LaunchOutcome::Uncertain
    );
    assert_eq!(remote.cleanup().unwrap(), LaunchOutcome::Uncertain);
    assert!(!context.complete_semantic_cleanup(token, &remote.identity()));
    context.installed();
    let installed = context.ownership_snapshot();
    assert_eq!(installed.token, Some(token));
    assert_eq!(
        installed.kind,
        LaunchOwnerKind::RemoteActor {
            identity: remote.identity()
        }
    );
    assert_eq!(installed.original_helper, HelperWaitProof::Pending);
    assert!(installed.remote.is_none());
    assert!(!context.complete_hosted_cleanup(&installed));
    let mut forged_generic = installed.clone();
    forged_generic.kind = LaunchOwnerKind::Generic;
    assert!(!context.complete_hosted_cleanup(&forged_generic));
    assert_eq!(context.outcome(), LaunchOutcome::Uncertain);
    // A late generic claim must not downgrade the permanent remote role.
    context.claim_owner(owner());
    assert_eq!(context.ownership_snapshot().kind, installed.kind);
}

fn helper_command(peer: &UnixStream, deny_subreaper: bool) -> ChildCommand {
    let fd = peer.as_raw_fd();
    let mut command = ChildCommand::new(helper_executable().unwrap());
    command
        .arg(ARGUMENT)
        .arg(fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if deny_subreaper {
                // Refuse only the required policy syscall in the actual
                // helper. No production switch or fake helper is introduced.
                let filter = [
                    libc::sock_filter {
                        code: 0x20,
                        jt: 0,
                        jf: 0,
                        k: 0,
                    },
                    libc::sock_filter {
                        code: 0x15,
                        jt: 0,
                        jf: 3,
                        k: libc::SYS_prctl as u32,
                    },
                    libc::sock_filter {
                        code: 0x20,
                        jt: 0,
                        jf: 0,
                        k: 16,
                    },
                    libc::sock_filter {
                        code: 0x15,
                        jt: 0,
                        jf: 1,
                        k: libc::PR_SET_CHILD_SUBREAPER as u32,
                    },
                    libc::sock_filter {
                        code: 0x06,
                        jt: 0,
                        jf: 0,
                        k: libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
                    },
                    libc::sock_filter {
                        code: 0x06,
                        jt: 0,
                        jf: 0,
                        k: libc::SECCOMP_RET_ALLOW,
                    },
                ];
                let program = libc::sock_fprog {
                    len: filter.len() as u16,
                    filter: filter.as_ptr().cast_mut(),
                };
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                    || libc::prctl(
                        libc::PR_SET_SECCOMP,
                        libc::SECCOMP_MODE_FILTER,
                        &program as *const libc::sock_fprog,
                    ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    command
}

fn refused_request(source: chat::Session) -> Startup {
    let prepared_identity = chat::protocol::create(&source.launch)
        .unwrap()
        .1
        .prepared_launch_identity()
        .unwrap();
    Startup {
        protocol: PROTOCOL,
        program: source.launch.recipe.program.clone(),
        arguments: source.launch.recipe.arguments.clone(),
        environment: source.launch.recipe.environment.clone(),
        working_directory: source.launch.recipe.working_directory.clone(),
        source,
        prepared_identity,
        directory_fd: None,
        directory_identity: None,
        mailbox_recognition: None,
        provider_directory_observed: false,
        recovery_environment: BTreeMap::new(),
    }
}

#[test]
fn actual_helper_preframe_eof_and_refused_authority_prove_original_wait73() {
    for authority in ["eof", "missing", "invalid"] {
        let owner = owner();
        let (control, peer) = UnixStream::pair().unwrap();
        let mut child = DirectChildGuard::new(owner.clone(), Some(control));
        let mut command = helper_command(&peer, false);
        child.spawn(&mut command).unwrap();
        drop(peer);
        if authority != "eof" {
            let mut source = fixture_source();
            if authority == "missing" {
                source.proof = None;
            }
            send(child.control(), &refused_request(source), TIMEOUT).unwrap();
            let response: Result<Attached, String> = receive(child.control(), TIMEOUT).unwrap();
            let Err(error) = response else {
                panic!("helper accepted refused authority")
            };
            if authority == "missing" {
                assert!(error.contains("lacks proof"));
            }
        }
        assert_eq!(child.wait().unwrap().code(), Some(73));
        assert_eq!(
            owner.original_helper_wait_proof(),
            HelperWaitProof::TeardownProved
        );
    }
}

#[test]
fn an_actual_status_one_never_proves_startup_teardown() {
    let owner = owner();
    let mut child = DirectChildGuard::new(owner.clone(), None);
    child
        .spawn(ChildCommand::new("/bin/sh").args(["-c", "exit 1"]))
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(1));
    owner.request_ordinary_cleanup();
    assert_eq!(owner.poll_cleanup(), LaunchOutcome::Uncertain);
    assert_eq!(owner.original_helper_wait_proof(), HelperWaitProof::Pending);
}

#[test]
fn failed_watcher_admission_retains_the_original_child_and_uncertain_owner() {
    let context = ExecutionContext::default();
    let owner = owner();
    let retained = Arc::downgrade(&owner);
    context.claim_startup_owner(owner.clone());
    let (publish, incoming) = std::sync::mpsc::sync_channel(1);
    drop(incoming);
    let mut child = DirectChildGuard::new(owner.clone(), None);
    child.spawn(&mut ChildCommand::new("/bin/true")).unwrap();
    let child_id = child.child.as_ref().unwrap().id();
    assert!(
        owner
            .publish_child(&publish, child.take_child())
            .unwrap_err()
            .contains("custody is retained")
    );
    drop(owner);
    let captured = context.request_ordinary_cleanup();
    assert_eq!(
        context.poll_cleanup(&captured).outcome,
        LaunchOutcome::Uncertain
    );
    assert_eq!(captured.original_helper, HelperWaitProof::Pending);
    assert!(
        captured
            .diagnostic
            .as_ref()
            .unwrap()
            .contains("custody is retained")
    );
    let retained = retained
        .upgrade()
        .expect("context must retain exact child owner");
    assert_eq!(
        retained
            .unwatched_child
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            ._child
            .id(),
        child_id
    );
    // Sole test cleanup consumes the retained Child itself. No replacement
    // waiter or synthetic production status is installed on the owner.
    assert!(child.wait().unwrap().success());
    assert_eq!(
        context.poll_cleanup(&captured).outcome,
        LaunchOutcome::Uncertain
    );
}

#[test]
fn failed_original_wait_retains_that_child_and_only_a_pending_diagnostic() {
    let context = ExecutionContext::default();
    let owner = owner();
    context.claim_startup_owner(owner.clone());
    let mut child = DirectChildGuard::new(owner.clone(), None);
    child.spawn(&mut ChildCommand::new("/bin/true")).unwrap();
    let child_id = child.child.as_ref().unwrap().id();
    owner.fail_next_wait.store(true, Ordering::Release);
    let error = child.wait().unwrap_err();
    assert!(error.contains("Child.wait failed"));
    assert_eq!(
        owner
            .unwatched_child
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            ._child
            .id(),
        child_id
    );
    let captured = context.request_ordinary_cleanup();
    assert_eq!(captured.original_helper, HelperWaitProof::Pending);
    assert!(
        captured
            .diagnostic
            .as_ref()
            .unwrap()
            .contains("without an automatic retry or cleanup proof")
    );
    assert_eq!(
        context.poll_cleanup(&captured).outcome,
        LaunchOutcome::Uncertain
    );
    // The original fixture custodian alone recovers the same retained Child
    // on Drop and performs its actual wait. Status zero cannot mint proof73.
    drop(child);
    assert!(owner.unwatched_child.lock().unwrap().is_none());
    assert_eq!(owner.original_helper_wait_proof(), HelperWaitProof::Pending);
    assert_eq!(
        context.poll_cleanup(&captured).outcome,
        LaunchOutcome::Uncertain
    );
}

#[test]
fn semantic_completion_retains_spawned_origin_until_its_original_wait() {
    let owner = owner();
    let (control, peer) = UnixStream::pair().unwrap();
    let mut child = DirectChildGuard::new(owner.clone(), Some(control));
    let mut command = helper_command(&peer, false);
    child.spawn(&mut command).unwrap();
    drop(peer);
    child.disconnect();
    let mut remote = RemoteActor::fixture();
    *Arc::get_mut(&mut remote.0)
        .unwrap()
        .origin
        .get_mut()
        .unwrap() = SpawnOrigin::SpawnedHere(owner.clone());
    let context = ExecutionContext::default();
    let token = context.claim_startup_owner(owner.clone());
    assert!(context.transfer_startup_owner(token, Arc::new(remote.clone()), remote.identity()));
    context.installed();
    assert_eq!(
        remote.original_helper_wait_proof(),
        HelperWaitProof::Pending
    );
    assert!(!context.complete_semantic_cleanup(token, &remote.identity()));
    assert_eq!(child.wait().unwrap().code(), Some(73));
    assert_eq!(
        remote.original_helper_wait_proof(),
        HelperWaitProof::TeardownProved
    );
    let mut wrong = remote.identity();
    wrong.instance = uuid::Uuid::new_v4();
    assert!(!context.complete_semantic_cleanup(token, &wrong));
    assert!(context.complete_semantic_cleanup(token, &remote.identity()));
    assert_eq!(
        context.ownership_snapshot().outcome,
        LaunchOutcome::TeardownProved
    );
    assert_eq!(
        context.ownership_snapshot().kind,
        LaunchOwnerKind::RemoteActor {
            identity: remote.identity()
        }
    );
    assert_eq!(
        RemoteActor::fixture().original_helper_wait_proof(),
        HelperWaitProof::NoParentChild
    );
}

#[test]
fn same_actor_attachment_inherits_strong_origin_and_refuses_conflicting_origins() {
    let owner = owner();
    let (control, peer) = UnixStream::pair().unwrap();
    let mut child = DirectChildGuard::new(owner.clone(), Some(control));
    child.spawn(&mut helper_command(&peer, false)).unwrap();
    drop(peer);
    child.disconnect();
    let mut previous = RemoteActor::fixture();
    *Arc::get_mut(&mut previous.0)
        .unwrap()
        .origin
        .get_mut()
        .unwrap() = SpawnOrigin::SpawnedHere(owner.clone());
    let adopted = previous.distinct_attachment_fixture();
    assert_eq!(
        adopted.original_helper_wait_proof(),
        HelperWaitProof::NoParentChild
    );
    adopted.inherit_original_helper_from(&previous).unwrap();
    adopted.inherit_original_helper_from(&adopted).unwrap();
    assert_eq!(
        adopted.original_helper_wait_proof(),
        HelperWaitProof::Pending
    );
    let mismatched = RemoteActor::fixture();
    assert!(mismatched.inherit_original_helper_from(&previous).is_err());
    let conflicting = previous.distinct_attachment_fixture();
    *conflicting.0.origin.lock().unwrap() = SpawnOrigin::SpawnedHere(self::owner());
    assert!(adopted.inherit_original_helper_from(&conflicting).is_err());
    let retained = Arc::downgrade(&owner);
    drop(previous);
    drop(owner);
    assert!(
        retained.strong_count() >= 2,
        "replacement attachment must retain progress in addition to the cleanup guard"
    );
    assert_eq!(child.wait().unwrap().code(), Some(73));
    assert_eq!(
        adopted.original_helper_wait_proof(),
        HelperWaitProof::TeardownProved
    );
}

#[test]
fn actual_helper_subreaper_refusal_is_before_provider_and_still_reaps73() {
    const CHILD: &str = "CAGENTS_SUBREAPER_REFUSAL_TEST";
    if std::env::var_os(CHILD).is_none() {
        let isolated = tempfile::tempdir_in("/var/tmp").unwrap();
        let test = thread::current().name().unwrap().to_owned();
        let output = ChildCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_RUNTIME_DIR", isolated.path())
            .env("XDG_DATA_HOME", isolated.path().join("data"))
            .env("XDG_CONFIG_HOME", isolated.path().join("config"))
            .env("XDG_CACHE_HOME", isolated.path().join("cache"))
            .env("HOME", isolated.path().join("home"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let marker = root.path().join("provider-started");
    let mut source = fixture_source();
    source.launch.recipe.program = "/bin/sh".into();
    source.launch.recipe.arguments = vec!["-c".into(), format!("touch {}", marker.display())];
    source.proof.as_mut().unwrap().boot_id = crate::daemon::read_boot_id().unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, source.proof.as_ref().unwrap()).unwrap();
    let owner = owner();
    let (control, peer) = UnixStream::pair().unwrap();
    let mut child = DirectChildGuard::new(owner.clone(), Some(control));
    // Confirm this exact caller would authorize before introducing the kernel
    // refusal; the helper still performs its own real authentication.
    authorize(child.control(), source.proof.as_ref().unwrap()).unwrap();
    let mut command = helper_command(&peer, true);
    child.spawn(&mut command).unwrap();
    drop(peer);
    send(child.control(), &refused_request(source), TIMEOUT).unwrap();
    let response: Result<Attached, String> = receive(child.control(), TIMEOUT).unwrap();
    let Err(error) = response else {
        panic!("helper accepted refused subreaper setup")
    };
    assert!(error.contains("Operation not permitted"));
    assert_eq!(child.wait().unwrap().code(), Some(73));
    assert!(
        !marker.exists(),
        "refused helper must not execute its provider"
    );
    assert_eq!(
        owner.original_helper_wait_proof(),
        HelperWaitProof::TeardownProved
    );
}
