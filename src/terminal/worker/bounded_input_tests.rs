// Input-only regressions ported from abd93. The process cases self-exec
// in private state. Their original-child/whole-family guardian stays external.
const INPUT_FIXTURE_ROOT: &str = "CAGENTS_BOUNDED_INPUT_FIXTURE_ROOT";
const INPUT_FIXTURE_TEST: &str = "CAGENTS_BOUNDED_INPUT_FIXTURE_TEST";
const INPUT_FIXTURE_SCOPE: &str = "CAGENTS_BOUNDED_INPUT_AUTHORITY_PRESENCE";

struct InputFixtureChild(std::process::Child);
impl Drop for InputFixtureChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            // Failure only, for this original child. Outer census still owns
            // any descendant settlement; no numeric PID is rediscovered here.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
struct InputFixtureDirectory(Option<tempfile::TempDir>);
impl Drop for InputFixtureDirectory {
    fn drop(&mut self) {
        if let Some(directory) = self.0.take() {
            let _preserved = directory.keep();
        }
    }
}
fn isolated_input_case(name: &str) -> bool {
    if let Some(root) = std::env::var_os(INPUT_FIXTURE_ROOT) {
        assert_eq!(std::env::var(INPUT_FIXTURE_TEST).as_deref(), Ok(name));
        let expected = match std::env::var(INPUT_FIXTURE_SCOPE).as_deref() {
            Ok("present") => true,
            Ok("absent") => false,
            _ => panic!("missing input fixture inherited-scope marker"),
        };
        assert_eq!(
            crate::agent_execution::Authority::from_environment()
                != crate::agent_execution::Authority::default(),
            expected,
            "input child lost its inherited opaque authority scope"
        );
        assert_eq!(
            std::env::current_dir().unwrap(),
            PathBuf::from(root).canonicalize().unwrap()
        );
        return false;
    }
    // Honour the owner's short non-repository TMPDIR; never alter shared
    // checkout markers. Preserve evidence on both success and failure.
    let directory = tempfile::Builder::new().prefix("in-").tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let _evidence = InputFixtureDirectory(Some(directory));
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["home", "run", "data", "config", "cache", "tmp"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let stdout = root.join("test.stdout");
    let stderr = root.join("test.stderr");
    let selector = format!("terminal::worker::tests::{name}");
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    // Leave delegation inherited and opaque; all other inputs are private.
    for (name, _) in std::env::vars_os() {
        if name != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV) {
            command.env_remove(name);
        }
    }
    command
        .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
        .current_dir(&root)
        .env(INPUT_FIXTURE_ROOT, &root)
        .env(INPUT_FIXTURE_TEST, name)
        .env(
            INPUT_FIXTURE_SCOPE,
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
        .stdout(std::process::Stdio::from(File::create(&stdout).unwrap()))
        .stderr(std::process::Stdio::from(File::create(&stderr).unwrap()));
    let mut child = InputFixtureChild(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "input fixture child timed out: {}",
            root.display()
        );
        thread::sleep(Duration::from_millis(10));
    };
    let output = fs::read_to_string(stdout).unwrap();
    let errors = fs::read_to_string(stderr).unwrap();
    assert!(status.success(), "input fixture failed: {output}\n{errors}");
    assert!(
        output.contains(&format!("test {selector} ... ok")),
        "missing exact child pass: {output}"
    );
    assert!(
        output.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "missing nonzero child summary: {output}"
    );
    true
}
fn input_root_exited(pidfd: &File) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let mut entry = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: this is an owned capability for the original root; poll
        // observes exit without rediscovering a numeric process identity.
        let result = unsafe { libc::poll(&mut entry, 1, 0) };
        if result >= 0 {
            return Ok(entry.revents & libc::POLLIN != 0);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
struct InputRootGuard {
    pidfd: File,
    armed: bool,
}
impl InputRootGuard {
    fn new(published: &PublishedState) -> Self {
        Self {
            pidfd: published
                .test_input_root_pidfd
                .get()
                .expect("actual startup pinned original unreaped fixture root")
                .try_clone()
                .unwrap(),
            armed: true,
        }
    }
    fn settled(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !input_root_exited(&self.pidfd).unwrap() {
            assert!(
                Instant::now() < deadline,
                "original fixture root exit remains unconfirmed"
            );
            thread::sleep(Duration::from_millis(5));
        }
        self.armed = false;
    }
}
impl Drop for InputRootGuard {
    fn drop(&mut self) {
        if self.armed && !input_root_exited(&self.pidfd).unwrap_or(false) {
            // Failure-only original capability, never a guessed/reused PID.
            let _ = close_processes::signal(&self.pidfd, libc::SIGCONT);
            let _ = close_processes::signal(&self.pidfd, libc::SIGKILL);
        }
    }
}
// Captured Workers run on the Ghostty-creating test thread. There is no OS
// Worker JoinHandle. Empty owner admits commands only; native cleanup is explicit.
fn finish_captured_input_worker(
    worker: Worker,
    handle: &TerminalHandle,
    root: &mut InputRootGuard,
) {
    let published = worker.published.clone();
    worker.run();
    published.release_completed_holder();
    assert!(published.retained_holder.lock().unwrap().is_none());
    handle.shutdown().unwrap();
    root.settled();
}

fn observe_worker_marker(worker: &mut Worker, marker: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut observed = Vec::new();
    loop {
        let message = worker
            .pty_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if let PtyMessage::Bytes(bytes) = message {
            observed.extend_from_slice(&bytes);
            worker.process_output(bytes);
            if observed.windows(marker.len()).any(|word| word == marker) {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "PTY fixture marker missing: {:?}",
            String::from_utf8_lossy(&observed)
        );
    }
}

#[test]
fn nonreading_child_keeps_publishing_and_interrupt_bypasses_backpressure() {
    if isolated_input_case("nonreading_child_keeps_publishing_and_interrupt_bypasses_backpressure")
    {
        return;
    }
    let output_gate = std::env::current_dir()
        .unwrap()
        .join("post-paste-output-gate");
    assert!(!output_gate.exists());
    let mut session = shell_session(
        "stty -echo -icanon; trap 'echo interrupted; exit 0' INT; echo ready; while :; do if [ -f \"$INPUT_OUTPUT_GATE\" ]; then echo post_paste_output; else echo tick; fi; sleep 0.05; done",
    );
    session.launch.environment.insert(
        "INPUT_OUTPUT_GATE".into(),
        output_gate.to_str().unwrap().to_owned(),
    );
    let handle = TerminalHandle::spawn(session, TerminalConfig::default(), |_| {}).unwrap();
    let mut root = InputRootGuard::new(&handle.inner.published);
    wait_for_text(&handle, "ready");
    let before = handle.snapshot().generation;
    let mut accepted_large_pastes = 0;
    let mut full_replies = 0;
    for _ in 0..3 {
        match handle.paste("x".repeat(1024 * 1024), PasteOrigin::Clipboard) {
            Ok(()) => accepted_large_pastes += 1,
            Err(TerminalSendError::Full) => full_replies += 1,
            Err(error) => panic!("unexpected paste admission failure: {error}"),
        }
    }
    assert!(
        accepted_large_pastes >= 1,
        "backpressure must not be implemented by rejecting every large paste"
    );
    assert!(
        full_replies >= 1,
        "the public paste caller must receive Full for excess input before acknowledgement"
    );
    // Only the actual child can emit this marker, and its gate is opened
    // after both accepted input and public Full have been observed. A redraw
    // generation alone could advance without parsing any new child output.
    fs::write(&output_gate, b"emit").unwrap();
    wait_for_text(&handle, "post_paste_output");
    assert!(
        handle.inner.published.command_budget.get().unwrap().used() > 0,
        "the accepted nonreading-child backlog must still own admission credit"
    );
    assert!(
        handle.snapshot().generation > before,
        "output stalled behind PTY write"
    );
    handle.input(vec![3]).unwrap();
    wait_for_text(&handle, "interrupted");
    let started = std::time::Instant::now();
    handle.shutdown().unwrap();
    assert!(handle.worker_joined_for_test());
    root.settled();
    assert!(started.elapsed() < WORKER_JOIN_TIMEOUT + Duration::from_millis(100));
}

#[test]
fn inherited_bracketed_paste_preserves_delivery_and_priority_interrupt_is_nonfatal() {
    if isolated_input_case(
        "inherited_bracketed_paste_preserves_delivery_and_priority_interrupt_is_nonfatal",
    ) {
        return;
    }
    let directory = std::env::current_dir().unwrap();
    let script = directory.join("bracketed-input-peer.py");
    fs::write(
        &script,
        r#"import os, signal, termios, time
from pathlib import Path
attributes = termios.tcgetattr(0)
attributes[0] = 0
attributes[1] = 0
attributes[3] = (attributes[3] & ~(termios.ECHO | termios.ICANON)) | termios.ISIG
attributes[6][termios.VMIN] = 1
attributes[6][termios.VTIME] = 0
termios.tcsetattr(0, termios.TCSANOW, attributes)
interrupts = 0
def interrupt(signum, frame):
    global interrupts
    interrupts += 1
    os.write(1, b'SIGINT_OBSERVED\n')
signal.signal(signal.SIGINT, interrupt)
# A shell may leave this mode enabled when it launches a nonreading child.
os.write(1, b'\x1b[?2004hBRACKETED_READY\n')
reported = False
while not interrupts:
    if not reported and Path('output-gate').exists():
        os.write(1, b'POST_PASTE_OUTPUT\n')
        reported = True
    time.sleep(0.005)
Path('interrupts').write_text(str(interrupts))
paste = b''
while not paste.endswith(b'\x1b[201~'):
    paste += os.read(0, 1)
Path('paste.bin').write_bytes(paste)
os.write(1, b'PASTE_FINISHED\n')
following = b''
while not following.endswith(b'\n'):
    following += os.read(0, 1)
Path('following.bin').write_bytes(following)
os.write(1, b'NEXT_INPUT_OBSERVED\n')
"#,
    )
    .unwrap();
    let recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "/usr/bin/python3".into(),
        arguments: vec!["-I".into(), "-u".into(), script.to_str().unwrap().into()],
        working_directory: directory.clone(),
        environment: BTreeMap::new(),
    };
    let (mut worker, events) = capture_worker_for(recipe);
    let mut root = InputRootGuard::new(&worker.published);
    let (commands, receiver) = command_channel(1);
    worker.commands = receiver;
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events: events.clone(),
            published: worker.published.clone(),
            worker: joins::JoinOwner::empty(worker.published.clone()),
        }),
    };
    // Drive the actual PTY, including finishing protocol bytes while its
    // reader resumes. A timeout can never stand in for observing a marker.
    let observe = |worker: &mut Worker, marker: &[u8]| {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        loop {
            assert!(worker.flush_pty_input());
            match worker.pty_rx.recv_timeout(Duration::from_millis(5)) {
                Ok(PtyMessage::Bytes(bytes)) => {
                    output.extend_from_slice(&bytes);
                    worker.process_output(bytes);
                    if output.windows(marker.len()).any(|bytes| bytes == marker) {
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                other => panic!("PTY closed before {marker:?}: {other:?}"),
            }
            assert!(Instant::now() < deadline, "missing PTY marker {marker:?}");
        }
    };
    observe(&mut worker, b"BRACKETED_READY");
    assert!(worker.terminal.mode(Mode::BRACKETED_PASTE).unwrap());
    handle
        .paste("x".repeat(1024 * 1024), PasteOrigin::Clipboard)
        .unwrap();
    worker.handle_command(worker.commands.try_recv().unwrap());
    assert!(!worker.pending_input.is_empty());
    assert!(!worker.pending_input.can_flush_kernel());
    assert_eq!(
        handle.paste("not accepted", PasteOrigin::Clipboard),
        Err(TerminalSendError::Full)
    );
    fs::write(directory.join("output-gate"), b"emit").unwrap();
    observe(&mut worker, b"POST_PASTE_OUTPUT");
    let activity = Some(crate::agent::Activity {
        provider: crate::agent::Provider::Claude,
        state: crate::agent::ActivityState::Working,
    });
    worker.set_agent_activity(activity);
    let mut interrupt = key_input(
        KeyAction::Press,
        TerminalKey::Character('c'),
        Some("c"),
        Some('c'),
    );
    interrupt.modifiers.control = true;
    handle.key(interrupt).unwrap();
    worker.interrupt_input();
    assert_eq!(handle.agent_activity(), activity);
    observe(&mut worker, b"PASTE_FINISHED");
    assert_eq!(fs::read(directory.join("interrupts")).unwrap(), b"1");
    let received = fs::read(directory.join("paste.bin")).unwrap();
    assert!(received.starts_with(b"\x1b[200~"));
    assert!(received.ends_with(b"\x1b[201~"));
    assert!(
        received[6..received.len() - 6]
            .iter()
            .all(|byte| *byte == b'x')
    );
    assert!(received.len() > 12 && received.len() < 1024 * 1024 + 12);
    assert!(worker.pending_input.is_empty());
    assert!(!worker.pending_input.can_flush_kernel());
    handle.input(b"NEXT_INPUT\n".to_vec()).unwrap();
    worker.handle_command(worker.commands.try_recv().unwrap());
    observe(&mut worker, b"NEXT_INPUT_OBSERVED");
    assert_eq!(
        fs::read(directory.join("following.bin")).unwrap(),
        b"NEXT_INPUT\n"
    );
    let notices = events.try_iter().collect::<Vec<_>>();
    assert_eq!(
        notices
            .iter()
            .filter(|event| matches!(event,
            TerminalEvent::InputNotice { operation: "cancel terminal input", message }
                if message == "Unsent input was canceled. Previously sent input was preserved."))
            .count(),
        1
    );
    assert!(
        !notices
            .iter()
            .any(|event| matches!(event, TerminalEvent::Error { .. }))
    );
    assert!(!worker.writer_failed);
    finish_captured_input_worker(worker, &handle, &mut root);
}

#[test]
fn an_interrupt_notice_distinguishes_consumed_from_unsent_input() {
    if isolated_input_case("an_interrupt_notice_distinguishes_consumed_from_unsent_input") {
        return;
    }
    for queued in [None, Some(0), Some(1), Some(2)] {
        let label = match queued {
            None => "consumed",
            Some(0) => "inbox",
            Some(1) => "retained",
            Some(2) => "carried",
            Some(_) => unreachable!(),
        };
        let directory = std::env::current_dir().unwrap().join(label);
        fs::create_dir(&directory).unwrap();
        let script = directory.join("consumed-paste-peer.py");
        fs::write(
            &script,
            r#"import os, signal, termios, time
from pathlib import Path
attributes = termios.tcgetattr(0)
attributes[0] = 0
attributes[1] = 0
attributes[3] = (attributes[3] & ~(termios.ECHO | termios.ICANON)) | termios.ISIG
attributes[6][termios.VMIN] = 1
attributes[6][termios.VTIME] = 0
termios.tcsetattr(0, termios.TCSANOW, attributes)
interrupts = 0
def interrupt(signum, frame):
    global interrupts
    interrupts += 1
    os.write(1, b'SIGINT_OBSERVED\n')
signal.signal(signal.SIGINT, interrupt)
os.write(1, b'\x1b[?2004hCONSUMED_PASTE_READY\n')
paste = b''
while not paste.endswith(b'\x1b[201~'):
    paste += os.read(0, 1)
Path('paste.bin').write_bytes(paste)
os.write(1, b'PASTE_CONSUMED\n')
while not interrupts:
    time.sleep(0.005)
Path('interrupts').write_text(str(interrupts))
following = b''
while not following.endswith(b'\n'):
    following += os.read(0, 1)
Path('following.bin').write_bytes(following)
os.write(1, b'NEXT_INPUT_OBSERVED\n')
"#,
        )
        .unwrap();
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/usr/bin/python3".into(),
            arguments: vec!["-I".into(), "-u".into(), script.to_str().unwrap().into()],
            working_directory: directory.clone(),
            environment: BTreeMap::new(),
        };
        let (mut worker, events) = capture_worker_for(recipe);
        let mut root = InputRootGuard::new(&worker.published);
        let (commands, receiver) = command_channel(if queued == Some(2) { 2 } else { 1 });
        worker.commands = receiver;
        let handle = TerminalHandle {
            inner: Arc::new(HandleInner {
                commands,
                events: events.clone(),
                published: worker.published.clone(),
                worker: joins::JoinOwner::empty(worker.published.clone()),
            }),
        };
        let observe = |worker: &mut Worker, marker: &[u8]| {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut output = Vec::new();
            loop {
                assert!(worker.flush_pty_input());
                match worker.pty_rx.recv_timeout(Duration::from_millis(5)) {
                    Ok(PtyMessage::Bytes(bytes)) => {
                        output.extend_from_slice(&bytes);
                        worker.process_output(bytes);
                        if output.windows(marker.len()).any(|bytes| bytes == marker) {
                            break;
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    other => panic!("PTY closed before {marker:?}: {other:?}"),
                }
                assert!(Instant::now() < deadline, "missing PTY marker {marker:?}");
            }
        };
        observe(&mut worker, b"CONSUMED_PASTE_READY");
        handle
            .paste("already read", PasteOrigin::Clipboard)
            .unwrap();
        worker.handle_command(worker.commands.try_recv().unwrap());
        observe(&mut worker, b"PASTE_CONSUMED");
        assert_eq!(
            fs::read(directory.join("paste.bin")).unwrap(),
            b"\x1b[200~already read\x1b[201~"
        );
        assert!(worker.pending_input.is_empty());
        assert!(!worker.pending_input.can_flush_kernel());
        assert!(worker.retained_commands.is_empty());
        assert!(worker.commands.is_empty());
        // Queue actual older-generation input without encoding it. Each
        // owner must count even when the encoded backlog is empty.
        let mut carried = None;
        if queued == Some(2) {
            handle.scroll_lines(1).unwrap();
            handle.input(b"DISCARD_ME".to_vec()).unwrap();
            let first = worker.commands.try_recv().unwrap();
            let scroll = coalesce_scroll_command(first, &worker.commands, &mut carried);
            worker.handle_command(scroll);
            assert!(carried.is_some());
            assert!(worker.pending_input.is_empty());
            assert!(worker.retained_commands.is_empty());
            assert!(worker.commands.is_empty());
            assert_eq!(handle.command_budget().used(), 1);
        } else if let Some(retained) = queued {
            handle.input(b"DISCARD_ME".to_vec()).unwrap();
            if retained == 1 {
                worker
                    .retained_commands
                    .push_back(worker.commands.try_recv().unwrap());
            }
        }
        let mut interrupt = key_input(
            KeyAction::Press,
            TerminalKey::Character('c'),
            Some("c"),
            Some('c'),
        );
        interrupt.modifiers.control = true;
        handle.key(interrupt).unwrap();
        let boundary = worker.interrupt_input_with_carried(carried.as_ref());
        if queued == Some(2) {
            assert_eq!(handle.command_budget().used(), 1);
            assert!(
                carried
                    .as_ref()
                    .is_some_and(|command| is_cancelable_input_before(command, boundary))
            );
            // Match the run loop: retain the exact owner through interruption,
            // then drop it at the same captured generation boundary.
            carried = None;
            assert!(carried.is_none());
            assert_eq!(handle.command_budget().used(), 0);
        }
        observe(&mut worker, b"SIGINT_OBSERVED");
        assert!(worker.pending_input.is_empty());
        assert!(!worker.pending_input.can_flush_kernel());
        handle.input(b"NEXT_INPUT\n".to_vec()).unwrap();
        worker.handle_command(worker.commands.try_recv().unwrap());
        observe(&mut worker, b"NEXT_INPUT_OBSERVED");
        assert_eq!(fs::read(directory.join("interrupts")).unwrap(), b"1");
        assert_eq!(
            fs::read(directory.join("following.bin")).unwrap(),
            b"NEXT_INPUT\n"
        );
        let notices = events.try_iter().collect::<Vec<_>>();
        let expected = if queued.is_some() {
            "Unsent input was canceled. Previously sent input was preserved."
        } else {
            "Previously sent input was preserved."
        };
        assert_eq!(
            notices
                .iter()
                .filter(|event| matches!(event,
                TerminalEvent::InputNotice { operation: "cancel terminal input", message }
                    if message == expected))
                .count(),
            1
        );
        assert_eq!(
            notices
                .iter()
                .filter(|event| matches!(event, TerminalEvent::InputNotice { .. }))
                .count(),
            1
        );
        assert!(
            !notices
                .iter()
                .any(|event| matches!(event, TerminalEvent::Error { .. }))
        );
        assert!(!worker.writer_failed);
        finish_captured_input_worker(worker, &handle, &mut root);
    }
}

#[test]
fn nonreading_child_shutdown_is_bounded_with_a_full_pending_paste() {
    if isolated_input_case("nonreading_child_shutdown_is_bounded_with_a_full_pending_paste") {
        return;
    }
    let handle = TerminalHandle::spawn(
        shell_session("stty -echo -icanon; echo ready; exec sleep 600"),
        TerminalConfig::default(),
        |_| {},
    )
    .unwrap();
    let mut root = InputRootGuard::new(&handle.inner.published);
    wait_for_text(&handle, "ready");
    handle
        .paste("x".repeat(1024 * 1024), PasteOrigin::Clipboard)
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    let started = std::time::Instant::now();
    handle.shutdown().unwrap();
    assert!(handle.worker_joined_for_test());
    root.settled();
    assert!(started.elapsed() < WORKER_JOIN_TIMEOUT + Duration::from_millis(100));
}

#[test]
fn encoded_owned_release_survives_priority_interrupt_in_userspace_and_kernel() {
    if isolated_input_case(
        "encoded_owned_release_survives_priority_interrupt_in_userspace_and_kernel",
    ) {
        return;
    }
    for release_in_kernel in [false, true] {
        let mut home = tempfile::tempdir().unwrap();
        home.disable_cleanup(true);
        eprintln!("bounded input private evidence: {}", home.path().display());
        let script = home.path().join("input-peer.py");
        fs::write(
            &script,
            r#"import os, signal, sys, tty
from pathlib import Path
tty.setraw(0)
def read_exact(count):
    data = b''
    while len(data) < count:
        chunk = os.read(0, count - len(data))
        if not chunk: raise RuntimeError('unexpected PTY EOF')
        data += chunk
    return data
os.write(1, b'\x1b[>3uPAIR_READY')
Path('press.bin').write_bytes(read_exact(int(sys.argv[1])))
os.write(1, b'PAIR_OBSERVED')
os.kill(os.getpid(), signal.SIGSTOP)
Path('received.bin').write_bytes(read_exact(int(Path('expected_len').read_text())))
os.write(1, b'PAIR_DONE')
"#,
        )
        .unwrap();
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 0,
        })
        .unwrap();
        terminal.vt_write(b"\x1b[>3u");
        let mut encoder = KeyEncoder::new().unwrap();
        let press = key_input(
            KeyAction::Press,
            TerminalKey::Character('a'),
            Some("a"),
            Some('a'),
        );
        let mut release = press.clone();
        release.action = KeyAction::Release;
        release.text = None;
        let down = encode_key_input(&terminal, &mut encoder, press.clone()).unwrap();
        let up = encode_key_input(&terminal, &mut encoder, release.clone()).unwrap();
        assert!(!down.is_empty() && !up.is_empty());
        let launch = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/usr/bin/python3".into(),
            arguments: vec![
                "-I".into(),
                "-u".into(),
                script.to_string_lossy().into_owned(),
                down.len().to_string(),
            ],
            working_directory: home.path().into(),
            environment: BTreeMap::new(),
        };
        let (mut worker, events) = capture_worker_for(launch);
        let mut root = InputRootGuard::new(&worker.published);
        let (commands, receiver) = command_channel(1);
        worker.commands = receiver;
        let handle = TerminalHandle {
            inner: Arc::new(HandleInner {
                commands,
                events,
                published: worker.published.clone(),
                worker: joins::JoinOwner::empty(worker.published.clone()),
            }),
        };
        observe_worker_marker(&mut worker, b"PAIR_READY");
        handle.key(press).unwrap();
        worker.handle_command(worker.commands.try_recv().unwrap());
        observe_worker_marker(&mut worker, b"PAIR_OBSERVED");
        assert_eq!(
            fs::read(home.path().join("press.bin")).unwrap(),
            down,
            "the actual child must consume its press before pausing"
        );
        let pid = worker.child_identity.as_ref().unwrap().process.pid;
        let stopped = Instant::now() + Duration::from_secs(3);
        loop {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            if stat
                .rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('T'))
            {
                break;
            }
            assert!(Instant::now() < stopped);
            thread::sleep(Duration::from_millis(1));
        }
        if release_in_kernel {
            handle.key(release.clone()).unwrap();
            worker.handle_command(worker.published.take_cleanup_command(true).unwrap());
            assert!(
                worker.pending_input.is_empty(),
                "this case must admit its unread release into the kernel"
            );
        }
        let bulk = vec![b'x'; 1024 * 1024];
        handle.input(bulk.clone()).unwrap();
        worker.handle_command(worker.commands.try_recv().unwrap());
        assert!(
            !worker.pending_input.is_empty(),
            "stopped child must force real write backpressure"
        );
        assert_eq!(handle.command_budget().used(), 1);
        assert_eq!(handle.focus(true), Err(TerminalSendError::Full));
        if !release_in_kernel {
            handle.key(release.clone()).expect("the original press reserved release admission before bulk saturated ordinary credits");
            assert_eq!(worker.published.cleanup_commands.lock().unwrap().len(), 1);
            worker.handle_command(worker.published.take_cleanup_command(true).unwrap());
        }
        let kernel_bulk = bulk.len()
            - (worker.pending_input.len() - if release_in_kernel { 0 } else { up.len() });
        assert!(kernel_bulk > 0);
        let mut interrupt = key_input(
            KeyAction::Press,
            TerminalKey::Character('c'),
            Some("c"),
            Some('c'),
        );
        interrupt.modifiers.control = true;
        let mut interrupt_up = interrupt.clone();
        interrupt_up.action = KeyAction::Release;
        interrupt_up.text = None;
        let ctrl_down = encode_key_input(&terminal, &mut encoder, interrupt.clone()).unwrap();
        let ctrl_up = encode_key_input(&terminal, &mut encoder, interrupt_up.clone()).unwrap();
        handle.key(interrupt).unwrap();
        worker.interrupt_input();
        handle.key(interrupt_up).unwrap();
        worker.handle_command(worker.published.take_cleanup_command(true).unwrap());
        let mut expected = Vec::new();
        if release_in_kernel {
            expected.extend_from_slice(&up);
        }
        expected.extend(std::iter::repeat_n(b'x', kernel_bulk));
        if !release_in_kernel {
            expected.extend_from_slice(&up);
        }
        expected.extend(ctrl_down);
        expected.extend(ctrl_up);
        fs::write(home.path().join("expected_len"), expected.len().to_string()).unwrap();
        close_processes::signal(&root.pidfd, libc::SIGCONT).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let actual = loop {
            worker.flush_pty_input();
            if let Ok(bytes) = fs::read(home.path().join("received.bin"))
                && bytes.len() == expected.len()
            {
                break bytes;
            }
            assert!(
                Instant::now() < deadline,
                "owned release did not drain after child resumed; kernel={release_in_kernel}"
            );
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(
            actual, expected,
            "one original release must survive exactly once, without flushing or replaying kernel bytes"
        );
        assert!(!worker.pending_input.has_held_input());
        worker.encode_key(release);
        assert!(
            worker.pending_input.is_empty(),
            "duplicate physical up must not be admitted"
        );
        finish_captured_input_worker(worker, &handle, &mut root);
    }
}

