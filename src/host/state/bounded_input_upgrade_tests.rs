//! Real HOST admission and stable-input-cut regressions. Private children retain
//! opaque inherited delegation. Only the private HOST fixture client is exercised; no provider runs.
use super::*;
use crate::{
    daemon::holder_registry::Record,
    model::{HostIncarnation, HostSessionId},
    terminal::PasteOrigin,
    terminal_ipc::{ClientRequest, ServerResponse, TerminalCloseResult},
    upgrade::handoff::{Phase, Store},
};
use std::{
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::fs::PermissionsExt,
};

const FIXTURE: &str = "CAGENTS_INPUT_UPGRADE_FIXTURE";
const FIXTURE_TEST: &str = "CAGENTS_INPUT_UPGRADE_TEST";
const FIXTURE_SCOPE: &str = "CAGENTS_INPUT_UPGRADE_SCOPE";
struct PrivateChild(std::process::Child);
impl Drop for PrivateChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
struct EvidenceDirectory(Option<tempfile::TempDir>);
impl Drop for EvidenceDirectory {
    fn drop(&mut self) {
        if let Some(directory) = self.0.take() {
            let _ = directory.keep();
        }
    }
}
fn isolated(name: &str) -> bool {
    if let Some(root) = std::env::var_os(FIXTURE) {
        assert_eq!(std::env::var(FIXTURE_TEST).as_deref(), Ok(name));
        let expected = match std::env::var(FIXTURE_SCOPE).as_deref() {
            Ok("present") => true,
            Ok("absent") => false,
            _ => panic!("missing inherited authority presence receipt"),
        };
        assert_eq!(
            crate::agent_execution::Authority::from_environment()
                != crate::agent_execution::Authority::default(),
            expected
        );
        assert_eq!(
            std::env::current_dir().unwrap(),
            PathBuf::from(root).canonicalize().unwrap()
        );
        return false;
    }
    let directory = tempfile::Builder::new().prefix("ic-").tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let _evidence = EvidenceDirectory(Some(directory));
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["home", "run", "data", "config", "cache", "tmp"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let selector = format!("host::state::bounded_input_upgrade_tests::{name}");
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    for (key, _) in std::env::vars_os() {
        if key != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV)
            && key != std::ffi::OsStr::new("CAGENTS_TEST_RETAINED_ACTOR_HELPER")
        {
            command.env_remove(key);
        }
    }
    command
        .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
        .current_dir(&root)
        .env(FIXTURE, &root)
        .env(FIXTURE_TEST, name)
        .env(
            FIXTURE_SCOPE,
            if crate::agent_execution::Authority::from_environment()
                != crate::agent_execution::Authority::default()
            {
                "present"
            } else {
                "absent"
            },
        )
        .env("HOME", root.join("home"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root.join("tmp"))
        .env("PATH", "/usr/bin:/bin")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(
            File::create(root.join("test.stdout")).unwrap(),
        ))
        .stderr(std::process::Stdio::from(
            File::create(root.join("test.stderr")).unwrap(),
        ));
    let mut child = PrivateChild(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "private HOST input child timeout: {}",
            root.display()
        );
        thread::sleep(Duration::from_millis(10));
    };
    let output = fs::read_to_string(root.join("test.stdout")).unwrap();
    let error = fs::read_to_string(root.join("test.stderr")).unwrap();
    assert!(
        status.success(),
        "private HOST input child failed: {output}\n{error}"
    );
    assert!(
        output.contains(&format!("test {selector} ... ok")),
        "{output}"
    );
    assert!(
        output.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "{output}"
    );
    true
}

