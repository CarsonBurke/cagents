//! Combined startup-custody/ordinary-exit controls. The original holder is
//! created in a separate, naturally exiting fixture process, so its adopter
//! has no local reaper credit that could mask the final startup-progress wake.
use super::*;
use crate::daemon::holder_registry::Record;
use serde::{Deserialize, Serialize};

const ORIGIN: &str = "CAGENTS_STARTUP_EXIT_ORIGIN_COUNT";

#[derive(Serialize, Deserialize)]
struct Original {
    session: TerminalSession,
    owner: Record,
}

fn registry_path() -> PathBuf {
    crate::instance::project_dirs()
        .unwrap()
        .data_dir()
        .join("workspace-model.sqlite3")
}

fn origin_body(root: &Path, count: usize) {
    assert!((1..=2).contains(&count));
    let paths = RuntimePaths::discover().unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("startup-exit-origin"),
        host_incarnation: HostIncarnation::new("original"),
        boot_id: daemon::read_boot_id().unwrap(),
    };
    let lease = HostLease::acquire(&paths, &proof).unwrap();
    let mut state = HostState::new(proof);
    state.enable_holder_recovery();
    state.register_client(0);
    state.install_model(model::Registry::open().unwrap());
    let host = Arc::new(state::HostLock::new(state));
    let mut originals = Vec::new();
    let mut handles = Vec::new();
    for index in 0..count {
        let directory = root.join(format!("original-{index}"));
        fs::create_dir(&directory).unwrap();
        let mut source = session(&directory);
        // One actual process, no shell descendants or provider invocation.
        source.launch.program = "/usr/bin/python3".into();
        source.launch.arguments = vec![
            "-u".into(),
            "-c".into(),
            "import pathlib,time\nwith open('launches','a') as f: f.write('launch\\n')\nprint('ORIGINAL_READY',flush=True)\nwhile not pathlib.Path('release').exists(): time.sleep(.005)".into(),
        ];
        source.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: source.launch.clone(),
            last_exit: None,
        };
        state::spawn_terminal(&host, 0, source.clone(), TerminalSpawnOptions::default()).unwrap();
        let (handle, issued) = {
            let state = host.lock().unwrap();
            (
                state.terminals[&source.id].handle.clone(),
                state.descriptor(0, source.id).unwrap().session,
            )
        };
        wait_for(|| directory.join("launches").exists());
        let owner = handle.holder_identity().unwrap().clone();
        let holders = crate::terminal::holder::directory().unwrap();
        handle
            .carry(crate::terminal::holder::socket(&holders, source.id))
            .unwrap();
        assert!(owner.root.is_current() && owner.holder.is_current());
        originals.push(Original {
            session: issued,
            owner,
        });
        handles.push(handle);
    }
    // These are actual issued sessions and actual holder records, not synthetic
    // adoption/cleanup receipts. Parent pins both original epochs before exit.
    fs::write(
        root.join("origin.json"),
        serde_json::to_vec(&originals).unwrap(),
    )
    .unwrap();
    wait_for(|| root.join("origin-exit").exists());
    drop(lease);
    // Carry has already completed its real detach. Natural fixture-process
    // exit leaves its genuine original holders available for orphan recovery.
    drop(handles);
}

fn assert_waitable_child_signal() {
    // A handler could add an unrelated Host poll wake, and NOCLDWAIT would
    // discard the status this fixture must actually reap. Observe, never reset.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) },
        0
    );
    assert_eq!(action.sa_sigaction, libc::SIG_DFL);
    assert_eq!(action.sa_flags & libc::SA_NOCLDWAIT, 0);
}