#[test]
fn reserved_cleanup_does_not_overtake_its_still_queued_press() {
    if isolated_input_case("reserved_cleanup_does_not_overtake_its_still_queued_press") {
        return;
    }
    let (mut worker, events) = capture_worker_for(
        shell_session("stty raw -echo; printf '\\033[>3uready'; exec sleep 600").launch,
    );
    let mut root = InputRootGuard::new(&worker.published);
    observe_worker_marker(&mut worker, b"ready");
    let (commands, receiver) = command_channel(1);
    worker.commands = receiver;
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events,
            published: worker.published.clone(),
            worker: joins::JoinOwner::empty(worker.published.clone()),
        }),
    };
    let down = key_input(
        KeyAction::Press,
        TerminalKey::Character('a'),
        Some("a"),
        Some('a'),
    );
    let mut up = down.clone();
    up.action = KeyAction::Release;
    up.text = None;
    handle.key(down).unwrap();
    handle
        .key(up)
        .expect("reserved cleanup cannot consume an ordinary full channel slot");
    assert!(worker.published.take_cleanup_command(true).is_none());
    worker.handle_command(worker.commands.try_recv().unwrap());
    worker.handle_command(worker.published.take_cleanup_command(true).unwrap());
    assert!(!worker.pending_input.has_held_input());
    worker.published.request_shutdown();
    finish_captured_input_worker(worker, &handle, &mut root);
}

