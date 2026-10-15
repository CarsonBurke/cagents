// These are owning HOST contracts, not native application evidence. The Codex
// peer is a private real app-server process; no Actor.child is injected. The
// ended-pane seed models a legacy database without historical generation proof.

// Own the exact subprocess through a fixed deadline. A failed/hung child never
// turns exit0/zero selected tests into owning-contract evidence, and its private
// runtime/receipts remain available for diagnosis instead of TempDir deletion.
struct OwningCloseDirectory(Option<tempfile::TempDir>);
impl Drop for OwningCloseDirectory {
    fn drop(&mut self) {
        if let Some(directory) = self.0.take() {
            eprintln!(
                "owning close fixture evidence retained at {}",
                directory.keep().display()
            );
        }
    }
}

struct OwningCloseChild {
    child: Option<std::process::Child>,
    pidfd: Option<std::fs::File>,
    reservation: Option<crate::daemon::child_reaper::Reservation>,
}
impl OwningCloseChild {
    fn status(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        let status = self
            .child
            .as_mut()
            .expect("original fixture child")
            .try_wait()?;
        if status.is_some() {
            self.child.take();
            self.pidfd.take();
            self.reservation.take();
        }
        Ok(status)
    }
}
impl Drop for OwningCloseChild {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        let Some(mut child) = self.child.take() else {
            return;
        };
        let reservation = self
            .reservation
            .take()
            .expect("spawn-time original-child credit");
        reservation.quarantine();
        if let Some(pidfd) = &self.pidfd {
            // SAFETY: this is the retained pidfd captured for our still-owned
            // original Child. No numeric PID lookup/reopening authorizes it.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        } else {
            // The sole unreaped Child still owns this PID if capture failed.
            let _ = child.kill();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        match self.pidfd.take() {
            Some(pidfd) => {
                if let Err((error, child, pidfd, reservation)) = reservation.retain(child, pidfd) {
                    reservation.preserve_suspended(
                        child,
                        Some(pidfd),
                        &format!("owning close fixture: {error}"),
                    );
                }
            }
            None => reservation.preserve_suspended(
                child,
                None,
                "owning close fixture pidfd capture failed",
            ),
        }
    }
}

