//! Actual retained-helper pre-discovery startup versus ordinary HOST exit.
//! RPC is the existing predetermined fixture User, not delegated-agent proof.
use super::*;
use crate::{
    agent::Provider,
    chat::{
        self as provider_chat,
        protocol::{ExecutionContext, LaunchOutcome},
        runtime::startup_observation::{Event, Observation, Spawned},
    },
};
use std::{
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    sync::atomic::{AtomicBool, Ordering},
};

type StartResult = Result<ServerResponse, ProtocolError>;

/// Install this before starting any worker. Every unwind cancels the ORIGINAL
/// context and observes its caller's Err BEFORE releasing the pre-provider gate.
/// There is no numeric-PID, process-group or descendant-signaling cleanup.
struct StartupFence {
    gate: Option<File>,
    context: Arc<ExecutionContext>,
    host: SharedHost,
    results: mpsc::Receiver<StartResult>,
    result: Option<StartResult>,
    worker: Option<JoinHandle<()>>,
    worker_finished: Arc<AtomicBool>,
    cleanup: Option<(
        mpsc::Receiver<Result<LaunchOutcome, String>>,
        JoinHandle<()>,
    )>,
}

impl StartupFence {
    fn observe_caller(&mut self) {
        if self.result.is_none() {
            self.result = self.results.try_recv().ok();
        }
    }

    fn caller_cancelled(&self) -> bool {
        self.result.as_ref().is_some_and(Result::is_err)
    }

    fn begin_failure_cleanup(&mut self) -> Result<(), String> {
        if self.cleanup.is_some() {
            return Ok(());
        }
        let context = self.context.clone();
        let finished = self.worker_finished.clone();
        let (complete, completion) = mpsc::channel();
        let cleanup = thread::Builder::new()
            .name("fixture-original-startup-cleanup".into())
            .spawn(move || {
                // An unwind can happen even before the real owner is claimed.
                // NoProvider at that instant is not cancellation of future work.
                let deadline = Instant::now() + Duration::from_secs(8);
                let result = loop {
                    let result = context.reconcile_cleanup();
                    if result != Ok(LaunchOutcome::NoProvider) || finished.load(Ordering::Acquire) {
                        break result;
                    }
                    if Instant::now() >= deadline {
                        break Err(
                            "original startup owner did not become observable for cleanup".into(),
                        );
                    }
                    thread::sleep(Duration::from_millis(5));
                };
                let _ = complete.send(result);
            })
            .map_err(|error| error.to_string())?;
        self.cleanup = Some((completion, cleanup));
        Ok(())
    }

    fn release_after_cancellation(&mut self) -> Result<(), String> {
        self.observe_caller();
        if self.result.is_none() {
            self.result = self.results.recv_timeout(Duration::from_secs(5)).ok();
        }
        if !self.caller_cancelled() {
            // Keep this exact inode locked until the private fixture process
            // exits and closes its control sockets. Unlocking an un-cancelled
            // helper could start a provider after the test has failed.
            if let Some(gate) = self.gate.take() {
                std::mem::forget(gate);
            }
            return Err(
                "original startup caller did not return Err; pre-provider gate retained".into(),
            );
        }
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| "original startup worker panicked")?;
        }
        eprintln!(
            "Original caller Err observed while pre-provider gate remains held; releasing gate"
        );
        drop(self.gate.take());
        Ok(())
    }

    fn finish_failure_cleanup(&mut self) -> Result<(), String> {
        if let Some((completion, cleanup)) = self.cleanup.take() {
            let outcome = completion
                .recv_timeout(Duration::from_secs(8))
                .map_err(|error| format!("original cleanup did not settle: {error}"))??;
            cleanup
                .join()
                .map_err(|_| "original cleanup worker panicked")?;
            if !matches!(
                outcome,
                LaunchOutcome::NoProvider | LaunchOutcome::TeardownProved
            ) {
                return Err("original cleanup did not prove teardown".into());
            }
        }
        Ok(())
    }
}

impl Drop for StartupFence {
    fn drop(&mut self) {
        if self.gate.is_some() {
            let cleanup = self.begin_failure_cleanup();
            let released = self.release_after_cancellation();
            if let Err(error) = cleanup
                .and(released)
                .and_then(|()| self.finish_failure_cleanup())
            {
                eprintln!("Exact startup failure cleanup remains unconfirmed: {error}");
            }
        }
        self.host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .request_shutdown();
        crate::daemon::wake();
    }
}

