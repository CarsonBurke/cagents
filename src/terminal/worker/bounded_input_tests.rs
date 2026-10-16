// Bounded input owning cases, ported semantically from reviewed NC-E3 14e.
// Real children stay under the outer original-child/family guardian.
// Exact root fallback only; the existing outer guardian proves full-family retirement.
// A failed assertion never rediscovers a PID or discards the original capability.
struct InputRootGuard {
    pidfd: File,
    armed: bool,
}
impl InputRootGuard {
    fn new(published: &PublishedState) -> Self {
        Self {
            pidfd: published
                .shutdown_pidfd
                .get()
                .expect("actual startup published root capability")
                .try_clone()
                .unwrap(),
            armed: true,
        }
    }
    fn settled(&mut self) {
        self.armed = false;
    }
}
impl Drop for InputRootGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = close_processes::signal(&self.pidfd, libc::SIGCONT);
            let _ = close_processes::signal(&self.pidfd, libc::SIGKILL);
        }
    }
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
    let handle = TerminalHandle::spawn(
            shell_session("stty -echo -icanon; trap 'echo interrupted; exit 0' INT; echo ready; while :; do echo tick; sleep 0.05; done"),
            TerminalConfig::default(), |_| {},
        ).unwrap();
    let mut root = InputRootGuard::new(&handle.inner.published);
    wait_for_text(&handle, "ready");
    let before = handle.snapshot().generation;
    let mut backpressure = false;
    for _ in 0..3 {
        match handle.paste("x".repeat(1024 * 1024), PasteOrigin::Clipboard) {
            Ok(()) => {}
            Err(TerminalSendError::Full) => backpressure = true,
            Err(error) => panic!("unexpected paste admission failure: {error}"),
        }
    }
    assert!(
        backpressure,
        "the storage bound must refuse excess input before acknowledgement"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        let snapshot = handle.snapshot();
        handle.acknowledge_redraw(snapshot.generation);
        backpressure |= handle.events().try_iter().any(|event| {
            matches!(event,
                TerminalEvent::Error { message, .. } if message.contains("isn't reading input"))
        });
        if backpressure && snapshot.generation > before {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        backpressure,
        "pending byte bound must report backpressure to client before acknowledgement"
    );
    assert!(
        handle.snapshot().generation > before,
        "output stalled behind PTY write"
    );
    handle.input(vec![3]).unwrap();
    wait_for_text(&handle, "interrupted");
    let started = std::time::Instant::now();
    handle.shutdown().unwrap();
    root.settled();
    assert!(started.elapsed() < WORKER_JOIN_TIMEOUT + Duration::from_millis(100));
}

#[test]
fn nonreading_child_shutdown_is_bounded_with_a_full_pending_paste() {
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
    root.settled();
    assert!(started.elapsed() < WORKER_JOIN_TIMEOUT + Duration::from_millis(100));
}

#[test]
fn encoded_owned_release_survives_priority_interrupt_in_userspace_and_kernel() {
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
        let reservation = joins::reserve().unwrap();
        let (mut worker, events) = capture_worker_for(launch);
        let mut root = InputRootGuard::new(&worker.published);
        worker.published.join_reservation.set(reservation).unwrap();
        let (commands, receiver) = command_channel(1);
        worker.commands = receiver;
        let handle = TerminalHandle {
            inner: Arc::new(HandleInner {
                commands,
                events,
                published: worker.published.clone(),
                worker: Arc::new(Mutex::new(None)),
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
        close_processes::signal(&worker.child.shutdown_pidfd().unwrap(), libc::SIGCONT).unwrap();
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
        worker.run();
        handle.shutdown().unwrap();
        root.settled();
    }
}

#[test]
fn reserved_cleanup_does_not_overtake_its_still_queued_press() {
    let reservation = joins::reserve().unwrap();
    let (mut worker, events) = capture_worker_for(
        shell_session("stty raw -echo; printf '\\033[>3uready'; exec sleep 600").launch,
    );
    let mut root = InputRootGuard::new(&worker.published);
    worker.published.join_reservation.set(reservation).unwrap();
    observe_worker_marker(&mut worker, b"ready");
    let (commands, receiver) = command_channel(1);
    worker.commands = receiver;
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events,
            published: worker.published.clone(),
            worker: Arc::new(Mutex::new(None)),
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
    worker.run();
    handle.shutdown().unwrap();
    root.settled();
}

#[test]
fn aggregate_credits_bound_interrupt_stash_while_producers_try_to_refill() {
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
            worker: Arc::new(Mutex::new(None)),
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
    worker.run();
    handle.shutdown().unwrap();
    root.settled();
}

#[test]
fn aggregate_command_credit_follows_actual_encoded_backlog_until_cancellation() {
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
            worker: Arc::new(Mutex::new(None)),
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
    worker.run();
    handle.shutdown().unwrap();
    root.settled();
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
            published,
            worker: Arc::new(Mutex::new(None)),
        }),
    };
    for _ in 0..3 {
        handle.scroll_delta(1).unwrap();
    }
    let first = receiver.try_recv().unwrap();
    let mut carried = None;
    let mut refills = 0;
    let combined = coalesce_scroll_command_with(first, &receiver, &mut carried, || {
        refills += 1;
        handle
            .scroll_delta(1)
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
    published.input_order.lock().unwrap().closing = true;
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events,
            published,
            worker: Arc::new(Mutex::new(None)),
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