fn foreign_originals(
    root: &Path,
    name: &str,
    count: usize,
) -> (Vec<Original>, Vec<(OriginalPin, OriginalPin)>) {
    // This function runs only in the exact private_runtime self-child. Become
    // the reaper of this fixture's orphaned holder before its separate origin
    // exits; namespace init is not obligated to wait within our test deadline.
    // This creates no child_reaper reservation or caller-local cleanup credit.
    assert!(std::env::var_os(ROOT).is_some());
    assert_waitable_child_signal();
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
        0
    );
    let stdout = root.join("origin.stdout");
    let stderr = root.join("origin.stderr");
    let mut command = Command::new(std::env::current_exe().unwrap());
    // The already isolated caller's HOME/authority/private paths are inherited
    // unchanged. private_runtime verifies the same public presence boolean in
    // this additional same-function origin child before any fixture body.
    command
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(ORIGIN, count.to_string())
        .stdin(Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap());
    let mut origin = TestChild(command.spawn().unwrap());
    wait_for(|| root.join("origin.json").exists());
    let originals: Vec<Original> =
        serde_json::from_slice(&fs::read(root.join("origin.json")).unwrap()).unwrap();
    assert_eq!(originals.len(), count);
    let pins = originals
        .iter()
        .map(|original| {
            assert_eq!(original.owner.terminal, original.session.id);
            let root = OriginalPin::new(original.owner.root.pid as u32);
            let holder = OriginalPin::new(original.owner.holder.pid as u32);
            assert_eq!(root.process, original.owner.root);
            assert_eq!(holder.process, original.owner.holder);
            (root, holder)
        })
        .collect();
    fs::write(root.join("origin-exit"), b"exit naturally").unwrap();
    wait_for(|| origin.0.try_wait().unwrap().is_some());
    let status = origin.0.wait().unwrap();
    let out = fs::read_to_string(&stdout).unwrap();
    let err = fs::read_to_string(&stderr).unwrap();
    assert!(
        status.success(),
        "actual origin failed: {status}\n{out}\n{err}"
    );
    assert!(out.contains(&format!("test {name} ... ok")));
    assert!(out.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"));
    for original in &originals {
        assert_eq!(
            Process::read(original.owner.holder.pid).unwrap(),
            original.owner.holder
        );
        let stat = fs::read_to_string(format!("/proc/{}/stat", original.owner.holder.pid)).unwrap();
        let fields = stat.rsplit_once(')').unwrap().1.split_whitespace();
        assert_eq!(
            fields.skip(1).next().unwrap().parse::<u32>().unwrap(),
            std::process::id(),
            "the isolated fixture must own its orphaned holder's final wait"
        );
    }
    assert!(
        !child_reaper::has_outstanding_ownership(),
        "foreign original is not a caller-local reaper credit"
    );
    (originals, pins)
}

fn same_original(host: &SharedHost, original: &Original) -> crate::terminal::TerminalHandle {
    let handle = host.lock().unwrap().terminals[&original.session.id]
        .handle
        .clone();
    assert_eq!(handle.holder_identity(), Some(&original.owner));
    assert!(original.owner.root.is_current() && original.owner.holder.is_current());
    assert_eq!(
        fs::read_to_string(original.session.launch.working_directory.join("launches")).unwrap(),
        "launch\n",
        "successor must not execute the recipe again"
    );
    handle
}

fn inspect_and_refuse_mutation(rpc: &mut Rpc, original: &Original, root: &Path) {
    let id = original.session.id;
    assert!(matches!(rpc.request(ClientRequest::List).unwrap(),
        ServerResponse::Listed { terminals } if terminals.iter().any(|terminal| terminal.session.id == id)));
    assert!(
        matches!(rpc.request(ClientRequest::Attach { terminal_id: id,
        after_generation: 0, after_event_sequence: 0 }).unwrap(),
        ServerResponse::Attached { terminal } if terminal.session.id == id)
    );
    assert!(
        matches!(rpc.request(ClientRequest::Snapshot { terminal_id: id,
        after_generation: 0 }).unwrap(),
        ServerResponse::Snapshot { terminal_id, .. } if terminal_id == id)
    );
    for request in [
        ClientRequest::Spawn {
            session: session(root),
            options: TerminalSpawnOptions::default(),
        },
        ClientRequest::Input {
            terminal_id: id,
            bytes: b"must not execute".to_vec(),
        },
        ClientRequest::Resume {
            terminal_id: id,
            visible: true,
        },
        ClientRequest::Shutdown,
    ] {
        assert_eq!(
            rpc.request(request).unwrap_err().code,
            ProtocolErrorCode::TerminalUnavailable
        );
    }
    assert_eq!(
        rpc.request(ClientRequest::CloseTerminalChecked {
            terminal_id: TerminalId::new(),
            allow_active: true,
        })
        .unwrap_err()
        .code,
        ProtocolErrorCode::PermissionDenied
    );
    assert!(original.owner.root.is_current() && original.owner.holder.is_current());
}