#[test]
fn aggregate_credits_bound_interrupt_stash_while_producers_try_to_refill() {
    if isolated_input_case("aggregate_credits_bound_interrupt_stash_while_producers_try_to_refill")
    {
        return;
    }
    let (mut worker, events) = capture_worker_for(
        shell_session("stty raw -echo; trap '' INT; echo ready; exec sleep 600").launch,
    );
    let mut root = InputRootGuard::new(&worker.published);
    observe_worker_marker(&mut worker, b"ready");
    let (commands, receiver) = command_channel(3);
    worker.commands = receiver;
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events,
            published: worker.published.clone(),
            worker: joins::JoinOwner::empty(worker.published.clone()),
        }),
    };
    handle.input(b"old generation".to_vec()).unwrap();
    handle.focus(true).unwrap();
    assert!(worker.published.request_interrupt(None));
    handle.input(b"new generation".to_vec()).unwrap();
    worker.interrupt_input();
    assert_eq!(
        worker.retained_commands.len(),
        2,
        "one control and post-boundary input must survive"
    );
    fn is_input(command: &Command, expected: &[u8]) -> bool {
        match command {
            Command::Admitted { command, .. } => is_input(command, expected),
            Command::OrderedInput { input, .. } => is_input(input, expected),
            Command::Input(bytes) => bytes == expected,
            _ => false,
        }
    }
    assert!(
        worker
            .retained_commands
            .iter()
            .any(|command| is_input(command, b"new generation"))
    );
    assert!(
        !worker
            .retained_commands
            .iter()
            .any(|command| is_input(command, b"old generation"))
    );
    assert_eq!(handle.command_budget().used(), 2);
    handle.focus(false).unwrap();
    for _ in 0..64 {
        assert!(worker.published.request_interrupt(None));
        worker.interrupt_input();
        assert!(worker.retained_commands.len() <= 3);
        assert!(handle.command_budget().used() <= 3);
        for _ in 0..4 {
            if handle.focus(true) == Err(TerminalSendError::Full) {
                break;
            }
        }
        assert_eq!(handle.focus(false), Err(TerminalSendError::Full));
    }
    assert_eq!(worker.retained_commands.len(), 3);
    assert_eq!(handle.command_budget().used(), 3);
    worker.published.request_shutdown();
    finish_captured_input_worker(worker, &handle, &mut root);
}