#[derive(Debug)]
struct GateWait {
    fd: i32,
    device: u64,
    inode: u64,
}

fn flock_arguments(syscall: &str) -> Option<(i32, u64)> {
    let mut fields = syscall.split_whitespace();
    if fields.next()?.parse::<libc::c_long>().ok()? != libc::SYS_flock {
        return None;
    }
    let fd = u64::from_str_radix(fields.next()?.strip_prefix("0x")?, 16).ok()?;
    let operation = u64::from_str_radix(fields.next()?.strip_prefix("0x")?, 16).ok()?;
    Some((i32::try_from(fd).ok()?, operation))
}

fn blocked_at_gate(helper: &Spawned, gate: &File) -> Result<Option<GateWait>, String> {
    if !helper.process.is_current() || helper_exited(helper) {
        return Ok(None);
    }
    let syscall = fs::read_to_string(format!("/proc/{}/syscall", helper.process.pid))
        .map_err(|error| format!("cannot observe original helper syscall: {error}"))?;
    let Some((fd, operation)) = flock_arguments(&syscall) else {
        return Ok(None);
    };
    if operation != libc::LOCK_SH as u64 {
        return Ok(None);
    }
    // Compare two stat views of the SAME open inode. /proc/locks instead
    // prints the inode's superblock device; filesystems such as btrfs may
    // report a different device from stat for a subvolume.
    let held = gate.metadata().map_err(|error| error.to_string())?;
    let observed = fs::metadata(format!("/proc/{}/fd/{fd}", helper.process.pid))
        .map_err(|error| format!("cannot inspect original flock descriptor: {error}"))?;
    if !observed.is_file()
        || (held.dev(), held.ino()) != (observed.dev(), observed.ino())
        || !helper.process.is_current()
        || helper_exited(helper)
    {
        return Ok(None);
    }
    // The exclusive gate remains held. This exact blocking LOCK_SH on that
    // inode cannot finish, so the observed syscall proves the preflight gate.
    Ok(Some(GateWait {
        fd,
        device: observed.dev(),
        inode: observed.ino(),
    }))
}

fn preserve_gate_diagnostics(
    root: &Path,
    helper: Option<&Spawned>,
    gate: &File,
    observation: &Result<Option<GateWait>, String>,
) {
    let held = gate
        .metadata()
        .map(|metadata| (metadata.dev(), metadata.ino()));
    let mut records = vec![
        (
            "gate-observation.txt",
            Ok(format!(
                "helper={:?}\nheld={held:?}\nobservation={observation:?}\n",
                helper.map(|helper| &helper.process)
            )),
        ),
        (
            "gate-owner-fdinfo.txt",
            fs::read_to_string(format!("/proc/self/fdinfo/{}", gate.as_raw_fd())),
        ),
    ];
    let helper_pid = helper.map(|helper| helper.process.pid.to_string());
    let inode = held.as_ref().ok().map(|(_, inode)| inode.to_string());
    records.push((
        "gate-locks.txt",
        fs::read_to_string("/proc/locks").map(|locks| {
            locks
                .lines()
                .filter(|line| {
                    line.split_whitespace().any(|field| {
                        helper_pid.as_deref() == Some(field)
                            || inode
                                .as_deref()
                                .is_some_and(|inode| field.rsplit(':').next() == Some(inode))
                    })
                })
                .map(|line| format!("{line}\n"))
                .collect()
        }),
    ));
    if let Some(helper) = helper {
        let syscall = fs::read_to_string(format!("/proc/{}/syscall", helper.process.pid));
        let descriptor = syscall
            .as_ref()
            .ok()
            .and_then(|syscall| flock_arguments(syscall));
        records.push(("gate-helper-syscall.txt", syscall));
        records.push((
            "gate-helper-wchan.txt",
            fs::read_to_string(format!("/proc/{}/wchan", helper.process.pid)),
        ));
        records.push((
            "gate-helper-status.txt",
            fs::read_to_string(format!("/proc/{}/status", helper.process.pid)).map(|status| {
                status
                    .lines()
                    .filter(|line| {
                        ["State:", "Tgid:", "Pid:", "PPid:", "NSpid:"]
                            .iter()
                            .any(|prefix| line.starts_with(prefix))
                    })
                    .map(|line| format!("{line}\n"))
                    .collect()
            }),
        ));
        if let Some((fd, _)) = descriptor {
            records.push((
                "gate-helper-fdinfo.txt",
                fs::read_to_string(format!("/proc/{}/fdinfo/{fd}", helper.process.pid)),
            ));
            records.push((
                "gate-helper-fd-metadata.txt",
                fs::metadata(format!("/proc/{}/fd/{fd}", helper.process.pid)).map(|metadata| {
                    format!(
                        "fd={fd} device={} inode={} regular={}\n",
                        metadata.dev(),
                        metadata.ino(),
                        metadata.is_file()
                    )
                }),
            ));
        }
    }
    for (name, contents) in records {
        let contents =
            contents.unwrap_or_else(|error| format!("observation unavailable: {error}\n"));
        let saved = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root.join(name))
            .and_then(|mut file| file.write_all(contents.as_bytes()));
        if let Err(error) = saved {
            eprintln!("Could not preserve {name}: {error}");
        }
    }
}