fn stale_close_is_refused(paths: &RuntimePaths, original: &Original) {
    let mut stale = Rpc::connect(&paths.socket);
    let current = stale.proof.clone();
    let mut old = current.clone();
    old.host_incarnation = HostIncarnation::new("wrong-original-incarnation");
    let id = stale.next;
    write_frame(
        &mut stale.stream,
        &ClientEnvelope {
            version: PROTOCOL_VERSION,
            request_id: id,
            proof: Some(old),
            request: ClientRequest::CloseTerminalChecked {
                terminal_id: original.session.id,
                allow_active: true,
            },
        },
    )
    .unwrap();
    loop {
        let envelope: ServerEnvelope = read_frame(&mut stale.stream).unwrap().unwrap();
        assert_eq!(envelope.proof, current);
        if envelope.request_id != Some(id) {
            continue;
        }
        assert!(matches!(envelope.message, ServerMessage::Error(error)
            if error.code == ProtocolErrorCode::ProofMismatch));
        break;
    }
    assert!(original.owner.root.is_current() && original.owner.holder.is_current());
}

fn prove_foreign_reaped(pin: &mut OriginalPin) {
    assert_waitable_child_signal();
    // The original root is normally reaped by its holder. The holder becomes
    // this isolated fixture's child when the foreign origin exits. Consume only
    // that captured original's status, never an arbitrary child or reused PID.
    wait_for(|| {
        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                pin.pidfd.as_raw_fd() as libc::id_t,
                &mut status,
                libc::WEXITED | libc::WNOHANG,
            )
        };
        if result == 0 {
            let observed = unsafe { status.si_pid() };
            assert!(observed == 0 || observed == pin.process.pid);
        } else {
            let error = io::Error::last_os_error();
            assert!(
                error.raw_os_error() == Some(libc::ECHILD)
                    || error.kind() == io::ErrorKind::Interrupted,
                "exact foreign original wait failed: {error}"
            );
        }
        match Process::read(pin.process.pid) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Ok(current) => current != pin.process,
            Err(error) => panic!("original reaping could not be observed: {error}"),
        }
    });
    pin.prove_reaped();
}

fn prove_reply_admission_closed(host: &SharedHost) {
    let mut state = host.lock().unwrap();
    let count = state.terminals.len();
    assert_eq!(
        state.begin_checked_close_reply(u64::MAX).unwrap_err().code,
        ProtocolErrorCode::TerminalUnavailable,
        "sealed exit must refuse a new reply obligation before any handler effects"
    );
    assert_eq!(state.terminals.len(), count);
}

fn prove_released(paths: &RuntimePaths) {
    assert!(!paths.socket.exists());
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&paths.lock)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "actual HostLease must have returned its kernel lock"
    );
}

#[test]
fn startup_recovery_and_exit_pending_keep_exact_close_available_until_foreign_owner_retires() {
    const NAME: &str = "host::exit_drain_tests::startup_recovery_tests::startup_recovery_and_exit_pending_keep_exact_close_available_until_foreign_owner_retires";
    let Some(root) = private_runtime(NAME) else {
        return;
    };
    if let Ok(count) = std::env::var(ORIGIN) {
        origin_body(&root, count.parse().unwrap());
        return;
    }
    let (originals, mut pins) = foreign_originals(&root, NAME, 1);
    let original = &originals[0];
    // Real SQLite layout damage makes the actual successor's model admission
    // unavailable; orphan adoption has already installed its exact live owner
    // before execution-authority restoration refuses. No fake error seam.
    let fault = rusqlite::Connection::open(registry_path()).unwrap();
    fault
        .execute_batch("ALTER TABLE agent_authorities RENAME TO unavailable_agent_authorities")
        .unwrap();
    let paths = RuntimePaths::discover().unwrap();
    let (host, completed, running) = run_host();
    let handle = same_original(&host, original);
    assert!(host.lock().unwrap().startup_blocks_shutdown());
    assert!(host.lock().unwrap().startup_recovery().is_some());
    let drain = begin_ordinary_exit(&host).expect("ordinary, not committed successor transfer");
    daemon::wake();
    wait_for(|| host.lock().unwrap().ordinary_exit_cleanup_pending());
    assert!(
        drain.try_seal_absence(),
        "foreign adoption has no local spawn ticket"
    );
    assert!(
        completed.try_recv().is_err(),
        "empty core is not startup absence"
    );
    assert_lease_held(&paths);
    let mut rpc = Rpc::connect(&paths.socket);
    inspect_and_refuse_mutation(&mut rpc, original, &root);
    stale_close_is_refused(&paths, original);
    assert!(
        host.lock().unwrap().terminals[&original.session.id]
            .handle
            .same_worker(&handle)
    );
    assert!(completed.recv_timeout(Duration::from_millis(50)).is_err());
    assert!(matches!(rpc.request(ClientRequest::CloseTerminalChecked {
        terminal_id: original.session.id, allow_active: true,
    }).unwrap(), ServerResponse::TerminalCloseChecked {
        terminal_id, outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
    } if terminal_id == original.session.id));
    // Keep this exact correlated-Close connection open through real Host.run
    // completion: neither client EOF nor a reaper ticket can supply its wake.
    assert!(
        completed
            .recv_timeout(Duration::from_secs(8))
            .unwrap()
            .is_ok()
    );
    running.join().unwrap();
    assert!(handle.worker_joined_for_test());
    prove_foreign_reaped(&mut pins[0].0);
    prove_foreign_reaped(&mut pins[0].1);
    assert!(!host.lock().unwrap().startup_blocks_shutdown());
    prove_released(&paths);
    prove_reply_admission_closed(&host);
    drop(rpc);
}