#[test]
fn aggregate_command_credit_follows_actual_encoded_backlog_until_cancellation() {
    if isolated_input_case(
        "aggregate_command_credit_follows_actual_encoded_backlog_until_cancellation",
    ) {
        return;
    }
    let (mut worker, events) =
        capture_worker_for(shell_session("stty raw -echo; echo ready; exec sleep 600").launch);
    let mut root = InputRootGuard::new(&worker.published);
    observe_worker_marker(&mut worker, b"ready");
    let (commands, receiver) = command_channel(1);
    worker.commands = receiver;
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events,
            published: worker.published.clone(),
            worker: joins::JoinOwner::empty(worker.published.clone()),
        }),
    };
    handle.input(vec![b'x'; 1024 * 1024]).unwrap();
    let command = worker.commands.try_recv().unwrap();
    worker.handle_command(command);
    assert!(
        !worker.pending_input.is_empty(),
        "actual stopped reader must backpressure the writer"
    );
    assert!(worker.current_admission.is_none());
    assert_eq!(
        handle.command_budget().used(),
        1,
        "bytes retain the consumed command's aggregate credit"
    );
    assert_eq!(handle.focus(true), Err(TerminalSendError::Full));
    worker.published.request_interrupt(None);
    worker.interrupt_input();
    assert_eq!(handle.command_budget().used(), 0);
    handle.focus(true).unwrap();
    let command = worker.commands.try_recv().unwrap();
    worker.handle_command(command);
    assert_eq!(handle.command_budget().used(), 0);
    worker.published.request_shutdown();
    finish_captured_input_worker(worker, &handle, &mut root);
}