fn helper_exited(helper: &Spawned) -> bool {
    let mut poll = libc::pollfd {
        fd: helper.pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert!(unsafe { libc::poll(&mut poll, 1, 0) } >= 0);
    poll.revents & libc::POLLIN != 0
}

#[derive(Debug)]
struct WhileGated {
    caller_cancelled_by_shutdown: bool,
    host_returned_before_proof: bool,
    same_startup_retained: bool,
    cleanup_still_uncertain: bool,
    helper_alive_and_blocked: bool,
    helper_has_no_children: bool,
    lease_held: bool,
    listener_present: bool,
    fresh_user_can_inspect: bool,
}

impl WhileGated {
    fn qualified(&self) -> bool {
        self.caller_cancelled_by_shutdown
            && !self.host_returned_before_proof
            && self.same_startup_retained
            && self.cleanup_still_uncertain
            && self.helper_alive_and_blocked
            && self.helper_has_no_children
            && self.lease_held
            && self.listener_present
            && self.fresh_user_can_inspect
    }
}

#[test]
fn ordinary_host_exit_cancels_and_retains_the_exact_pre_discovery_native_startup_until_reaped() {
    pre_discovery_shutdown(false);
}

#[test]
fn ordinary_host_exit_cancels_an_admitted_native_startup_before_waiting_for_control_writer() {
    pre_discovery_shutdown(true);
}

fn pre_discovery_shutdown(admitted_reader: bool) {
    let name = if admitted_reader {
        "host::exit_drain_tests::native_startup_shutdown_tests::ordinary_host_exit_cancels_an_admitted_native_startup_before_waiting_for_control_writer"
    } else {
        "host::exit_drain_tests::native_startup_shutdown_tests::ordinary_host_exit_cancels_and_retains_the_exact_pre_discovery_native_startup_until_reaped"
    };
    let Some(root) = private_runtime_retained_actor(name) else {
        return;
    };
    let paths = RuntimePaths::discover().unwrap();
    assert_ne!(
        std::env::temp_dir().canonicalize().unwrap(),
        paths.directory
    );
    let (host, completed, running) = run_host();
    let mut user = Rpc::connect(&paths.socket);
    let gate_path = paths.directory.join("transcript-cache.lock");
    let gate = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&gate_path)
        .unwrap();
    let metadata = gate.metadata().unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.uid(), crate::daemon::current_uid());
    assert_eq!(metadata.nlink(), 1);
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(
        unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );

    let mut source = provider_chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    // Existing inert ELF is only executable-validation input. The helper must
    // observe cancellation after preflight and never reach provider creation.
    // No provider protocol script or substituted helper implements the oracle.
    source.launch.recipe.program = "/usr/bin/false".into();
    source.proof = Some(user.proof.clone());
    let context = Arc::new(ExecutionContext::capture(&root.join("project")).unwrap());
    let observation = Observation::install(source.clone(), context.clone());
    let (result, results) = mpsc::channel();
    let worker_finished = Arc::new(AtomicBool::new(false));
    let mut startup = StartupFence {
        gate: Some(gate),
        context: context.clone(),
        host: host.clone(),
        results,
        result: None,
        worker: None,
        worker_finished: worker_finished.clone(),
        cleanup: None,
    };
    let starter_host = host.clone();
    let starter_source = source.clone();
    let starter_context = context.clone();
    let control_gate = host.lock().unwrap().control_gate.clone();
    startup.worker = Some(thread::spawn(move || {
        // Equivalent to the execution service's admitted-start reader. This
        // variant covers its lock mechanics, not a public ChatStart RPC.
        let _admitted = admitted_reader.then(|| control_gate.read().unwrap());
        let started =
            super::super::chat::start_execution(&starter_host, 0, starter_source, starter_context);
        worker_finished.store(true, Ordering::Release);
        let _ = result.send(started);
    }));

    let mut host_result = None;
    let mut actual_helper = None;
    let mut gate_observation = Ok(None);
    let mut shutdown_stimulated = false;
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        actual_helper = Some(
            match observation
                .events
                .recv_timeout(Duration::from_secs(8))
                .unwrap()
            {
                Event::Spawned(helper) => helper.unwrap(),
                Event::Reaped(_) => {
                    panic!("sole watcher reported a wait before the actual spawn observation")
                }
            },
        );
        let helper = actual_helper.as_ref().unwrap();
        eprintln!("Original helper spawned: process={:?}", helper.process);
        wait_for(|| {
            gate_observation = blocked_at_gate(helper, startup.gate.as_ref().unwrap());
            gate_observation
                .as_ref()
                .is_ok_and(|observed| observed.is_some())
        });
        assert!(helper.process.is_current() && !helper_exited(helper));
        let gate_wait = gate_observation.as_ref().unwrap().as_ref().unwrap();
        eprintln!(
            "Actual helper blocked on exact transcript-cache gate: process={:?}, syscall=SYS_flock, operation=LOCK_SH, fd={}, device={}, inode={}, admitted_reader={admitted_reader}",
            helper.process, gate_wait.fd, gate_wait.device, gate_wait.inode,
        );
        startup.observe_caller();
        assert!(startup.result.is_none(), "startup must still be blocked");
        assert_eq!(context.outcome(), LaunchOutcome::Uncertain);
        if admitted_reader {
            // Signal-style ordinary shutdown: the public Shutdown RPC itself
            // is an exclusive request and would wait behind this reader.
            host.lock().unwrap().request_shutdown();
            crate::daemon::wake();
        } else {
            assert!(matches!(
                user.request(ClientRequest::Shutdown).unwrap(),
                ServerResponse::ShutdownAccepted
            ));
        }
        shutdown_stimulated = true;
        // Much shorter than the transport's own 30-second startup timeout.
        // Observe the production cancellation/early-exit result before any
        // manual cleanup can close the original control socket.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            startup.observe_caller();
            host_result = completed.try_recv().ok();
            // A prompt caller Err alone is insufficient: an exit caller that
            // cancels and then immediately seals without waiting for proof is
            // still wrong. Keep observing HOST through the held-gate window.
            if host_result.is_some() || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let same_startup_retained = {
            let state = host.lock().unwrap();
            state.native_actor_startups.len() == 1
                && state
                    .native_actor_startups
                    .get(&source.id)
                    .is_some_and(|pending| pending.matches_for_test(&source, &source, &context))
                && state.chats.is_empty()
        };
        let mut snapshot = WhileGated {
            caller_cancelled_by_shutdown: startup.caller_cancelled(),
            host_returned_before_proof: host_result.is_some(),
            same_startup_retained,
            cleanup_still_uncertain: context.outcome() == LaunchOutcome::Uncertain,
            helper_alive_and_blocked: helper.process.is_current()
                && !helper_exited(helper)
                && blocked_at_gate(helper, startup.gate.as_ref().unwrap())
                    .is_ok_and(|observed| observed.is_some()),
            helper_has_no_children: fs::read_to_string(format!(
                "/proc/{0}/task/{0}/children",
                helper.process.pid,
            ))
            .unwrap()
            .trim()
            .is_empty(),
            lease_held: HostLease::is_held(&paths).unwrap(),
            listener_present: paths.socket.exists(),
            fresh_user_can_inspect: false,
        };
        if snapshot.caller_cancelled_by_shutdown
            && !snapshot.host_returned_before_proof
            && snapshot.listener_present
            && snapshot.lease_held
        {
            let mut fresh = Rpc::connect(&paths.socket);
            snapshot.fresh_user_can_inspect = matches!(fresh.request(ClientRequest::List).unwrap(),
                ServerResponse::Listed { terminals } if terminals.is_empty());
        }
        snapshot
    }));

    preserve_gate_diagnostics(
        &root,
        actual_helper.as_ref(),
        startup.gate.as_ref().unwrap(),
        &gate_observation,
    );
    let production_qualified = checked.as_ref().is_ok_and(WhileGated::qualified);
    if !production_qualified {
        if let Ok(snapshot) = &checked {
            eprintln!(
                "Observed BEFORE failure-only cleanup, while original gate held: {snapshot:?}"
            );
        }
        startup.begin_failure_cleanup().unwrap();
    }
    // Even a failing baseline MUST cancel and observe the ORIGINAL caller's
    // Err while the helper is still blocked, before allowing it past preflight.
    startup.release_after_cancellation().unwrap();
    startup.finish_failure_cleanup().unwrap();
    let waited = if actual_helper.is_none() && context.outcome() == LaunchOutcome::NoProvider {
        None
    } else {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match observation
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Event::Reaped(waited) => break Some(waited.unwrap()),
                Event::Spawned(helper) => {
                    assert!(actual_helper.is_none(), "duplicate actual helper spawn");
                    actual_helper = Some(helper.unwrap());
                    eprintln!(
                        "Original helper observed during failure cleanup: process={:?}",
                        actual_helper.as_ref().unwrap().process
                    );
                }
            }
        }
    };
    if let Some(helper) = &actual_helper {
        let waited = waited.expect("observed original helper requires its sole Child.wait");
        eprintln!(
            "Original sole Child.wait observed: process={:?}, status={waited}",
            helper.process
        );
        assert_eq!(
            waited.code(),
            Some(73),
            "actual sole Child.wait must prove helper teardown"
        );
        assert!(helper_exited(helper));
        assert!(
            match Process::read(helper.process.pid) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                Ok(current) => current != helper.process,
                Err(error) => panic!("could not observe original Child reaping: {error}"),
            },
            "kernel exit alone is not the original Child.wait"
        );
    } else {
        assert_eq!(context.outcome(), LaunchOutcome::NoProvider);
        eprintln!(
            "Failure-only startup cleanup settled without spawning a helper; no Child.wait status claimed"
        );
    }
    if !shutdown_stimulated {
        assert!(!production_qualified);
        host.lock().unwrap().request_shutdown();
        crate::daemon::wake();
        eprintln!(
            "Failure-only fixture HOST shutdown requested after owned cleanup; production shutdown request was not completed or acknowledged"
        );
    }
    if host_result.is_none() {
        host_result = Some(completed.recv_timeout(Duration::from_secs(8)).unwrap());
    }
    running.join().unwrap();
    assert!(host_result.unwrap().is_ok());
    assert!(!paths.socket.exists());
    assert!(!HostLease::is_held(&paths).unwrap());
    if let Some(helper) = actual_helper {
        eprintln!(
            "Original cleanup settled: process={:?}, failure_only_cleanup={}, sole_Child_wait_exit=73, pidfd_exited=true, original_reaped=true, actual_host_returned=true, lease_released=true",
            helper.process, !production_qualified,
        );
    } else {
        eprintln!(
            "Failure-only fixture settled: no helper spawned, actual_host_returned=true, listener_removed=true, lease_released=true"
        );
    }
    // Setup panics retain their original meaning, but only AFTER the original
    // helper and actual HOST have been proved settled and the receipt emitted.
    let snapshot = match checked {
        Ok(snapshot) => snapshot,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    assert!(
        snapshot.qualified(),
        "ordinary shutdown violated the pre-cleanup oracle: {snapshot:?}"
    );
    assert_eq!(context.outcome(), LaunchOutcome::TeardownProved);
    assert!(
        host.lock().unwrap().native_actor_startups.is_empty(),
        "production shutdown must release the exact startup only after proof"
    );
}
