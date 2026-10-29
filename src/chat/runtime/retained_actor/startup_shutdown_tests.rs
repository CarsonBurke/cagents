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
        cleanup_waiters: std::sync::atomic::AtomicUsize::new(0),
    })
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
        let mut command = helper_command(&peer, false);
        let child = owner.spawn_child(&mut command).unwrap();
        drop(peer);
        if authority != "eof" {
            let mut source = fixture_source();
            if authority == "missing" {
                source.proof = None;
            }
            send(&control, &refused_request(source), TIMEOUT).unwrap();
            let response: Result<Attached, String> = receive(&control, TIMEOUT).unwrap();
            let Err(error) = response else {
                panic!("helper accepted refused authority")
            };
            if authority == "missing" {
                assert!(error.contains("lacks proof"));
            }
        }
        drop(control);
        assert_eq!(owner.wait_for_child(child).unwrap().code(), Some(73));
        assert_eq!(
            owner.original_helper_wait_proof(),
            HelperWaitProof::TeardownProved
        );
    }
}

#[test]
fn an_actual_status_one_never_proves_startup_teardown() {
    let owner = owner();
    let child = owner
        .spawn_child(ChildCommand::new("/bin/sh").args(["-c", "exit 1"]))
        .unwrap();
    assert_eq!(owner.wait_for_child(child).unwrap().code(), Some(1));
    owner.request_ordinary_cleanup();
    assert_eq!(owner.poll_cleanup(), LaunchOutcome::Uncertain);
    assert_eq!(owner.original_helper_wait_proof(), HelperWaitProof::Pending);
}

#[test]
fn semantic_completion_retains_spawned_origin_until_its_original_wait() {
    let owner = owner();
    let (control, peer) = UnixStream::pair().unwrap();
    let mut command = helper_command(&peer, false);
    let child = owner.spawn_child(&mut command).unwrap();
    drop(peer);
    drop(control);
    let mut remote = RemoteActor::fixture();
    Arc::get_mut(&mut remote.0).unwrap().origin = SpawnOrigin::SpawnedHere(owner.clone());
    let context = ExecutionContext::default();
    let token = context.claim_startup_owner(owner.clone());
    assert!(context.transfer_startup_owner(token, Arc::new(remote.clone()), remote.identity()));
    context.installed();
    assert_eq!(
        remote.original_helper_wait_proof(),
        HelperWaitProof::Pending
    );
    assert!(!context.complete_semantic_cleanup(token, &remote.identity()));
    assert_eq!(owner.wait_for_child(child).unwrap().code(), Some(73));
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
    // Confirm this exact caller would authorize before introducing the kernel
    // refusal; the helper still performs its own real authentication.
    authorize(&control, source.proof.as_ref().unwrap()).unwrap();
    let mut command = helper_command(&peer, true);
    let child = owner.spawn_child(&mut command).unwrap();
    drop(peer);
    send(&control, &refused_request(source), TIMEOUT).unwrap();
    let response: Result<Attached, String> = receive(&control, TIMEOUT).unwrap();
    let Err(error) = response else {
        panic!("helper accepted refused subreaper setup")
    };
    assert!(error.contains("Operation not permitted"));
    assert_eq!(owner.wait_for_child(child).unwrap().code(), Some(73));
    assert!(
        !marker.exists(),
        "refused helper must not execute its provider"
    );
    assert_eq!(
        owner.original_helper_wait_proof(),
        HelperWaitProof::TeardownProved
    );
}