#[test]
fn continuous_scroll_refill_yields_after_the_initial_bounded_batch() {
    let (commands, receiver) = command_channel(3);
    let (_events, events) = crossbeam_channel::unbounded();
    let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events,
            worker: joins::JoinOwner::empty(published.clone()),
            published,
        }),
    };
    for _ in 0..3 {
        handle.scroll_lines(1).unwrap();
    }
    let first = receiver.try_recv().unwrap();
    let mut carried = None;
    let mut refills = 0;
    let combined = coalesce_scroll_command_with(first, &receiver, &mut carried, || {
        refills += 1;
        handle
            .scroll_lines(1)
            .expect("folded credits allow an actual concurrent-style refill");
    });
    assert_eq!(
        refills, 2,
        "a producer cannot extend this turn indefinitely"
    );
    assert_eq!(receiver.len(), 2);
    assert!(carried.is_none());
    assert_eq!(handle.command_budget().used(), 3);
    let Command::Admitted { command, .. } = combined else {
        panic!("admission credit lost")
    };
    assert!(matches!(*command, Command::Scroll(ScrollCommand::Delta(3))));
    // An interrupt is independently admitted while the next batch remains queued.
    handle.input(vec![3]).unwrap();
    assert!(
        handle
            .inner
            .published
            .interrupt_requested
            .load(Ordering::Acquire)
    );
}

#[test]
fn stop_fence_refuses_even_a_pre_reserved_physical_release() {
    let (commands, _receiver) = command_channel(1);
    let (_events, events) = crossbeam_channel::unbounded();
    let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
    let mut up = key_input(
        KeyAction::Release,
        TerminalKey::Character('a'),
        None,
        Some('a'),
    );
    up.text = Some("a".into());
    let identity = admission::Identity::key(&up);
    let _original = published.cleanup_pool.press(identity).unwrap();
    published.request_shutdown();
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events,
            worker: joins::JoinOwner::empty(published.clone()),
            published,
        }),
    };
    assert_eq!(handle.key(up), Err(TerminalSendError::Closed));
    assert!(
        handle
            .inner
            .published
            .cleanup_commands
            .lock()
            .unwrap()
            .is_empty()
    );
    assert!(handle.inner.published.cleanup_pool.find(identity).is_some());
}

#[test]
fn large_associated_text_fits_the_bound_reserved_before_handle_ack() {
    let mut terminal = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })
    .unwrap();
    let mut encoder = KeyEncoder::new().unwrap();
    for mode in [
        b"".as_slice(),
        b"\x1b[>3u".as_slice(),
        b"\x1b[>31u".as_slice(),
    ] {
        terminal.vt_write(mode);
        let mut input = key_input(
            KeyAction::Press,
            TerminalKey::Character('a'),
            None,
            Some('a'),
        );
        input.text = Some("aé界".repeat(4096));
        let bound = key_storage_bound(&input).unwrap();
        let encoded = encode_key_input(&terminal, &mut encoder, input).unwrap();
        assert!(!encoded.is_empty());
        assert!(encoded.len() <= bound);
    }
}