fn owning_close_isolated(name: &str) -> Option<tempfile::TempDir> {
    use std::{
        os::{
            fd::FromRawFd,
            unix::{fs::OpenOptionsExt, process::CommandExt},
        },
        process::Stdio,
    };
    const CHILD: &str = "CAGENTS_EXECUTION_SERVICE_TEST";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return None;
    }
    let mut directory = OwningCloseDirectory(Some(
        tempfile::Builder::new()
            .prefix("cagents-owning-close-")
            .tempdir()
            .unwrap(),
    ));
    let root = directory.0.as_ref().unwrap().path();
    for name in [
        "bin",
        "home/.claude",
        "home/.codex",
        "data",
        "config",
        "cache",
        "tmp",
        "run",
        "project",
    ] {
        std::fs::create_dir_all(root.join(name)).unwrap();
    }
    install_claude(&root.join("bin/claude"));
    let open_log = |name| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root.join(name))
            .unwrap()
    };
    let stdout = open_log("child.stdout.log");
    let stderr = open_log("child.stderr.log");
    let exact = format!("host::agent_execution::tests::{name}");
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
        .env_clear()
        .env(CHILD, name)
        .env("CAGENTS_EXECUTION_FIXTURE", root)
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root.join("tmp"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if name == "combined_close_retained_actor_and_real_pair_keep_receipts_across_late_conflict" {
        command.env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1");
    }
    // SAFETY: only async-signal-safe setrlimit calls run between fork and exec;
    // no allocation, mutex, or parent-side mutation is performed here.
    unsafe {
        command.pre_exec(|| {
            let file = libc::rlimit {
                rlim_cur: 4 * 1024 * 1024,
                rlim_max: 4 * 1024 * 1024,
            };
            let core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &file) != 0
                || libc::setrlimit(libc::RLIMIT_CORE, &core) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let reservation = crate::daemon::child_reaper::reserve().unwrap();
    let child = command.spawn().unwrap();
    let pid = child.id();
    let mut original = OwningCloseChild {
        child: Some(child),
        pidfd: None,
        reservation: Some(reservation),
    };
    // SAFETY: capture occurs while this original Child is exclusively retained
    // and unreaped. No successor numeric PID can replace that kernel identity.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(descriptor >= 0, "cannot capture original fixture pidfd");
    // SAFETY: successful pidfd_open returned a new owned descriptor.
    original.pidfd = Some(unsafe { std::fs::File::from_raw_fd(descriptor as i32) });
    let startup = serde_json::json!({"schema":"owning-close-test-child-v1", "test":exact,
        "pid":pid, "authority":"retained_original_child_and_pidfd", "deadline_seconds":90,
        "stdout_limit_bytes":4*1024*1024, "stderr_limit_bytes":4*1024*1024});
    std::fs::write(
        root.join("child-startup.json"),
        serde_json::to_vec_pretty(&startup).unwrap(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = original.status().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "owning test deadline exceeded: {exact}"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let out = std::fs::read(root.join("child.stdout.log")).unwrap();
    let err = std::fs::read(root.join("child.stderr.log")).unwrap();
    assert!(out.len() <= 4 * 1024 * 1024 && err.len() <= 4 * 1024 * 1024);
    let text = String::from_utf8_lossy(&out);
    let named = text
        .lines()
        .filter(|line| line.trim() == format!("test {exact} ... ok"))
        .count();
    let summaries = text
        .lines()
        .filter(|line| line.starts_with("test result:"))
        .collect::<Vec<_>>();
    let one = text
        .lines()
        .filter(|line| *line == "running 1 test")
        .count()
        == 1
        && named == 1
        && summaries.len() == 1
        && summaries[0].starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;");
    let result = serde_json::json!({"schema":"owning-close-test-result-v1", "test":exact,
        "exit_code":status.code(), "exact_named_one_passed":one, "original_child_reaped":true,
        "native_or_full_cohort_retirement_claimed":false});
    std::fs::write(
        root.join("child-result.json"),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
    assert!(
        status.success() && one,
        "owning test did not execute exactly one passing named contract: {exact}; inspect private retained child.stdout.log/child.stderr.log"
    );
    Some(directory.0.take().unwrap())
}

struct LocalCodexPresentationFixture {
    host: SharedHost,
    path: std::path::PathBuf,
    agent: Agent,
    run: Run,
    pane: crate::workspace_model::views::Pane,
    handle: chat::runtime::Handle,
    owner: crate::daemon::holder_registry::Record,
    pair: crate::terminal::TerminalHandle,
    pair_source: crate::model::TerminalSession,
    proof: HostSessionProof,
    malformed_receipt: Arc<std::sync::Mutex<Option<(u64, u64)>>>,
    _lease: crate::daemon::HostLease,
    _notifications: crate::host::notifications::Broker,
}

impl LocalCodexPresentationFixture {
    fn start() -> Self {
        let (host, mut request, path) = fixture();
        let root = path.parent().unwrap();
        let program = root.join("bin/codex");
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            let marker = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(root.join("combined-close-owner.json"))
                .unwrap();
            serde_json::to_writer(marker, &serde_json::json!({"root": root.canonicalize().unwrap(), "uid": std::fs::metadata(root).unwrap().uid()})).unwrap();
        }
        std::fs::write(
            &program,
            include_str!("../../../tests/fixtures/combined-close-codex.py"),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let proof = host.lock().unwrap().proof().clone();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        let notifications = crate::host::notifications::Broker::start(
            &host,
            crate::notifications::Application::Cagents,
        )
        .unwrap();
        host.lock().unwrap().notifications = Some(notifications.handle());
        host.lock().unwrap().enable_holder_recovery();
        let Destination::New(spec) = &mut request.destination else {
            unreachable!()
        };
        spec.provider = Provider::Codex;
        spec.model = Some("fixture-model".into());
        spec.reasoning_effort = Some("low".into());
        let accepted = submit(&host, &Authority::default(), request).unwrap();
        let run = settled(&host, accepted.run.id);
        assert_eq!(run.state, State::Succeeded, "{run:?}");
        assert_eq!(run.result.text, "LOCAL_CODEX_CLOSE_RESULT");
        let store = registry(&host).unwrap();
        let agent = store.lock().unwrap().agent(accepted.agent.id).unwrap();
        let HostedSession::Chat(original) = agent.session.as_ref().unwrap() else {
            unreachable!()
        };
        let (handle, hosted) = {
            let state = host.lock().unwrap();
            let record = &state.chats[&original.id];
            (record.handle.clone(), record.session.clone())
        };
        assert!(
            handle.actor_identity().is_none(),
            "this is the actual Local facade, not Remote"
        );
        assert!(
            handle.provider_process().is_none(),
            "OwnedUnixWebSocket server is owned by native_holder, not Actor.child"
        );
        let owner = handle
            .native_holder_record()
            .expect("actual owned native server")
            .clone();
        assert!(!chat::runtime::Handle::is_remote_record(&owner));
        let (recovery, exited) = chat::native_holder::inspect(&owner, &proof).unwrap();
        assert!(!exited);
        assert_eq!(recovery.server_process.as_ref(), Some(&owner.root));
        let log: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(root.join("codex-processes.jsonl"))
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(log["pid"].as_i64(), Some(i64::from(owner.root.pid)));
        assert_eq!(log["start"].as_u64(), Some(owner.root.start));

        let recipe = crate::model::LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: vec![],
            working_directory: root.join("project"),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        // This is the real production pairing publisher after its matched-CLI
        // recipe boundary, not a manually assigned HostState/Agent pair.
        create_console_from_recipe(&host, &store, agent.id, &hosted, &handle, None, recipe)
            .unwrap_or_else(|error| match error {
                ConsoleError::Known(message) | ConsoleError::Uncertain(message) => {
                    panic!("pairing failed: {message}")
                }
            });
        let agent = store.lock().unwrap().agent(agent.id).unwrap();
        let pair_source = agent.console.as_ref().unwrap().clone();
        let pair = host.lock().unwrap().terminals[&pair_source.id]
            .handle
            .clone();
        let (published, instance) = {
            let state = host.lock().unwrap();
            let record = &state.chats[&original.id];
            assert!(
                record
                    .paired_terminal
                    .as_ref()
                    .is_some_and(|owned| owned.same_worker(&pair))
            );
            (record.session.clone(), record.execution_instance)
        };
        assert!(published.terminal.is_some());
        let HostedSession::Chat(retained) = agent.session.as_ref().unwrap() else {
            unreachable!()
        };
        assert!(
            retained.terminal.is_none(),
            "bind_agent_console retains the exact earlier Agent source"
        );
        assert_ne!(
            &published, retained,
            "fixture must exercise the genuine fresh-pair alias gap"
        );
        assert!(
            !host
                .lock()
                .unwrap()
                .upgrade_chats
                .contains_key(&retained.id)
        );
        let (paired_recovery, _) = chat::native_holder::inspect(&owner, &proof).unwrap();
        assert!(
            paired_recovery.source == *retained
                || paired_recovery.previous_sources.contains(retained)
        );
        assert_eq!(
            paired_recovery.paired_holder.as_ref(),
            pair.holder_identity()
        );
        assert!(!instance.is_nil());

        let mut ended = pair_source.clone();
        ended.continuity = crate::model::TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
        let pane = crate::workspace_model::views::Pane {
            id: crate::model::CardId::from_uuid(agent.id.as_uuid()),
            title: "Legacy Local Codex presentation".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 1.0,
            kind: crate::model::CardKind::Terminal(ended),
            scope: None,
        };
        store
            .lock()
            .unwrap()
            .apply(&ModelEdit::SeedView {
                scope: agent.placement.clone(),
                panes: vec![pane.clone()],
                closed: vec![],
            })
            .unwrap();
        Self {
            host,
            path,
            agent,
            run,
            pane,
            handle,
            owner,
            pair,
            pair_source,
            proof,
            malformed_receipt: Arc::new(std::sync::Mutex::new(None)),
            _lease: lease,
            _notifications: notifications,
        }
    }

    fn inspect(&self) -> crate::agent_execution::PresentationCloseTarget {
        let Reply::PresentationClose(Some(target)) = process(
            &self.host,
            Authority::default(),
            Operation::InspectPresentationClose(self.agent.id),
        )
        .unwrap() else {
            panic!("missing current close target")
        };
        assert_eq!(
            target.backend,
            crate::agent_execution::PresentationCloseBackend::Process {
                pid: self.owner.root.pid,
                start: self.owner.root.start
            }
        );
        assert_eq!(target.observation.agent, self.agent);
        assert_eq!(target.observation.pane, self.pane);
        target
    }

    fn rpc_counts(&self) -> (usize, usize) {
        let root = self.path.parent().unwrap();
        let spawns = std::fs::read_to_string(root.join("codex-processes.jsonl"))
            .unwrap()
            .lines()
            .count();
        let turns = std::fs::read_to_string(root.join("codex-rpc.jsonl"))
            .unwrap()
            .lines()
            .filter(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["method"] == "turn/start"
            })
            .count();
        (spawns, turns)
    }

    fn receipt_path(&self) -> std::path::PathBuf {
        crate::daemon::holder_registry::directory(crate::daemon::holder_registry::Kind::Native)
            .unwrap()
            .join(format!(
                "{}-{}-{}.closed",
                self.owner.terminal, self.owner.holder.pid, self.owner.holder.start
            ))
    }
}

impl Drop for LocalCodexPresentationFixture {
    fn drop(&mut self) {
        // Only a malformed file with this fixture's content is removable here.
        // The failure test additionally checks its original inode before unlink.
        let receipt = self.receipt_path();
        use std::os::unix::fs::MetadataExt;
        let owned = *self.malformed_receipt.lock().unwrap();
        if let (Some(expected), Ok(metadata)) = (owned, std::fs::symlink_metadata(&receipt))
            && !metadata.file_type().is_symlink()
            && (metadata.dev(), metadata.ino()) == expected
            && std::fs::read(&receipt).ok().as_deref()
                == Some(b"owned combined-close transient failure")
        {
            let _ = std::fs::remove_file(receipt);
        }
        let _ = self.handle.close_for_resume();
        let _ = self.pair.close_with_receipt(&self.pair_source, &self.proof);
    }
}

#[test]
fn combined_close_local_codex_without_actor_child_uses_real_server_and_fresh_pair_alias() {
    if owning_close_isolated(
        "combined_close_local_codex_without_actor_child_uses_real_server_and_fresh_pair_alias",
    )
    .is_some()
    {
        return;
    }
    let fixture = LocalCodexPresentationFixture::start();
    let store = registry(&fixture.host).unwrap();
    let before = store.lock().unwrap().model().unwrap();
    let counts = fixture.rpc_counts();
    assert_eq!(counts, (1, 1));
    assert!(
        close(&fixture.host, &Authority::default(), fixture.agent.id)
            .unwrap_err()
            .contains("historical generation proof")
    );
    assert_eq!(store.lock().unwrap().model().unwrap(), before);
    assert!(fixture.owner.root.is_current());
    assert!(!fixture.pair.is_completed());
    assert_eq!(fixture.rpc_counts(), counts);
    let target = fixture.inspect();
    let mut wrong_instance = target.clone();
    wrong_instance.actor_instance = Some(uuid::Uuid::new_v4());
    assert!(
        process(
            &fixture.host,
            Authority::default(),
            Operation::ConfirmPresentationClose {
                observed: wrong_instance
            }
        )
        .is_err()
    );
    assert!(fixture.owner.root.is_current());
    assert!(!fixture.pair.is_completed());
    assert_eq!(fixture.rpc_counts(), counts);
    let Reply::Closed(closed) = process(
        &fixture.host,
        Authority::default(),
        Operation::ConfirmPresentationClose { observed: target },
    )
    .unwrap() else {
        panic!("not closed")
    };
    assert!(closed.closed);
    assert!(!fixture.owner.root.is_current());
    assert!(!fixture.owner.holder.is_current());
    assert!(
        crate::terminal::holder::cleanup::confirmed_owner(
            fixture.pair.holder_identity().unwrap(),
            &fixture.proof
        )
        .unwrap()
    );
    let HostedSession::Chat(source) = fixture.agent.session.as_ref().unwrap() else {
        unreachable!()
    };
    let receipt = chat::native_holder::closed_source(source, &fixture.proof)
        .unwrap()
        .unwrap();
    assert_eq!(receipt.0, fixture.owner);
    assert!(receipt.1);
    assert_eq!(
        store.lock().unwrap().agent_run(fixture.run.id).unwrap(),
        fixture.run
    );
    assert!(
        store
            .lock()
            .unwrap()
            .inspect_presentation_repair(&Origin::User, fixture.agent.id)
            .unwrap()
            .is_none()
    );
    assert!(
        !fixture
            .host
            .lock()
            .unwrap()
            .native_closing
            .contains_key(&source.id)
    );
    assert_eq!(fixture.rpc_counts(), counts);
    assert_eq!(
        close(&fixture.host, &Authority::default(), fixture.agent.id).unwrap(),
        closed
    );
    assert_eq!(fixture.rpc_counts(), counts);
}

#[test]
fn combined_close_local_codex_failure_keeps_exact_holder_then_retries_without_respawn() {
    if owning_close_isolated(
        "combined_close_local_codex_failure_keeps_exact_holder_then_retries_without_respawn",
    )
    .is_some()
    {
        return;
    }
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let fixture = LocalCodexPresentationFixture::start();
    let target = fixture.inspect();
    let store = registry(&fixture.host).unwrap();
    let before = store.lock().unwrap().model().unwrap();
    let counts = fixture.rpc_counts();
    let path = fixture.receipt_path();
    let injected = path.clone();
    let (identity, original) = std::sync::mpsc::channel();
    let owned_receipt = fixture.malformed_receipt.clone();
    crate::host::chat::before_presentation_provider_close(move || {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(injected)
            .unwrap();
        file.write_all(b"owned combined-close transient failure")
            .unwrap();
        file.sync_all().unwrap();
        let metadata = file.metadata().unwrap();
        *owned_receipt.lock().unwrap() = Some((metadata.dev(), metadata.ino()));
        identity.send((metadata.dev(), metadata.ino())).unwrap();
    });
    let error = process(
        &fixture.host,
        Authority::default(),
        Operation::ConfirmPresentationClose { observed: target },
    )
    .unwrap_err();
    assert!(!error.is_empty());
    let (device, inode) = original.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        fixture.owner.root.is_current(),
        "uncertain local teardown must retain the exact native server"
    );
    assert!(!fixture.pair.is_completed());
    let HostedSession::Chat(source) = fixture.agent.session.as_ref().unwrap() else {
        unreachable!()
    };
    assert!(
        fixture
            .host
            .lock()
            .unwrap()
            .native_closing
            .contains_key(&source.id)
    );
    assert!(
        fixture.host.lock().unwrap().chats[&source.id]
            .handle
            .same_actor(&fixture.handle)
    );
    assert_eq!(
        store.lock().unwrap().agent(fixture.agent.id).unwrap(),
        fixture.agent
    );
    assert_eq!(
        store.lock().unwrap().agent_run(fixture.run.id).unwrap(),
        fixture.run
    );
    assert_eq!(store.lock().unwrap().model().unwrap(), before);
    assert_eq!(fixture.rpc_counts(), counts);
    // This is reconciliation uncertainty, not an assertion that the local
    // reducer is untouched/usable. Remove only our exact malformed inode.
    let metadata = std::fs::symlink_metadata(&path).unwrap();
    assert!(!metadata.file_type().is_symlink());
    assert_eq!((metadata.dev(), metadata.ino()), (device, inode));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"owned combined-close transient failure"
    );
    std::fs::remove_file(&path).unwrap();
    *fixture.malformed_receipt.lock().unwrap() = None;
    let fresh = fixture.inspect();
    let Reply::Closed(closed) = process(
        &fixture.host,
        Authority::default(),
        Operation::ConfirmPresentationClose { observed: fresh },
    )
    .unwrap() else {
        panic!("not closed")
    };
    assert!(closed.closed);
    assert!(!fixture.owner.root.is_current());
    assert_eq!(fixture.rpc_counts(), counts);
    assert_eq!(
        store.lock().unwrap().agent_run(fixture.run.id).unwrap(),
        fixture.run
    );
    assert!(
        store
            .lock()
            .unwrap()
            .inspect_presentation_repair(&Origin::User, fixture.agent.id)
            .unwrap()
            .is_none()
    );
}

// Real Server-only cleanup/receipt contract. The sleep owner is deliberately
// not claimed as Codex decoder or native UI qualification.
#[test]
fn combined_close_server_cold_retry_uses_saved_owner_after_lease_rotation() {
    if owning_close_isolated(
        "combined_close_server_cold_retry_uses_saved_owner_after_lease_rotation",
    )
    .is_some()
    {
        return;
    }
    use crate::daemon::holder_registry::{self as holders, Kind};
    use chat::protocol::{ExecutionContext, ProcessSpec, TransportSpec};
    let (host, mut request, path) = fixture();
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.provider = Provider::Codex;
    let store = registry(&host).unwrap();
    let accepted = store
        .lock()
        .unwrap()
        .submit_agent(&Origin::User, &request, now())
        .unwrap();
    let current_proof = host.lock().unwrap().proof().clone();
    let mut original_proof = current_proof.clone();
    original_proof.host_incarnation = HostIncarnation::new("before-holder-adoption");
    let directory = path.parent().unwrap();
    let id = TerminalId::new();
    let mut original = chat::fresh_session(
        Provider::Codex,
        id,
        directory.to_owned(),
        &Account::system(Provider::Codex),
    )
    .unwrap();
    original.proof = Some(original_proof.clone());
    original.claim_owned_codex_listener(&directory.join("tmp"), &original_proof);
    let socket = original
        .owned_codex_listener
        .as_ref()
        .unwrap()
        .socket
        .clone();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let lease = crate::daemon::HostLease::acquire(&paths, &original_proof).unwrap();
    let context = Arc::new(ExecutionContext::new(Some(Arc::new(
        std::fs::File::open(directory).unwrap(),
    ))));
    let process = ProcessSpec {
        execution: Some(context.clone()),
        messages: None,
        transport: TransportSpec::OwnedUnixWebSocket { socket },
        program: "/bin/sleep".into(),
        arguments: vec!["60".into()],
        working_directory: directory.to_owned(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let mut holder = chat::native_holder::spawn(process, original.clone()).unwrap();
    context.installed();
    let owner = holders::evidence(
        &holders::socket(&holders::directory(Kind::Native).unwrap(), id),
        Kind::Native,
    )
    .unwrap()
    .unwrap();
    holder
        .confirm(
            uuid::Uuid::new_v4().to_string(),
            chat::protocol::ServerIdentity {
                user_agent: "cagents/0.157.1 (Linux)".into(),
                canonical_home: directory.to_owned(),
                reported_home: directory.display().to_string(),
            },
            owner.root.clone(),
        )
        .unwrap();
    holder.preserve();
    store
        .lock()
        .unwrap()
        .bind_agent(
            accepted.agent.id,
            None,
            HostedSession::Chat(original.clone()),
        )
        .unwrap();
    drop(lease);
    let _lease = crate::daemon::HostLease::acquire(&paths, &current_proof).unwrap();
    let (adopted, recovery) =
        chat::native_holder::adopt(&owner, &original, &current_proof).unwrap();
    let detached = adopted.preserve();
    assert_ne!(recovery.source.proof, original.proof);
    assert!(recovery.previous_sources.contains(&original));
    let current = HostedSession::Chat(recovery.source.clone());
    store
        .lock()
        .unwrap()
        .adopt_authenticated_agent_session(
            &recovery
                .previous_sources
                .iter()
                .cloned()
                .map(HostedSession::Chat)
                .collect::<Vec<_>>(),
            &current,
        )
        .unwrap();
    let (agent, scope, pane) = {
        let mut store = store.lock().unwrap();
        let mut recipe = recovery.source.launch.recipe.clone();
        recipe.program = "/bin/cat".into();
        recipe.arguments.clear();
        let terminal = crate::model::TerminalSession {
            id,
            launch: recipe,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::Exited {
                last_exit: crate::model::TerminalExit {
                    code: Some(0),
                    signal: None,
                    occurred_at_unix_ms: 1,
                },
            },
        };
        let agent = store
            .bind_agent_console(accepted.agent.id, &current, terminal.clone())
            .unwrap();
        let scope = agent.placement.clone();
        let pane = crate::workspace_model::views::Pane {
            id: crate::model::CardId::from_uuid(agent.id.as_uuid()),
            title: "Legacy adopted console".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 1.0,
            kind: crate::model::CardKind::Terminal(terminal),
            scope: None,
        };
        store
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![pane.clone()],
                closed: vec![],
            })
            .unwrap();
        (agent, scope, pane)
    };

    assert!(
        !host.lock().unwrap().chats.contains_key(&id),
        "this must take NativeOwner::Server, not Actor"
    );
    let before_agent = store.lock().unwrap().agent(agent.id).unwrap();
    let before_run = store.lock().unwrap().agent_run(accepted.run.id).unwrap();
    let Reply::PresentationClose(Some(target)) = process(
        &host,
        Authority::default(),
        Operation::InspectPresentationClose(agent.id),
    )
    .unwrap() else {
        panic!("missing server target")
    };
    assert_eq!(target.actor_instance, None);
    let database = path.clone();
    crate::host::chat::before_presentation_close_commit(move || {
        let db = rusqlite::Connection::open(database).unwrap();
        db.execute("UPDATE shared_views SET revision=revision+1", [])
            .unwrap();
    });
    let error = process(
        &host,
        Authority::default(),
        Operation::ConfirmPresentationClose { observed: target },
    )
    .unwrap_err();
    assert!(error.contains("view revision changed"), "{error}");
    assert!(!owner.root.is_current());
    assert!(!owner.holder.is_current());
    assert!(host.lock().unwrap().native_closing.contains_key(&id));
    assert_eq!(store.lock().unwrap().agent(agent.id).unwrap(), before_agent);
    assert_eq!(
        store.lock().unwrap().agent_run(accepted.run.id).unwrap(),
        before_run
    );
    let Reply::PresentationClose(Some(old_target)) = process(
        &host,
        Authority::default(),
        Operation::InspectPresentationClose(agent.id),
    )
    .unwrap() else {
        panic!("missing proved server target")
    };
    let receipt_path = holders::directory(Kind::Native).unwrap().join(format!(
        "{}-{}-{}.closed",
        id, owner.holder.pid, owner.holder.start
    ));
    let receipt_bytes = std::fs::read(&receipt_path).unwrap();
    assert_eq!(
        chat::native_holder::closed_source_proof(&recovery.source, &current_proof).unwrap(),
        Some(current_proof.clone())
    );
    // Drop only the old daemon lease; the fresh HostState below has no RAM
    // pending owner. Genuine durable exact receipt remains the only authority.
    drop(_lease);
    let mut next_proof = current_proof.clone();
    next_proof.host_incarnation = HostIncarnation::new("server-cold-retry");
    let _next_lease = crate::daemon::HostLease::acquire(&paths, &next_proof).unwrap();
    let mut state = HostState::new(next_proof.clone());
    state.install_model(Registry::at(&path).unwrap());
    let next = Arc::new(crate::host::state::HostLock::new(state));
    assert!(
        process(
            &next,
            Authority::default(),
            Operation::ConfirmPresentationClose {
                observed: old_target
            }
        )
        .is_err()
    );
    assert_eq!(store.lock().unwrap().agent(agent.id).unwrap(), before_agent);
    assert!(next.lock().unwrap().native_closing.is_empty());
    // Temporarily move the genuine receipt out of discovery. Absence alone
    // must refuse, and restoring its exact inode is not proof fabrication.
    let hidden = receipt_path.with_extension("owned-test-backup");
    assert!(!hidden.exists());
    std::fs::rename(&receipt_path, &hidden).unwrap();
    let missing = process(
        &next,
        Authority::default(),
        Operation::InspectPresentationClose(agent.id),
    );
    std::fs::rename(&hidden, &receipt_path).unwrap();
    assert!(
        missing.is_err(),
        "no owner process and no receipt cannot grant teardown authority"
    );
    assert_eq!(std::fs::read(&receipt_path).unwrap(), receipt_bytes);
    assert_eq!(
        chat::native_holder::closed_source_proof(&recovery.source, &next_proof).unwrap(),
        Some(current_proof.clone())
    );
    let Reply::PresentationClose(Some(fresh)) = process(
        &next,
        Authority::default(),
        Operation::InspectPresentationClose(agent.id),
    )
    .unwrap() else {
        panic!("missing cold server target")
    };
    assert_eq!(
        fresh.backend,
        crate::agent_execution::PresentationCloseBackend::RetiredOwner
    );
    assert_eq!(fresh.actor_instance, None);
    assert_eq!(fresh.observation.pane, pane);
    assert_eq!(fresh.observation.agent, before_agent);
    assert_eq!(fresh.host, next_proof);
    let provider_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = provider_calls.clone();
    crate::host::chat::before_presentation_provider_close(move || {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let Reply::Closed(closed) = process(
        &next,
        Authority::default(),
        Operation::ConfirmPresentationClose { observed: fresh },
    )
    .unwrap() else {
        panic!("not closed")
    };
    assert!(closed.closed);
    let after_run = store.lock().unwrap().agent_run(accepted.run.id).unwrap();
    assert_eq!(after_run.state, State::Cancelled);
    assert_eq!(after_run.result, before_run.result);
    assert!(
        store
            .lock()
            .unwrap()
            .inspect_presentation_repair(&Origin::User, agent.id)
            .unwrap()
            .is_none()
    );
    assert!(next.lock().unwrap().native_closing.is_empty());
    assert_eq!(
        provider_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "cold receipt-only retry must not invoke the actual provider-close site"
    );
    assert_eq!(
        std::fs::read(&receipt_path).unwrap(),
        receipt_bytes,
        "fresh lease must never rewrite the original saved proof"
    );
    assert_eq!(
        close(&next, &Authority::default(), agent.id).unwrap(),
        closed
    );
    drop(detached); // No provider call; already-proved cleanup was consumed.
}

#[test]
fn combined_close_local_admission_avoids_held_presentation_store_and_host_census() {
    if owning_close_isolated(
        "combined_close_local_admission_avoids_held_presentation_store_and_host_census",
    )
    .is_some()
    {
        return;
    }
    let fixture = LocalCodexPresentationFixture::start();
    let target = fixture.inspect();
    let (held, holding) = std::sync::mpsc::channel();
    let (release_store, released) = std::sync::mpsc::channel();
    let handle = fixture.handle.clone();
    let owner = std::thread::spawn(move || {
        handle.with_presentation_store_locked(|| {
            held.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(10)).unwrap();
        })
    });
    holding.recv_timeout(Duration::from_secs(1)).unwrap();
    let (admitted, admission) = std::sync::mpsc::channel();
    let (release_close, released_close) = std::sync::mpsc::channel();
    crate::host::chat::before_presentation_provider_close(move || {
        admitted.send(()).unwrap();
        released_close
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
    });
    let host = fixture.host.clone();
    let close = std::thread::spawn(move || {
        process(
            &host,
            Authority::default(),
            Operation::ConfirmPresentationClose { observed: target },
        )
    });
    // The destructive-call gate is after identity admission and outside
    // HostState. It must be reachable while actual Store is still held.
    let admitted = admission.recv_timeout(Duration::from_secs(2));
    let (census_done, census) = std::sync::mpsc::channel();
    let host = fixture.host.clone();
    let census_owner = std::thread::spawn(move || {
        census_done.send(host.lock().unwrap().chats.len()).unwrap();
    });
    let responsive = census.recv_timeout(Duration::from_millis(500));
    // Always release both bounded latches before joins/assertions. Even a
    // regression keeps the original test child under the parent deadline.
    release_store.send(()).unwrap();
    let _ = release_close.send(());
    owner.join().unwrap();
    census_owner.join().unwrap();
    let outcome = close.join().unwrap();
    assert!(
        admitted.is_ok(),
        "close admission hydrated/locked presentation Store under HostState"
    );
    assert!(
        responsive.is_ok(),
        "unrelated host census waited on presentation Store"
    );
    let Reply::Closed(closed) = outcome.unwrap() else {
        panic!("not closed")
    };
    assert!(closed.closed);
    let store = registry(&fixture.host).unwrap();
    assert_eq!(
        store.lock().unwrap().agent_run(fixture.run.id).unwrap(),
        fixture.run
    );
    assert!(
        store
            .lock()
            .unwrap()
            .inspect_presentation_repair(&Origin::User, fixture.agent.id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn combined_close_retained_local_owner_refuses_degraded_managed_registry_before_adoption() {
    if owning_close_isolated(
        "combined_close_retained_local_owner_refuses_degraded_managed_registry_before_adoption",
    )
    .is_some()
    {
        return;
    }
    let fixture = LocalCodexPresentationFixture::start();
    let store = registry(&fixture.host).unwrap();
    let before_model = fixture.host.lock().unwrap().read_model().unwrap();
    let before_run = store.lock().unwrap().agent_run(fixture.run.id).unwrap();
    let counts = fixture.rpc_counts();
    let HostedSession::Chat(source) = fixture.agent.session.as_ref().unwrap() else {
        unreachable!()
    };
    let before_holder = chat::native_holder::inspect(&fixture.owner, &fixture.proof)
        .unwrap()
        .0;
    struct Restore {
        host: SharedHost,
        id: TerminalId,
        registry: Option<SharedRegistry>,
        record: Option<crate::host::chat::Record>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            let mut state = self.host.lock().unwrap();
            if state.execution_registry.is_none() {
                state.execution_registry = self.registry.take();
            }
            if !state.chats.contains_key(&self.id)
                && let Some(record) = self.record.take()
            {
                state.chats.insert(self.id, record);
            }
            // Any unexpected replacement stays intact; retired handles owned
            // by this guard are dropped only after the HostState lock.
            drop(state);
        }
    }
    let restore = {
        let mut state = fixture.host.lock().unwrap();
        let registry = state
            .execution_registry
            .take()
            .expect("actual managed connection");
        let record = state
            .chats
            .remove(&source.id)
            .expect("actual Local actor record");
        Restore {
            host: fixture.host.clone(),
            id: source.id,
            registry: Some(registry),
            record: Some(record),
        }
    };
    // The durable sidebar model is still real and installed. Only its second
    // execution connection has become unavailable; this is not registry-less
    // ordinary interactive mode. Keep the exact Local handle alive separately.
    let installed = fixture.host.lock().unwrap().read_model();
    let started = Instant::now();
    let preflight = preflight_retirement(&fixture.host, source);
    let retired = retired(&fixture.host, source);
    let close = crate::host::chat::close_retained_native(&fixture.host, source);
    let elapsed = started.elapsed();
    let pending = fixture
        .host
        .lock()
        .unwrap()
        .native_closing
        .contains_key(&source.id);
    let after_holder = chat::native_holder::inspect(&fixture.owner, &fixture.proof);
    let after_model = fixture.host.lock().unwrap().read_model();
    drop(restore); // Restore the original connection/record before cleanup/assertions.
    assert!(installed.is_ok());
    for result in [preflight, retired, close] {
        let error = result.unwrap_err();
        assert!(
            error.contains("managed execution registry is unavailable"),
            "{error}"
        );
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "unavailable execution store must refuse before provider cleanup waits"
    );
    assert!(
        !pending,
        "refusal must precede installing/adopting any cleanup owner"
    );
    assert!(fixture.owner.root.is_current());
    assert!(fixture.owner.holder.is_current());
    assert!(!fixture.pair.is_completed());
    assert!(fixture.handle.provider_process().is_none());
    let (after_holder, exited) = after_holder.unwrap();
    assert!(!exited);
    assert_eq!(after_holder.source, before_holder.source);
    assert_eq!(
        after_holder.previous_sources,
        before_holder.previous_sources
    );
    assert_eq!(after_holder.server_process, before_holder.server_process);
    assert_eq!(after_holder.paired_holder, before_holder.paired_holder);
    assert_eq!(after_model.unwrap(), before_model);
    assert_eq!(
        store.lock().unwrap().agent(fixture.agent.id).unwrap(),
        fixture.agent
    );
    assert_eq!(
        store.lock().unwrap().agent_run(fixture.run.id).unwrap(),
        before_run
    );
    assert!(Arc::ptr_eq(&registry(&fixture.host).unwrap(), &store));
    assert_eq!(fixture.rpc_counts(), counts);
}

#[test]
fn combined_close_local_busy_presentation_identity_refuses_without_blocking_host() {
    if owning_close_isolated(
        "combined_close_local_busy_presentation_identity_refuses_without_blocking_host",
    )
    .is_some()
    {
        return;
    }
    let fixture = LocalCodexPresentationFixture::start();
    let target = fixture.inspect();
    let store = registry(&fixture.host).unwrap();
    let before = store.lock().unwrap().model().unwrap();
    let counts = fixture.rpc_counts();
    let (held, holding) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let handle = fixture.handle.clone();
    let owner = std::thread::spawn(move || {
        handle.with_presentation_state_locked(|| {
            held.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(10)).unwrap();
        })
    });
    holding.recv_timeout(Duration::from_secs(1)).unwrap();
    let host = fixture.host.clone();
    let (done, completion) = std::sync::mpsc::channel();
    let closing = std::thread::spawn(move || {
        done.send(process(
            &host,
            Authority::default(),
            Operation::ConfirmPresentationClose { observed: target },
        ))
        .unwrap();
    });
    let outcome = completion.recv_timeout(Duration::from_secs(2));
    let (sent, census) = std::sync::mpsc::channel();
    let host = fixture.host.clone();
    let reader = std::thread::spawn(move || {
        sent.send(host.lock().unwrap().chats.len()).unwrap();
    });
    let responsive = census.recv_timeout(Duration::from_millis(500));
    release.send(()).unwrap();
    owner.join().unwrap();
    reader.join().unwrap();
    closing.join().unwrap();
    let error = outcome
        .expect("presentation state contention must return instead of waiting under HostState")
        .unwrap_err();
    assert!(error.contains("metadata is busy or unavailable"), "{error}");
    assert!(responsive.is_ok());
    assert!(fixture.owner.root.is_current());
    assert!(!fixture.pair.is_completed());
    assert_eq!(store.lock().unwrap().model().unwrap(), before);
    assert_eq!(
        store.lock().unwrap().agent(fixture.agent.id).unwrap(),
        fixture.agent
    );
    assert_eq!(
        store.lock().unwrap().agent_run(fixture.run.id).unwrap(),
        fixture.run
    );
    assert_eq!(fixture.rpc_counts(), counts);
    // A new explicit inspection after contention has ended still names the
    // same exact owner; the busy refusal did not mint/consume close authority.
    let fresh = fixture.inspect();
    let Reply::Closed(closed) = process(
        &fixture.host,
        Authority::default(),
        Operation::ConfirmPresentationClose { observed: fresh },
    )
    .unwrap() else {
        panic!("not closed")
    };
    assert!(closed.closed);
    assert_eq!(
        store.lock().unwrap().agent_run(fixture.run.id).unwrap(),
        fixture.run
    );
    assert_eq!(fixture.rpc_counts(), counts);
}