// Capabilities are opened for recorded original processes, then their complete
// process epochs are rechecked. Normal success disarms only after actual exit.
struct OriginalProcesses {
    fds: Vec<File>,
    armed: bool,
}
fn exited(fd: &File) -> bool {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let result = unsafe { libc::poll(&mut poll, 1, 0) };
        if result >= 0 {
            return poll.revents & libc::POLLIN != 0;
        }
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::Interrupted
        );
    }
}
impl OriginalProcesses {
    fn pin(record: &Record) -> Self {
        let mut pins = Self {
            fds: Vec::new(),
            armed: true,
        };
        for process in [&record.root, &record.holder] {
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, process.pid, 0) };
            assert!(fd >= 0, "cannot pin original fixture process");
            pins.fds.push(unsafe { File::from_raw_fd(fd as i32) });
            assert!(
                process.is_current(),
                "fixture epoch changed during capability capture"
            );
        }
        pins
    }
    fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.fds.iter().any(|fd| !exited(fd)) {
            assert!(
                Instant::now() < deadline,
                "original root/holder retirement remains unconfirmed"
            );
            thread::sleep(Duration::from_millis(5));
        }
        self.armed = false;
    }
}
impl Drop for OriginalProcesses {
    fn drop(&mut self) {
        if self.armed {
            for fd in &self.fds {
                if !exited(fd) {
                    // Failure-only cleanup for the original pinned fixture.
                    unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            fd.as_raw_fd(),
                            libc::SIGCONT,
                            std::ptr::null::<libc::siginfo_t>(),
                            0,
                        );
                    }
                    unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            fd.as_raw_fd(),
                            libc::SIGKILL,
                            std::ptr::null::<libc::siginfo_t>(),
                            0,
                        );
                    }
                }
            }
        }
    }
}
fn proof(name: &str) -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new(name),
        host_incarnation: HostIncarnation::new("original"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
    }
}
fn host(proof: HostSessionProof) -> SharedHost {
    let mut state = HostState::new(proof);
    state.enable_holder_recovery();
    state.register_client(0);
    Arc::new(HostLock::new(state))
}
fn source(root: &std::path::Path, script: &str) -> TerminalSession {
    let recipe = crate::model::LaunchRecipe {
        program: "/usr/bin/python3".into(),
        arguments: vec!["-u".into(), "-c".into(), script.into()],
        working_directory: root.to_owned(),
        environment: BTreeMap::new(),
        unset_environment: Default::default(),
    };
    TerminalSession {
        id: TerminalId::new(),
        launch: recipe.clone(),
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
        resume_hint: None,
        transcript_path: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
    }
}
fn spawn(host: &SharedHost, source: &TerminalSession) -> TerminalHandle {
    spawn_terminal(host, 0, source.clone(), TerminalSpawnOptions::default()).unwrap();
    host.lock().unwrap().attach(0, source.id).unwrap();
    host.lock().unwrap().terminals[&source.id].handle.clone()
}
fn wait_file(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "child marker missing: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn wait_text(handle: &TerminalHandle, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = handle.snapshot();
        handle.acknowledge_redraw(snapshot.generation);
        let text = snapshot
            .lines
            .iter()
            .map(|line| {
                line.cells
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.contains(expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "missing actual parsed child output {expected:?}: {text}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
// Exact query is emitted only after a private file gate. The child itself
// records emission and consumes the real PTY reply; no synthetic event/frame.
const QUERY_CHILD: &str = r#"
import os, pathlib, time, tty
tty.setraw(0)
with open('launches', 'a') as f: f.write('launch\n')
os.write(1, b'STABLE_INPUT_READY\x1b[H')
while not pathlib.Path('query-gate').exists(): time.sleep(.005)
os.write(1, b'\x1b[6n')
pathlib.Path('query-emitted').write_bytes(b'emitted')
reply = bytearray()
while not reply.endswith(b'R'):
    reply.extend(os.read(0, 64))
pathlib.Path('reply-received.tmp').write_bytes(reply)
os.replace('reply-received.tmp', 'reply-received')
os.write(1, b'REPLY_CONSUMED')
while True: time.sleep(1)
"#;

#[test]
fn host_paste_reports_retryable_queue_full_without_losing_interrupt_or_child_output() {
    if isolated("host_paste_reports_retryable_queue_full_without_losing_interrupt_or_child_output")
    {
        return;
    }
    let root = std::env::current_dir().unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = proof("input-queue-full");
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let host = host(proof);
    let source = source(
        &root,
        r#"
import os, pathlib, signal, time, tty
tty.setraw(0)
def stop(sig, frame):
    os.write(1, b'HOST_INTERRUPTED')
    raise SystemExit(0)
signal.signal(signal.SIGINT, stop)
# ISIG is needed for the actual raw Input([3]) priority interrupt.
import termios
attrs=termios.tcgetattr(0); attrs[3] |= termios.ISIG; termios.tcsetattr(0, termios.TCSANOW, attrs)
os.write(1, b'HOST_READY\r\n')
while True:
    if pathlib.Path('output-gate').exists(): os.write(1, b'HOST_POST_PASTE_OUTPUT\r\n')
    time.sleep(.05)
"#,
    );
    let handle = spawn(&host, &source);
    let owner = handle.holder_identity().unwrap().clone();
    let mut roots = OriginalProcesses::pin(&owner);
    wait_text(&handle, "HOST_READY");
    let mut accepted = 0;
    let mut full = 0;
    for _ in 0..3 {
        match super::super::client::process_request(
            &host,
            0,
            ClientRequest::Paste {
                terminal_id: source.id,
                text: "x".repeat(1024 * 1024),
                origin: PasteOrigin::Clipboard,
            },
        ) {
            Ok((ServerResponse::Accepted, events, close)) => {
                assert!(events.is_empty());
                assert!(!close);
                accepted += 1;
            }
            Err(error) => {
                assert_eq!(error.code, ProtocolErrorCode::QueueFull);
                assert!(error.retryable);
                assert_eq!(error.message, "the terminal command queue is full");
                full += 1;
            }
            Ok(other) => panic!("unexpected actual HOST paste response: {other:?}"),
        }
    }
    assert!(accepted >= 1, "reject-all is not backpressure");
    assert!(
        full >= 1,
        "actual HOST must expose Full rather than silently accept/drop"
    );
    fs::write(root.join("output-gate"), b"emit").unwrap();
    wait_text(&handle, "HOST_POST_PASTE_OUTPUT");
    let (reply, _, _) = super::super::client::process_request(
        &host,
        0,
        ClientRequest::Input {
            terminal_id: source.id,
            bytes: vec![3],
        },
    )
    .unwrap();
    assert!(matches!(reply, ServerResponse::Accepted));
    wait_text(&handle, "HOST_INTERRUPTED");
    handle.shutdown().unwrap();
    assert!(handle.worker_joined_for_test());
    roots.settle();
}

#[test]
fn exact_input_guard_rollback_restarts_reader_and_old_token_cannot_cancel_new_cut() {
    if isolated("exact_input_guard_rollback_restarts_reader_and_old_token_cannot_cancel_new_cut") {
        return;
    }
    let root = std::env::current_dir().unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = proof("input-guard-rollback");
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let host = host(proof);
    let source = source(&root, QUERY_CHILD);
    let handle = spawn(&host, &source);
    let owner = handle.holder_identity().unwrap().clone();
    let mut roots = OriginalProcesses::pin(&owner);
    wait_text(&handle, "STABLE_INPUT_READY");
    let first = PreparedTerminalInput::prepare(handle.clone()).unwrap();
    let old_token = first.token.clone();
    drop(first); // Guard waits for actual reader restart acknowledgement.
    let second = PreparedTerminalInput::prepare(handle.clone()).unwrap();
    assert!(!Arc::ptr_eq(&old_token, &second.token));
    handle.cancel_input_handoff(&old_token);
    fs::write(root.join("query-gate"), b"query").unwrap();
    wait_file(&root.join("query-emitted"));
    // Query cannot be consumed during this still-active cut. Observe the
    // entire finite interval, rather than infer success from input rejection.
    let until = Instant::now() + Duration::from_millis(100);
    while Instant::now() < until {
        assert!(second.token.load(Ordering::Acquire));
        assert!(matches!(
            second.resumed.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ));
        assert!(!root.join("reply-received").exists());
        thread::sleep(Duration::from_millis(5));
    }
    drop(second);
    wait_file(&root.join("reply-received"));
    assert_eq!(fs::read(root.join("reply-received")).unwrap(), b"\x1b[1;1R");
    wait_text(&handle, "REPLY_CONSUMED");
    assert_eq!(handle.close_checked(true), TerminalCloseResult::Closed);
    roots.settle();
}

#[test]
fn late_child_reply_survives_committed_carry_and_final_save_failures_without_recipe_restart() {
    if isolated(
        "late_child_reply_survives_committed_carry_and_final_save_failures_without_recipe_restart",
    ) {
        return;
    }
    let root = std::env::current_dir().unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let mut proof = proof("input-upgrade-custody");
    let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
    for (index, fault) in ["carry", "save", "none"].into_iter().enumerate() {
        let case = root.join(format!("case-{index}"));
        fs::create_dir(&case).unwrap();
        let original = host(proof.clone());
        let source = source(&case, QUERY_CHILD);
        let handle = spawn(&original, &source);
        let owner = handle.holder_identity().unwrap().clone();
        let mut roots = OriginalProcesses::pin(&owner);
        wait_text(&handle, "STABLE_INPUT_READY");
        let checkpoint = Store::discover().unwrap().path.parent().unwrap().to_owned();
        let holders = holder_directory().unwrap();
        let capability = || {
            original
                .lock()
                .unwrap()
                .shutdown_preflight(Instant::now())
                .unwrap()
                .capability
        };
        // This early filesystem refusal happens before any reader cut.
        let occupied = case.join("occupied-holder-directory");
        fs::write(&occupied, b"occupied").unwrap();
        let refusal = prepare_upgrade_with_carry_policy(
            &original,
            capability(),
            false,
            Some(&occupied),
            true,
            || Store::open(&checkpoint),
        )
        .unwrap_err();
        assert!(refusal.message.contains("not committed"));
        assert!(!original.lock().unwrap().shutting_down);
        assert!(owner.root.is_current());
        // Discovery is reached only AFTER the real carried reader is cut.
        // Its injected failure must drop the host-owned exact guard and wait
        // for rollback; a fresh cut then proves admission was actually reopened.
        let mut reached_after_cut = false;
        let refusal = prepare_upgrade_with_carry_policy(
            &original,
            capability(),
            false,
            Some(&holders),
            true,
            || {
                reached_after_cut = true;
                Err("injected checkpoint discovery failure after reader cut".into())
            },
        )
        .unwrap_err();
        assert!(reached_after_cut);
        assert!(
            refusal
                .message
                .contains("injected checkpoint discovery failure after reader cut")
        );
        assert!(!original.lock().unwrap().shutting_down);
        assert!(!original.lock().unwrap().has_committed_upgrade());
        let rollback_probe = PreparedTerminalInput::prepare(handle.clone()).unwrap();
        drop(rollback_probe);
        assert!(owner.root.is_current());
        assert!(owner.holder.is_current());
        prepare_upgrade_with_carry_policy(
            &original,
            capability(),
            false,
            Some(&holders),
            true,
            || Store::open(&checkpoint),
        )
        .unwrap();
        {
            let state = original.lock().unwrap();
            let cuts = &state.upgrade.as_ref().unwrap().prepared_input;
            assert_eq!(cuts.len(), 1);
            assert!(cuts[0].token.load(Ordering::Acquire));
        }
        let store = Store::open(&checkpoint).unwrap();
        let prepared = store.load().unwrap().unwrap();
        assert!(prepared.terminals.is_empty());
        assert_eq!(prepared.carried.len(), 1);
        assert!(prepared.preserve_carried);
        let encoded: serde_json::Value =
            serde_json::from_slice(&fs::read(&store.path).unwrap()).unwrap();
        assert_eq!(encoded["version"], 4);
        fs::write(case.join("query-gate"), b"query").unwrap();
        wait_file(&case.join("query-emitted"));
        assert!(
            !case.join("reply-received").exists(),
            "post-cut query was consumed by old reader"
        );
        if fault == "carry" {
            original.lock().unwrap().upgrade.as_mut().unwrap().holders =
                Some(case.join("wrong-holder-directory"));
        } else if fault == "save" {
            fs::rename(&store.path, checkpoint.join("prepared-backup.json")).unwrap();
            fs::create_dir(&store.path).unwrap();
        }
        let (ordinary, _) = original.lock().unwrap().graceful_shutdown();
        let outcome = super::super::finish_chat_shutdown(&original);
        assert_eq!(outcome.is_err(), fault != "none", "{outcome:?}");
        // Exercise the ordinary outer shutdown tail after the actual preserve
        // path, including its committed failure cases; it must not kill owner.
        for terminal in ordinary {
            let _ = terminal.shutdown();
        }
        assert!(owner.root.is_current(), "{fault}: continuing root lost");
        assert!(owner.holder.is_current());
        if fault == "save" {
            let retained: crate::upgrade::handoff::Checkpoint =
                serde_json::from_slice(&fs::read(checkpoint.join("prepared-backup.json")).unwrap())
                    .unwrap();
            assert!(retained.terminals.is_empty());
            assert!(retained.preserve_carried);
            fs::remove_dir(&store.path).unwrap();
            fs::rename(checkpoint.join("prepared-backup.json"), &store.path).unwrap();
        } else {
            let retained = store.load().unwrap().unwrap();
            assert!(retained.terminals.is_empty());
            assert_eq!(
                retained.phase,
                if fault == "none" {
                    Phase::Ready
                } else {
                    Phase::Prepared
                }
            );
        }
        drop(original);
        drop(lease.take());
        proof.host_incarnation = HostIncarnation::new(format!("successor-{index}"));
        lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
        let mut next = HostState::new(proof.clone());
        next.enable_holder_recovery();
        next.install_model(
            crate::host::model::Registry::at(&case.join("registry.sqlite3")).unwrap(),
        );
        let successor = Arc::new(HostLock::new(next));
        restore_upgrade(&successor).unwrap();
        restore_orphaned_holders(&successor).unwrap();
        let adopted = successor.lock().unwrap().terminals[&source.id]
            .handle
            .clone();
        assert_eq!(adopted.holder_identity(), Some(&owner));
        wait_file(&case.join("reply-received"));
        assert_eq!(fs::read(case.join("reply-received")).unwrap(), b"\x1b[1;1R");
        // Restoration detaches its internal subscriber and keeps parsing while
        // hidden. Reattach and reveal through HOST before asking for a rendered
        // grid; reading the cached snapshot alone does not request a capture.
        assert!(
            !successor
                .lock()
                .unwrap()
                .subscriptions
                .attached(0, source.id)
        );
        let (response, _, _) = super::super::client::process_request(
            &successor,
            0,
            ClientRequest::Attach {
                terminal_id: source.id,
                after_generation: 0,
                after_event_sequence: 0,
            },
        )
        .unwrap();
        assert!(matches!(response, ServerResponse::Attached { terminal }
                if terminal.session.id == source.id && terminal.session.launch == source.launch));
        let (response, _, _) = super::super::client::process_request(
            &successor,
            0,
            ClientRequest::SetVisibility {
                terminal_id: source.id,
                visibility: crate::terminal_ipc::TerminalVisibility::Visible,
            },
        )
        .unwrap();
        assert!(matches!(response, ServerResponse::Accepted));
        wait_text(&adopted, "REPLY_CONSUMED");
        assert_eq!(
            fs::read_to_string(case.join("launches")).unwrap(),
            "launch\n",
            "recipe executed twice"
        );
        assert_eq!(adopted.close_checked(true), TerminalCloseResult::Closed);
        roots.settle();
        // A later missing owner is never permission to execute the recipe.
        store.save(&prepared).unwrap();
        let missing = Arc::new(HostLock::new(HostState::new(proof.clone())));
        let refusal = restore_upgrade(&missing).unwrap_err();
        assert!(refusal.contains("no recipe restart was authorized"));
        assert!(missing.lock().unwrap().terminals.is_empty());
        assert_eq!(
            fs::read_to_string(case.join("launches")).unwrap(),
            "launch\n"
        );
        store.remove().unwrap();
    }
}

#[test]
fn restore_keeps_partial_originals_and_raw_prefix_reachable_after_commit_or_store_failure() {
    const NAME: &str =
        "restore_keeps_partial_originals_and_raw_prefix_reachable_after_commit_or_store_failure";
    if isolated(NAME) {
        return;
    }
    let root = std::env::current_dir().unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    for (index, fault) in ["commit", "store", "none"].into_iter().enumerate() {
        let case = root.join(format!("startup-{index}"));
        fs::create_dir(&case).unwrap();
        let mut proof = proof(&format!("retained-startup-{index}"));
        let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
        let original = host(proof.clone());
        let mut sources = Vec::new();
        for number in 0..2 {
            let cwd = case.join(format!("original-{number}"));
            fs::create_dir(&cwd).unwrap();
            let session = source(&cwd, QUERY_CHILD);
            let handle = spawn(&original, &session);
            wait_text(&handle, "STABLE_INPUT_READY");
            sources.push((session, handle));
        }
        // The checkpoint uses this exact ID order; the second failure cannot
        // accidentally become a first-owner failure on a random UUID order.
        sources.sort_by_key(|(session, _)| session.id);
        let owners = sources
            .iter()
            .map(|(_, handle)| handle.holder_identity().unwrap().clone())
            .collect::<Vec<_>>();
        let mut roots = owners
            .iter()
            .map(OriginalProcesses::pin)
            .collect::<Vec<_>>();
        let checkpoint_directory = Store::discover().unwrap().path.parent().unwrap().to_owned();
        let holders = holder_directory().unwrap();
        let capability = original
            .lock()
            .unwrap()
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        prepare_upgrade_with_carry_policy(
            &original,
            capability,
            false,
            Some(&holders),
            true,
            || Store::open(&checkpoint_directory),
        )
        .unwrap();
        let store = Store::open(&checkpoint_directory).unwrap();
        for (source, _) in &sources {
            fs::write(source.launch.working_directory.join("query-gate"), b"emit").unwrap();
            wait_file(&source.launch.working_directory.join("query-emitted"));
            assert!(
                !source
                    .launch
                    .working_directory
                    .join("reply-received")
                    .exists()
            );
        }
        let (old_handles, _) = original.lock().unwrap().graceful_shutdown();
        super::super::finish_chat_shutdown(&original).unwrap();
        for handle in old_handles {
            handle.shutdown().unwrap();
        }
        assert_eq!(store.load().unwrap().unwrap().phase, Phase::Ready);
        drop(original);
        drop(lease.take());
        proof.host_incarnation = HostIncarnation::new(format!("retained-successor-{index}"));
        lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
        let successor = host(proof.clone());
        {
            let mut state = successor.lock().unwrap();
            state.install_model(
                crate::host::model::Registry::at(&case.join("registry.sqlite3")).unwrap(),
            );
            if fault == "commit" {
                state.test_fail_startup_commit = Some(sources[1].0.id);
            }
            if fault == "store" {
                state.test_checkpoint_failure_at = Some((sources[1].0.id, store.path.clone()));
            }
        }
        let result = restore_upgrade(&successor);
        assert_eq!(result.is_err(), fault != "none", "{fault}: {result:?}");
        let handles = {
            let state = successor.lock().unwrap();
            assert_eq!(
                state.terminals.len(),
                2,
                "both actual originals require independently retained parent records"
            );
            sources
                .iter()
                .map(|(source, _)| state.terminals[&source.id].handle.clone())
                .collect::<Vec<_>>()
        };
        for ((source, handle), owner) in sources
            .iter()
            .map(|(source, _)| source)
            .zip(handles.iter())
            .zip(owners.iter())
        {
            assert_eq!(handle.holder_identity(), Some(owner));
            assert!(owner.root.is_current());
            assert!(owner.holder.is_current());
            assert_eq!(
                fs::read_to_string(source.launch.working_directory.join("launches")).unwrap(),
                "launch\n"
            );
        }
        if fault == "commit" {
            assert!(!handles[0].startup_unresolved());
            assert!(handles[1].startup_unresolved());
            let (_, pending, cleanup) = handles[1].retained_startup_state_for_test();
            assert!(pending && !cleanup);
            assert_eq!(handles[1].retained_startup_bytes_for_test(), b"\x1b[6n");
            assert!(
                !sources[1]
                    .0
                    .launch
                    .working_directory
                    .join("reply-received")
                    .exists()
            );
            assert!(result.as_ref().unwrap_err().contains("original ownership"));
        }
        for (position, (source, _)) in sources.iter().enumerate() {
            if fault == "commit" && position == 1 {
                continue;
            }
            wait_file(&source.launch.working_directory.join("reply-received"));
            assert_eq!(
                fs::read(source.launch.working_directory.join("reply-received")).unwrap(),
                b"\x1b[1;1R"
            );
        }
        if fault != "none" {
            assert_eq!(
                successor
                    .lock()
                    .unwrap()
                    .retained_startup_checkpoint
                    .as_ref()
                    .unwrap()
                    .phase,
                Phase::Restoring
            );
            let (response, _, _) =
                super::super::client::process_request(&successor, 0, ClientRequest::List).unwrap();
            let ServerResponse::Listed { terminals } = response else {
                panic!("existing List must remain reachable");
            };
            assert_eq!(terminals.len(), 2);
            for request in [
                ClientRequest::Input {
                    terminal_id: sources[0].0.id,
                    bytes: b"not admitted".to_vec(),
                },
                ClientRequest::Resume {
                    terminal_id: sources[1].0.id,
                    visible: true,
                },
                ClientRequest::Shutdown,
                ClientRequest::Spawn {
                    session: source(&case, QUERY_CHILD),
                    options: TerminalSpawnOptions::default(),
                },
            ] {
                let error =
                    super::super::client::process_request(&successor, 0, request).unwrap_err();
                assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
                assert!(error.message.contains("Startup recovery"));
            }
            successor.lock().unwrap().request_shutdown();
            assert!(!successor.lock().unwrap().is_shutting_down());
            assert!(successor.lock().unwrap().graceful_shutdown().0.is_empty());
            assert!(owners.iter().all(|owner| owner.root.is_current()));
            if fault == "commit" {
                assert_eq!(store.load().unwrap().unwrap().phase, Phase::Restoring);
                assert_eq!(handles[1].retained_startup_bytes_for_test(), b"\x1b[6n");
            } else {
                assert!(
                    store.load().is_err(),
                    "real checkpoint path failure must be observable"
                );
                let preserved: crate::upgrade::handoff::Checkpoint = serde_json::from_slice(
                    &fs::read(store.path.with_extension("before-failure.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(preserved.phase, Phase::Restoring);
                assert_eq!(preserved.carried.len(), 2);
            }
        }
        // Existing public checked Close, not ordinary Stop or a new recovery
        // wire operation, deliberately authorizes each original destruction.
        for ((source, _), handle) in sources.iter().zip(handles.iter()) {
            // Successful lineage restoration detaches execution client zero.
            // Reattach through the real public route before its checked Close;
            // the unresolved original is already attached and stays parked.
            if !successor
                .lock()
                .unwrap()
                .subscriptions
                .attached(0, source.id)
            {
                let (response, _, _) = super::super::client::process_request(
                    &successor,
                    0,
                    ClientRequest::Attach {
                        terminal_id: source.id,
                        after_generation: 0,
                        after_event_sequence: 0,
                    },
                )
                .unwrap();
                assert!(
                    matches!(response, ServerResponse::Attached { terminal } if terminal.session.id == source.id)
                );
            }
            let (response, _, _) = super::super::client::process_request(
                &successor,
                0,
                ClientRequest::CloseTerminalChecked {
                    terminal_id: source.id,
                    allow_active: true,
                },
            )
            .unwrap();
            assert!(matches!(
                response,
                ServerResponse::TerminalCloseChecked {
                    outcome: TerminalCloseResult::Closed,
                    ..
                }
            ));
            assert!(handle.worker_joined_for_test());
        }
        for root in &mut roots {
            root.settle();
        }
        for owner in &owners {
            assert!(
                !crate::daemon::holder_registry::retirement_pending(owner).unwrap(),
                "exact original holder discovery must be retired after proved Close"
            );
        }
        assert!(!successor.lock().unwrap().recovery_owns_work());
        successor.lock().unwrap().request_shutdown();
        assert!(successor.lock().unwrap().is_shutting_down());
        if fault == "store" {
            fs::remove_dir(&store.path).unwrap();
            fs::rename(
                store.path.with_extension("before-failure.json"),
                &store.path,
            )
            .unwrap();
        }
        if fault != "none" {
            store.remove().unwrap();
        }
        drop(successor);
        drop(lease.take());
    }
}

#[test]
fn orphan_authority_restore_failure_keeps_the_committed_original_in_recovery_until_explicit_close()
{
    const NAME: &str = "orphan_authority_restore_failure_keeps_the_committed_original_in_recovery_until_explicit_close";
    if isolated(NAME) {
        return;
    }
    let root = std::env::current_dir().unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    for (index, unavailable_registry_table) in [true, false].into_iter().enumerate() {
        let case = root.join(format!("orphan-authority-{index}"));
        fs::create_dir(&case).unwrap();
        let mut proof = proof(&format!("orphan-authority-{index}"));
        let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
        let original = host(proof.clone());
        let source = source(&case, QUERY_CHILD);
        let handle = spawn(&original, &source);
        wait_text(&handle, "STABLE_INPUT_READY");
        let owner = handle.holder_identity().unwrap().clone();
        let mut processes = OriginalProcesses::pin(&owner);
        let directory = holder_directory().unwrap();
        let socket = crate::terminal::holder::socket(&directory, source.id);
        handle.carry(socket).unwrap();
        fs::write(case.join("query-gate"), b"emit").unwrap();
        wait_file(&case.join("query-emitted"));
        assert!(!case.join("reply-received").exists());
        drop(lease.take());
        proof.host_incarnation = HostIncarnation::new(format!("orphan-successor-{index}"));
        lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
        let successor = host(proof.clone());
        let registry_path = case.join("registry.sqlite3");
        successor
            .lock()
            .unwrap()
            .install_model(crate::host::model::Registry::at(&registry_path).unwrap());
        let fault = rusqlite::Connection::open(&registry_path).unwrap();
        if unavailable_registry_table {
            // A real SQLite query failure AFTER holder commit/activation, not
            // an injected replacement result or a synthetic source DTO.
            fault
                .execute_batch(
                    "ALTER TABLE agent_authorities RENAME TO unavailable_agent_authorities",
                )
                .unwrap();
        }
        let result = restore_orphaned_holders(&successor);
        assert_eq!(result.is_err(), unavailable_registry_table, "{result:?}");
        let adopted = {
            let state = successor.lock().unwrap();
            assert_eq!(state.terminals.len(), 1);
            assert_eq!(
                state.startup_recovery().is_some(),
                unavailable_registry_table
            );
            state.terminals[&source.id].handle.clone()
        };
        assert_eq!(adopted.holder_identity(), Some(&owner));
        assert!(owner.root.is_current() && owner.holder.is_current());
        assert!(
            !adopted.startup_unresolved(),
            "this original's reader already activated successfully before the metadata failure"
        );
        wait_file(&case.join("reply-received"));
        assert_eq!(fs::read(case.join("reply-received")).unwrap(), b"\x1b[1;1R");
        assert_eq!(
            fs::read_to_string(case.join("launches")).unwrap(),
            "launch\n"
        );
        if unavailable_registry_table {
            assert!(result.unwrap_err().contains("agent_authorities"));
            let (response, _, _) =
                super::super::client::process_request(&successor, 0, ClientRequest::List).unwrap();
            assert!(
                matches!(response, ServerResponse::Listed { terminals } if terminals.len() == 1)
            );
            for request in [
                ClientRequest::Input {
                    terminal_id: source.id,
                    bytes: b"must not execute".to_vec(),
                },
                ClientRequest::Resume {
                    terminal_id: source.id,
                    visible: true,
                },
                ClientRequest::Shutdown,
            ] {
                let error =
                    super::super::client::process_request(&successor, 0, request).unwrap_err();
                assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
            }
            successor.lock().unwrap().request_shutdown();
            assert!(!successor.lock().unwrap().is_shutting_down());
            assert!(successor.lock().unwrap().graceful_shutdown().0.is_empty());
            assert!(owner.root.is_current() && owner.holder.is_current());
            fault
                .execute_batch(
                    "ALTER TABLE unavailable_agent_authorities RENAME TO agent_authorities",
                )
                .unwrap();
            // Repairing the query does not retry adoption or clear quarantine.
            assert!(successor.lock().unwrap().startup_recovery().is_some());
            assert_eq!(adopted.holder_identity(), Some(&owner));
        }
        let (response, _, _) = super::super::client::process_request(
            &successor,
            0,
            ClientRequest::Attach {
                terminal_id: source.id,
                after_generation: 0,
                after_event_sequence: 0,
            },
        )
        .unwrap();
        assert!(
            matches!(response, ServerResponse::Attached { terminal } if terminal.session.id == source.id)
        );
        let (response, _, _) = super::super::client::process_request(
            &successor,
            0,
            ClientRequest::CloseTerminalChecked {
                terminal_id: source.id,
                allow_active: true,
            },
        )
        .unwrap();
        assert!(matches!(
            response,
            ServerResponse::TerminalCloseChecked {
                outcome: TerminalCloseResult::Closed,
                ..
            }
        ));
        assert!(adopted.worker_joined_for_test());
        processes.settle();
        assert!(!crate::daemon::holder_registry::retirement_pending(&owner).unwrap());
        assert!(!successor.lock().unwrap().recovery_owns_work());
        successor.lock().unwrap().request_shutdown();
        assert!(successor.lock().unwrap().is_shutting_down());
        drop(successor);
        drop(original);
        drop(handle);
        drop(lease.take());
    }
}