#[test]
fn wrapped_handoff_retains_coalesced_input_and_refuses_before_transfer() {
    if isolated_input_case("wrapped_handoff_retains_coalesced_input_and_refuses_before_transfer") {
        return;
    }
    fn control(operation: usize, socket: PathBuf) -> (Command, Box<dyn FnOnce() -> String>) {
        match operation {
            0 => {
                let (reply, result) = crossbeam_channel::bounded(1);
                (
                    Command::PrepareUpgrade(reply),
                    Box::new(move || {
                        result
                            .recv_timeout(Duration::from_secs(1))
                            .unwrap()
                            .err()
                            .expect("Prepare must refuse pending input")
                    }),
                )
            }
            1 => {
                let (reply, result) = crossbeam_channel::bounded(1);
                (
                    Command::Carry { socket, reply },
                    Box::new(move || {
                        result
                            .recv_timeout(Duration::from_secs(1))
                            .unwrap()
                            .err()
                            .expect("Carry must refuse pending input")
                    }),
                )
            }
            2 => {
                let (reply, result) = crossbeam_channel::bounded(1);
                (
                    Command::PreserveForRestart(reply),
                    Box::new(move || {
                        result
                            .recv_timeout(Duration::from_secs(1))
                            .unwrap()
                            .err()
                            .expect("Preserve must refuse pending input")
                    }),
                )
            }
            _ => unreachable!(),
        }
    }
    fn original_input(command: &Command) -> (&Arc<admission::Permit>, &[u8]) {
        let Command::Admitted { permit, command } = command else {
            panic!("accepted input lost its aggregate permit");
        };
        let Command::OrderedInput { input, .. } = command.as_ref() else {
            panic!("accepted input lost its ordering owner");
        };
        let Command::Input(bytes) = input.as_ref() else {
            panic!("the coalescer did not carry the actual accepted bytes");
        };
        (permit, bytes)
    }
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = crate::model::HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("bounded-input-handoff"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: crate::model::HostIncarnation::new("bounded-input-handoff"),
    };
    // A genuine private fixture lease gives Preserve a registered owner. An
    // unregistered legacy rejection would not test its pending-input guard.
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    for operation in 0..3 {
        let mut session = shell_session("stty raw -echo; echo ready; exec sleep 600");
        session.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        let recovery = crate::terminal::holder::Recovery {
            source: session.clone(),
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            options: crate::terminal::TerminalSpawnOptions::default(),
            execution_environment: None,
            execution_recipe: None,
        };
        let (commands, receiver) = command_channel(4);
        let (events, received) = crossbeam_channel::unbounded();
        let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
        let mut worker = Worker::start(
            session,
            TerminalConfig {
                holder_recovery: Some(recovery),
                ..TerminalConfig::default()
            },
            None,
            receiver,
            events,
            published.clone(),
        )
        .unwrap()
        .0;
        let mut root = InputRootGuard::new(&published);
        let handle = TerminalHandle {
            inner: Arc::new(HandleInner {
                commands,
                events: received,
                worker: joins::JoinOwner::empty(published.clone()),
                published,
            }),
        };
        observe_worker_marker(&mut worker, b"ready");
        assert!(
            worker.holder.registered(),
            "Preserve must reach its actual input-drain guard"
        );
        let socket = paths
            .directory
            .join(format!("unreached-input-carry-{operation}.sock"));
        assert!(!socket.exists());
        let bytes = b"same accepted coalesced input".to_vec();
        handle.scroll_lines(1).unwrap();
        handle.input(bytes.clone()).unwrap();
        let (command, response) = control(operation, socket.clone());
        handle.try_command(command).unwrap();
        let first = worker.commands.try_recv().unwrap();
        let mut carried = None;
        let scroll = coalesce_scroll_command(first, &worker.commands, &mut carried);
        worker.handle_command(scroll);
        let (permit, data) = original_input(carried.as_ref().unwrap());
        assert_eq!(data, bytes);
        let original = Arc::downgrade(permit);
        assert!(worker.pending_input.is_empty());
        assert!(worker.retained_commands.is_empty());
        let command = worker.commands.try_recv().unwrap();
        assert!(
            matches!(&command, Command::Admitted { .. }),
            "real producer must wrap control admission"
        );
        assert!(worker.commands.is_empty());
        worker.retain_carried_for_handoff(&command, &mut carried);
        assert!(
            carried.is_none(),
            "wrapped control must recover the coalescer's original carry slot"
        );
        assert_eq!(worker.retained_commands.len(), 1);
        let (retained, data) = original_input(worker.retained_commands.front().unwrap());
        assert!(
            Arc::ptr_eq(&original.upgrade().unwrap(), retained),
            "handoff must retain the same accepted owner, not a reconstructed copy"
        );
        assert_eq!(data, bytes);
        assert!(
            !worker.handle_command(command),
            "pending accepted input must not transfer ownership"
        );
        assert!(response().contains("still pending"));
        assert!(!worker.carried);
        assert!(worker.reader.is_some());
        assert!(!input_root_exited(&root.pidfd).unwrap());
        assert!(!socket.exists());
        assert_eq!(handle.command_budget().used(), 1);

        // Now encode those exact retained bytes into the real pending writer.
        // A zero write budget is an existing seam; no fake reply is supplied.
        worker.test_write_budget = Some(0);
        let input = worker.retained_commands.pop_front().unwrap();
        worker.handle_command(input);
        assert_eq!(worker.pending_input.len(), bytes.len());
        assert_eq!(worker.pending_input.front(), bytes);
        assert_eq!(handle.command_budget().used(), 1);
        let (command, response) = control(operation, socket.clone());
        handle.try_command(command).unwrap();
        let command = worker.commands.try_recv().unwrap();
        assert!(!worker.handle_command(command));
        assert!(response().contains("still pending"));
        assert_eq!(worker.pending_input.front(), bytes);
        assert_eq!(handle.command_budget().used(), 1);
        assert!(!worker.carried);
        assert!(!socket.exists());
        worker.published.request_shutdown();
        finish_captured_input_worker(worker, &handle, &mut root);
    }
}

// These additive stable-cut controls use the actual captured Worker on its
// creating thread. They assert holder/root settlement, not an empty join-owner
// standing in for an OS Worker join.
fn input_handoff_worker(script: &str) -> (Worker, TerminalHandle, InputRootGuard) {
    let mut recipe = LaunchRecipe {
        program: "/usr/bin/python3".into(),
        arguments: vec!["-u".into(), "-c".into(), script.into()],
        working_directory: std::env::current_dir().unwrap(),
        environment: BTreeMap::new(),
        unset_environment: Default::default(),
    };
    // No process-local authority is synthesized for this terminal recipe.
    recipe
        .environment
        .insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
    let (mut worker, events) = capture_worker_for(recipe);
    let root = InputRootGuard::new(&worker.published);
    let (commands, receiver) = command_channel(4);
    worker.commands = receiver;
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events,
            published: worker.published.clone(),
            worker: joins::JoinOwner::empty(worker.published.clone()),
        }),
    };
    (worker, handle, root)
}