#[test]
fn final_foreign_worker_completion_wakes_exit_after_other_recovery_owner_checked_close() {
    const NAME: &str = "host::exit_drain_tests::startup_recovery_tests::final_foreign_worker_completion_wakes_exit_after_other_recovery_owner_checked_close";
    let Some(root) = private_runtime(NAME) else {
        return;
    };
    if let Ok(count) = std::env::var(ORIGIN) {
        origin_body(&root, count.parse().unwrap());
        return;
    }
    let (originals, mut pins) = foreign_originals(&root, NAME, 2);
    let paths = RuntimePaths::discover().unwrap();
    let (host, completed, running) = run_host();
    let active = same_original(&host, &originals[0]);
    let before_recovery = same_original(&host, &originals[1]);
    // Detach this same actor through the real carry protocol, then exercise a
    // second actual orphan restoration with the registry connection now broken.
    // All setup/adoption finishes BEFORE ordinary exit closes new admission.
    let holders = crate::terminal::holder::directory().unwrap();
    before_recovery
        .carry(crate::terminal::holder::socket(
            &holders,
            originals[1].session.id,
        ))
        .unwrap();
    host.lock()
        .unwrap()
        .retire_terminal(originals[1].session.id)
        .unwrap();
    let fault = rusqlite::Connection::open(registry_path()).unwrap();
    fault
        .execute_batch("ALTER TABLE agent_authorities RENAME TO unavailable_agent_authorities")
        .unwrap();
    assert!(state::restore_orphaned_holders(&host).is_err());
    let retained = same_original(&host, &originals[1]);
    assert!(host.lock().unwrap().startup_blocks_shutdown());
    let drain = begin_ordinary_exit(&host).unwrap();
    daemon::wake();
    assert!(
        drain.try_seal_absence(),
        "both original holders belong to the foreign origin"
    );
    assert!(completed.try_recv().is_err());
    assert_lease_held(&paths);
    let mut rpc = Rpc::connect(&paths.socket);
    inspect_and_refuse_mutation(&mut rpc, &originals[1], &root);
    stale_close_is_refused(&paths, &originals[1]);
    assert!(matches!(rpc.request(ClientRequest::CloseTerminalChecked {
        terminal_id: originals[1].session.id, allow_active: true,
    }).unwrap(), ServerResponse::TerminalCloseChecked {
        terminal_id, outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
    } if terminal_id == originals[1].session.id));
    assert!(retained.worker_joined_for_test());
    prove_foreign_reaped(&mut pins[1].0);
    prove_foreign_reaped(&mut pins[1].1);
    assert!(
        host.lock().unwrap().startup_blocks_shutdown(),
        "the other exact owner still runs"
    );
    assert!(completed.recv_timeout(Duration::from_millis(50)).is_err());
    assert_lease_held(&paths);
    assert!(
        host.lock().unwrap().terminals[&originals[0].session.id]
            .handle
            .same_worker(&active)
    );
    // No Stop, Close, client disconnect or new recipe execution for A. Its
    // actual child exits naturally, publishing Completed on its existing worker.
    fs::write(
        originals[0]
            .session
            .launch
            .working_directory
            .join("release"),
        b"exit naturally",
    )
    .unwrap();
    assert!(
        completed
            .recv_timeout(Duration::from_secs(8))
            .unwrap()
            .is_ok()
    );
    running.join().unwrap();
    assert!(active.worker_joined_for_test());
    prove_foreign_reaped(&mut pins[0].0);
    prove_foreign_reaped(&mut pins[0].1);
    assert!(!host.lock().unwrap().startup_blocks_shutdown());
    prove_released(&paths);
    prove_reply_admission_closed(&host);
    drop(rpc);
    drop(before_recovery);
}