#[test]
fn generated_reply_flood_survives_saturated_input_refusal_and_priority_cancellation_exactly_once() {
    if isolated_input_case(
        "generated_reply_flood_survives_saturated_input_refusal_and_priority_cancellation_exactly_once",
    ) {
        return;
    }
    const QUERIES: usize = 64;
    let (mut worker, handle, mut root) = input_handoff_worker(
        r#"
import os, pathlib, select, time, tty
tty.setraw(0)
os.write(1, b'REPLY_FLOOD_READY\x1b[H')
while not pathlib.Path('query-gate').exists(): time.sleep(.005)
os.write(1, b'\x1b[6n' * 64 + b'FLOOD_EMITTED')
reply = bytearray()
while len(reply) < 64 * 6 + 1: reply.extend(os.read(0, 4096))
# Retain any erroneously replayed suffix, rather than truncate at expected size.
while select.select([0], [], [], .1)[0]: reply.extend(os.read(0, 4096))
pathlib.Path('actual-replies.tmp').write_bytes(reply)
os.replace('actual-replies.tmp', 'actual-replies')
while True: time.sleep(1)
"#,
    );
    observe_worker_marker(&mut worker, b"REPLY_FLOOD_READY");
    worker.test_write_budget = Some(0);
    let accepted = vec![b'x'; admission::ORDINARY_BYTES];
    handle.input(accepted.clone()).unwrap();
    worker.handle_command(worker.commands.try_recv().unwrap());
    assert_eq!(worker.pending_input.len(), accepted.len());
    assert_eq!(worker.pending_input.front(), accepted);
    assert_eq!(handle.command_budget().used(), 1);
    fs::write("query-gate", b"query").unwrap();
    observe_worker_marker(&mut worker, b"FLOOD_EMITTED");
    fn replies(worker: &Worker) -> Vec<Vec<u8>> {
        worker
            .effects
            .borrow()
            .ordered
            .iter()
            .filter_map(|effect| match effect {
                Effect::PtyWrite(bytes) => Some(bytes.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    }
    assert_eq!(replies(&worker), vec![b"\x1b[1;1R".to_vec(); QUERIES]);
    assert!(worker.reply_pending());
    assert_eq!(worker.pending_input.front(), accepted);

    // Actual public preparation queues and is rejected by the owner because
    // the original accepted bytes/replies still belong to this Worker.
    let token = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (resumed, restarted) = crossbeam_channel::bounded(1);
    thread::scope(|scope| {
        let handle = &handle;
        let token = token.clone();
        let prepare = scope.spawn(move || handle.prepare_input_handoff(token, resumed));
        let command = worker
            .commands
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(!worker.handle_command(command));
        let error = prepare.join().unwrap().unwrap_err();
        assert!(error.contains("still pending"), "{error}");
    });
    assert!(!token.load(Ordering::Acquire));
    assert_eq!(
        restarted.recv_timeout(Duration::from_secs(1)).unwrap(),
        Ok(())
    );
    assert!(worker.reader.is_some());
    assert!(worker.reader_wake.is_some());
    assert_eq!(worker.pending_input.front(), accepted);
    assert_eq!(replies(&worker), vec![b"\x1b[1;1R".to_vec(); QUERIES]);
    assert!(!input_root_exited(&root.pidfd).unwrap());

    // Cancel only user input. The real deferred parser replies must be sent
    // before the later raw interrupt, exactly once, through the original PTY.
    worker.test_write_budget = None;
    handle.input(vec![3]).unwrap();
    worker.interrupt_input();
    for _ in 0..4 {
        worker.flush_pty_input();
        worker.drain_effects();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::path::Path::new("actual-replies").exists() {
        assert!(
            Instant::now() < deadline,
            "actual child did not receive conserved replies"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let expected = [b"\x1b[1;1R".repeat(QUERIES), vec![3]].concat();
    assert_eq!(fs::read("actual-replies").unwrap(), expected);
    assert!(worker.pending_input.is_empty());
    assert!(!worker.reply_pending());
    assert_eq!(handle.command_budget().used(), 0);
    worker.published.request_shutdown();
    finish_captured_input_worker(worker, &handle, &mut root);
}

#[test]
fn timed_out_queued_input_preparation_never_stops_reader_or_loses_accepted_input() {
    if isolated_input_case(
        "timed_out_queued_input_preparation_never_stops_reader_or_loses_accepted_input",
    ) {
        return;
    }
    let (mut worker, handle, mut root) = input_handoff_worker(
        r#"
import os, time, tty
tty.setraw(0)
os.write(1, b'TIMEOUT_READY')
while True: time.sleep(1)
"#,
    );
    observe_worker_marker(&mut worker, b"TIMEOUT_READY");
    worker.test_write_budget = Some(0);
    let bytes = b"accepted before timed-out stable cut".to_vec();
    handle.input(bytes.clone()).unwrap();
    let reader = worker.reader.as_ref().unwrap().thread().id();
    let token = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (resumed, restarted) = crossbeam_channel::bounded(1);
    let started = Instant::now();
    let error = handle
        .prepare_input_handoff(token.clone(), resumed)
        .unwrap_err();
    assert!(error.contains("remains unconfirmed"), "{error}");
    assert!(started.elapsed() >= Duration::from_secs(3));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!token.load(Ordering::Acquire));
    assert!(matches!(
        restarted.try_recv(),
        Err(crossbeam_channel::TryRecvError::Empty)
    ));
    assert_eq!(worker.reader.as_ref().unwrap().thread().id(), reader);
    // Owner has not processed either accepted command yet. The exact timeout
    // token must cancel without discarding the earlier bytes or touching reader.
    worker.handle_command(worker.commands.try_recv().unwrap());
    assert_eq!(worker.pending_input.front(), bytes);
    worker.handle_command(worker.commands.try_recv().unwrap());
    assert_eq!(
        restarted.recv_timeout(Duration::from_secs(1)).unwrap(),
        Ok(())
    );
    assert_eq!(worker.reader.as_ref().unwrap().thread().id(), reader);
    assert!(worker.reader_wake.is_some());
    assert_eq!(worker.pending_input.front(), bytes);
    assert!(worker.input_handoff.is_none());
    assert!(!worker.published.input_order.lock().unwrap().handoff);
    handle
        .input(b"new accepted input after rollback".to_vec())
        .unwrap();
    worker.handle_command(worker.commands.try_recv().unwrap());
    assert!(worker.pending_input.len() > bytes.len());
    worker.published.request_shutdown();
    finish_captured_input_worker(worker, &handle, &mut root);
}

#[test]
fn inactive_pty_and_exited_child_still_observe_capture_cancellation() {
    let (commands, command_rx) = command_channel(1);
    let (_pty, pty_rx) = crossbeam_channel::bounded(1);
    let (_exit, exit_rx) = crossbeam_channel::bounded(1);
    let (capture, capture_rx) = crossbeam_channel::bounded(1);
    capture.send(()).unwrap();
    let (done, result) = crossbeam_channel::bounded(1);
    let waiter = thread::spawn(move || {
        let capture = matches!(
            wait_for_worker_event(
                &command_rx,
                &pty_rx,
                &exit_rx,
                &capture_rx,
                false,
                false,
                Duration::MAX,
                None
            ),
            WorkerEvent::Capture
        );
        done.send(capture).unwrap();
    });
    let observed = result.recv_timeout(Duration::from_secs(1));
    if observed.is_err() {
        // Old command-only branch is released and joined before the assertion
        // fails; this regression never leaks its waiter on the negative path.
        commands.send(Command::Shutdown).unwrap();
    }
    waiter.join().unwrap();
    assert_eq!(
        observed.unwrap(),
        true,
        "queued exact cancellation was ignored"
    );
}

#[test]
fn reader_cut_refuses_a_consumed_child_query_until_its_exact_reply_is_delivered() {
    if isolated_input_case(
        "reader_cut_refuses_a_consumed_child_query_until_its_exact_reply_is_delivered",
    ) {
        return;
    }
    let (mut worker, handle, mut root) = input_handoff_worker(
        r#"
import os, pathlib, time, tty
tty.setraw(0)
os.write(1, b'PRECUT_READY\x1b[H')
while not pathlib.Path('precut-query-gate').exists(): time.sleep(.005)
os.write(1, b'\x1b[6n')
pathlib.Path('precut-query-emitted').write_bytes(b'emitted')
reply = bytearray()
while not reply.endswith(b'R'): reply.extend(os.read(0, 64))
pathlib.Path('precut-reply.tmp').write_bytes(reply)
os.replace('precut-reply.tmp', 'precut-reply')
while True: time.sleep(1)
"#,
    );
    // Consume the entire initial write, including home, before the only later
    // raw output is this real query. Do not parse that query before the cut.
    observe_worker_marker(&mut worker, b"PRECUT_READY\x1b[H");
    assert!(worker.pending_input.is_empty());
    assert!(!worker.reply_pending());
    worker.test_write_budget = Some(0);
    let old_reader = worker.reader.as_ref().unwrap().thread().id();
    fs::write("precut-query-gate", b"query").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while worker.pty_rx.is_empty() {
        assert!(
            Instant::now() < deadline,
            "actual reader never queued child query"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        worker.pending_input.is_empty(),
        "first drain check must pass"
    );
    let token = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (resumed, restarted) = crossbeam_channel::bounded(1);
    thread::scope(|scope| {
        let handle = &handle;
        let token = token.clone();
        let prepare = scope.spawn(move || handle.prepare_input_handoff(token, resumed));
        let command = worker
            .commands
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(!worker.handle_command(command));
        let error = prepare.join().unwrap().unwrap_err();
        assert!(error.contains("still pending"), "{error}");
    });
    assert!(!token.load(Ordering::Acquire));
    assert_eq!(
        restarted.recv_timeout(Duration::from_secs(1)).unwrap(),
        Ok(())
    );
    assert_ne!(
        worker.reader.as_ref().unwrap().thread().id(),
        old_reader,
        "refused cut must actually restart joined original reader"
    );
    assert!(worker.reader_wake.is_some());
    assert_eq!(worker.pending_input.front(), b"\x1b[1;1R");
    assert_eq!(worker.pending_input.len(), 6);
    assert!(!worker.reply_pending());
    assert!(!worker.published.input_order.lock().unwrap().handoff);
    assert!(!input_root_exited(&root.pidfd).unwrap());
    assert!(!std::path::Path::new("precut-reply").exists());
    worker.test_write_budget = None;
    assert!(worker.flush_pty_input());
    while !std::path::Path::new("precut-reply").exists() {
        assert!(
            Instant::now() < deadline,
            "child did not receive retained precommit reply"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fs::read("precut-reply").unwrap(), b"\x1b[1;1R");
    assert!(worker.pending_input.is_empty());
    worker.published.request_shutdown();
    finish_captured_input_worker(worker, &handle, &mut root);
}

#[test]
fn ambiguous_adoption_commit_keeps_the_original_reader_prefix_and_parent_owner_until_explicit_close()
 {
    const NAME: &str = "ambiguous_adoption_commit_keeps_the_original_reader_prefix_and_parent_owner_until_explicit_close";
    if isolated_input_case(NAME) {
        return;
    }
    let root = std::env::current_dir().unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = crate::model::HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("startup-prefix-custody"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: crate::model::HostIncarnation::new("startup-prefix-custody"),
    };
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let mut session = shell_session("");
    session.launch.program = "/usr/bin/python3".into();
    session.launch.arguments = vec![
        "-c".into(),
        r#"import os,pathlib,time,tty
tty.setraw(0)
pathlib.Path('launches').write_bytes(b'one')
os.write(1,b'ORIGINAL_READY')
while not pathlib.Path('emit').exists(): time.sleep(.005)
os.write(1,b'RAW_PREFIX\x1b[6n')
pathlib.Path('emitted').write_bytes(b'yes')
reply=os.read(0,128)
pathlib.Path('unexpected-reply').write_bytes(reply)
while True: time.sleep(1)
"#
        .into(),
    ];
    session.launch.working_directory = root.clone();
    session.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    let recovery = crate::terminal::holder::Recovery {
        source: session.clone(),
        previous_sources: Vec::new(),
        relaunches: Vec::new(),
        options: crate::terminal::TerminalSpawnOptions::default(),
        execution_environment: None,
        execution_recipe: None,
    };
    let original = TerminalHandle::spawn(
        session.clone(),
        TerminalConfig {
            holder_recovery: Some(recovery.clone()),
            ..TerminalConfig::default()
        },
        |_| {},
    )
    .unwrap();
    let owner = original.holder_identity().unwrap().clone();
    let original_record = owner.clone();
    let holder_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, owner.holder.pid, 0) };
    assert!(holder_fd >= 0 && owner.holder.is_current());
    let original_holder = unsafe { File::from_raw_fd(holder_fd as i32) };
    let mut original_root = InputRootGuard::new(&original.inner.published);
    wait_for_text(&original, "ORIGINAL_READY");
    let socket = crate::daemon::holder_registry::socket(
        &crate::daemon::holder_registry::directory(crate::daemon::holder_registry::Kind::Terminal)
            .unwrap(),
        session.id,
    );
    original.carry(socket.clone()).unwrap();
    fs::write(root.join("emit"), b"query").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("emitted").exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let parent = Arc::new(Mutex::new(None::<TerminalHandle>));
    let captured = parent.clone();
    let error = TerminalHandle::spawn_with_execution_installed(
        session,
        TerminalConfig {
            adoption: Some(socket),
            holder_recovery: Some(recovery),
            ..TerminalConfig::default()
        },
        None,
        |_| {},
        move |handle| {
            assert!(owner.root.is_current());
            assert!(owner.holder.is_current());
            handle.fail_startup_commit_after_ack_for_test();
            *captured.lock().unwrap() = Some(handle.clone());
            Ok(())
        },
    )
    .unwrap_err();
    let TerminalStartError::CustodyRetained { handle, message } = error else {
        panic!("expected real committed original custody");
    };
    assert!(message.contains("AFTER actual holder commit acknowledgement"));
    let retained = parent.lock().unwrap().as_ref().unwrap().clone();
    assert!(retained.same_worker(&handle));
    drop(handle); // the error is deliberately not the only parent owner
    assert!(retained.startup_unresolved());
    let (_, pending, cleanup) = retained.retained_startup_state_for_test();
    assert!(
        pending,
        "actual Holder must remain adoption_pending after uncertain outcome"
    );
    assert!(
        !cleanup,
        "no exact original family cleanup has been authorized"
    );
    assert!(!retained.is_completed());
    assert!(
        retained
            .startup_diagnostic()
            .unwrap()
            .contains("AFTER actual")
    );
    assert_eq!(
        retained.retained_startup_bytes_for_test(),
        b"RAW_PREFIX\x1b[6n"
    );
    assert_eq!(
        retained.input(b"must not be admitted".to_vec()),
        Err(TerminalSendError::Closed)
    );
    assert_eq!(retained.shutdown(), Err(TerminalSendError::Closed));
    assert_eq!(
        retained.retained_startup_bytes_for_test(),
        b"RAW_PREFIX\x1b[6n"
    );
    assert!(
        !root.join("unexpected-reply").exists(),
        "a parked reader must not answer the raw query"
    );
    assert_eq!(fs::read(root.join("launches")).unwrap(), b"one");
    assert_eq!(
        retained.close_checked(false),
        super::super::ipc::TerminalCloseResult::Active
    );
    let (_, pending, cleanup) = retained.retained_startup_state_for_test();
    assert!(
        pending && !cleanup,
        "failed checked close cannot certify cleanup or drop provisional retention"
    );
    assert_eq!(
        retained.retained_startup_bytes_for_test(),
        b"RAW_PREFIX\x1b[6n"
    );
    assert_eq!(
        retained.close_checked(true),
        super::super::ipc::TerminalCloseResult::Closed
    );
    assert!(retained.is_completed());
    assert!(retained.worker_joined_for_test());
    original_root.settled();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !input_root_exited(&original_holder).unwrap() {
        assert!(
            Instant::now() < deadline,
            "the same original holder must exit after explicit proved cleanup"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!crate::daemon::holder_registry::retirement_pending(&original_record).unwrap());
    parent.lock().unwrap().take();
}
