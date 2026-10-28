//! Real helper qualification of the historical Pending -> retired-history join.
//! Only the previous-boot Agent/run are typed fixture data. Installation ACK,
//! provider identity, closed publication and private retirement are real.
use super::*;
use crate::{
    agent_execution::{Destination, HostedSession, Origin, Spec, State, Submit, Surface, Target},
    host::model::Registry,
    process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
};
use std::{
    fs,
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::Path,
};

/// Containment uses an original-process pidfd, never an unpinned numeric kill.
/// This proves only fixture execution exit, not semantic actor retirement.
fn contain_process(process: &crate::daemon::holder_registry::Process) -> Result<(), String> {
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, process.pid, 0) } as i32;
    if raw < 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error.to_string())
        };
    }
    let original = unsafe { OwnedFd::from_raw_fd(raw) };
    match crate::daemon::holder_registry::Process::read(process.pid) {
        Ok(actual) if actual == *process => {}
        Ok(_) => return Ok(()), // Original exited; a reused PID is not ours.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    }
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            original.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } < 0
    {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error.to_string());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut poll = libc::pollfd {
            fd: original.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Owned fixture process did not exit after containment".into());
        }
        let result = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().min(100) as i32) };
        if result > 0 && poll.revents & libc::POLLIN != 0 {
            return Ok(());
        }
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
}

fn contain_record(record: &crate::daemon::holder_registry::Record) -> Result<(), String> {
    let root = contain_process(&record.root);
    let holder = contain_process(&record.holder);
    root.and(holder)
}

struct OwnedHelperGuard(crate::daemon::holder_registry::Record);
impl OwnedHelperGuard {
    fn capture(root: &Path, handle: &Handle) -> Self {
        let identity = handle
            .actor_identity()
            .expect("real helper identity required");
        let guard = Self(identity.record);
        // Persist actual custody for the parent before any installed-helper
        // assertion can panic. This file is not a retirement/authority proof.
        let directory = root.join("owned-helpers");
        fs::create_dir_all(&directory).unwrap();
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join(format!("{}.json", identity.instance)))
            .unwrap();
        serde_json::to_writer(file, &guard.0).unwrap();
        guard
    }
}
impl Drop for OwnedHelperGuard {
    fn drop(&mut self) {
        if let Err(error) = contain_record(&self.0) {
            eprintln!("fixture containment remains unproved: {error}");
        }
    }
}

fn contain_directory(directory: &Path) -> Result<(), String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let mut first_error = None;
    for entry in entries {
        let result = (|| {
            let entry = entry.map_err(|error| error.to_string())?;
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            if kind.is_dir() {
                contain_directory(&entry.path())?;
            } else if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "json") {
                let file = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(entry.path())
                    .map_err(|error| error.to_string())?;
                let metadata = file.metadata().map_err(|error| error.to_string())?;
                if !metadata.is_file()
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.mode() & 0o077 != 0
                    || metadata.nlink() != 1
                    || metadata.len() > 4096
                {
                    return Err("Unsafe fixture custody record".into());
                }
                let mut bytes = Vec::new();
                file.take(4097)
                    .read_to_end(&mut bytes)
                    .map_err(|error| error.to_string())?;
                let Ok(record) =
                    serde_json::from_slice::<crate::daemon::holder_registry::Record>(&bytes)
                else {
                    return Ok(());
                };
                if record.kind != crate::daemon::holder_registry::Kind::Native
                    || record.application != crate::instance::APPLICATION_ID
                    || record.boot
                        != crate::daemon::read_boot_id().map_err(|error| error.to_string())?
                {
                    return Err("Fixture custody changed its application or boot".into());
                }
                contain_record(&record)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

struct DirectoryGuard(std::path::PathBuf);
impl DirectoryGuard {
    fn contain(&self) -> Result<(), String> {
        // The parent allocated this fresh private root. Only its own real
        // native discoveries and captured owner records are inspected.
        let discoveries = contain_directory(&self.0.join("run"));
        let captured = contain_directory(&self.0.join("owned-helpers"));
        discoveries.and(captured)
    }
}
impl Drop for DirectoryGuard {
    fn drop(&mut self) {
        if let Err(error) = self.contain() {
            eprintln!("fixture directory containment remains unproved: {error}");
        }
    }
}

fn isolated() -> Option<std::path::PathBuf> {
    const CHILD: &str = "CAGENTS_ARCHIVED_HISTORY_RUNTIME_TEST";
    let test = thread::current().name().unwrap().to_owned();
    if std::env::var(CHILD).as_deref() == Ok(test.as_str()) {
        return Some(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap().into());
    }
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    for child in [
        "home/.claude",
        "run",
        "data",
        "config",
        "cache",
        "bin",
        "project",
        "tmp",
    ] {
        fs::create_dir_all(root.path().join(child)).unwrap();
    }
    let mut command = CommandSpec::new(std::env::current_exe().unwrap());
    // Preserve the caller's opaque delegation and actual matched helper. This
    // child owns only its fresh HOME/runtime/data and never becomes User.
    for (key, _) in std::env::vars_os() {
        if key != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV)
            && key != std::ffi::OsStr::new("CAGENTS_TEST_RETAINED_ACTOR_HELPER")
        {
            command = command.env_remove(key);
        }
    }
    let helper = super::super::retained_actor_tests::retained_helper_executable();
    let guardian = DirectoryGuard(root.path().into());
    let command = command
        .args(["--exact", &test, "--nocapture", "--test-threads=1"])
        .cwd(root.path())
        .env(CHILD, &test)
        .env("CAGENTS_TEST_RETAINED_ACTOR_HELPER", helper)
        .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
        .env("CAGENTS_HOLDER_ROOT", root.path())
        .env("HOME", root.path().join("home"))
        .env("CLAUDE_CONFIG_DIR", root.path().join("home/.claude"))
        .env("XDG_RUNTIME_DIR", root.path().join("run"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("TMPDIR", root.path().join("tmp"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.path().join("bin").display()),
        )
        .timeout(Duration::from_secs(90))
        .output_limits(256 * 1024, 256 * 1024);
    let output = SystemProcessExecutor.execute(&command, &CancellationToken::new());
    let containment_probe = root.path().join("parent-containment-required.json");
    let detached = containment_probe.exists().then(|| {
        let record: crate::daemon::holder_registry::Record =
            serde_json::from_slice(&fs::read(containment_probe).unwrap()).unwrap();
        [record.root, record.holder].map(|process| {
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, process.pid, 0) } as i32;
            assert!(
                raw >= 0,
                "detached fixture process disappeared before its parent took custody"
            );
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            assert_eq!(
                crate::daemon::holder_registry::Process::read(process.pid).unwrap(),
                process
            );
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(
                unsafe { libc::poll(&mut poll, 1, 0) },
                0,
                "fixture must leave actual execution for parent containment"
            );
            fd
        })
    });
    let contained = guardian.contain();
    let retained = root.keep();
    let (stdout, stderr) = match &output {
        Ok(output) => (&output.stdout, &output.stderr),
        Err(error) => (&error.stdout, &error.stderr),
    };
    fs::write(retained.join("test.stdout"), stdout).unwrap();
    fs::write(retained.join("test.stderr"), stderr).unwrap();
    contained.expect("every escaped fixture helper/provider must have observed execution exit");
    if let Some(detached) = detached {
        for fd in detached {
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert!(
                unsafe { libc::poll(&mut poll, 1, 0) } > 0 && poll.revents & libc::POLLIN != 0,
                "parent did not observe exact detached fixture execution exit"
            );
        }
    }
    let output = output.expect("bounded archived-history child must settle");
    assert!(
        output.success,
        "archived-history fixture {}:\n{}\n{}",
        retained.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("archived-history fixture evidence: {}", retained.display());
    None
}

#[test]
fn authenticated_archive_parent_contains_helpers_after_abrupt_child_exit() {
    let Some(root) = isolated() else { return };
    let fixture = Fixture::new(root);
    // This actual helper is installed and detached from the executor's process
    // group. Exit without Rust destructors to exercise parent-owned custody.
    // The record is custody only and is never consumed as retirement proof.
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(fixture.root.join("parent-containment-required.json"))
        .unwrap();
    serde_json::to_writer(file, &fixture.identity.record).unwrap();
    std::process::exit(0);
}

fn wait(mut predicate: impl FnMut() -> bool, reason: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "{reason}");
        let ready = predicate();
        assert!(Instant::now() < deadline, "{reason}");
        if ready {
            return;
        }
        thread::sleep(
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

macro_rules! inspect {
    ($fixture:expr) => {{
        let proof = $fixture.host.lock().unwrap().proof().clone();
        Handle::inspect_remote(&$fixture.identity.record, proof).unwrap()
    }};
}

struct Fixture {
    root: std::path::PathBuf,
    host: SharedHost,
    _lease: crate::daemon::HostLease,
    handle: Handle,
    identity: chat::runtime::ActorIdentity,
    source: Session,
    historical: crate::agent_execution::Agent,
    run: crate::agent_execution::Run,
    _context: Arc<chat::protocol::ExecutionContext>,
    _cleanup: OwnedHelperGuard,
}

impl Fixture {
    fn new(root: std::path::PathBuf) -> Self {
        Self::with_account_and_result(
            root,
            &crate::account::Account::system(Provider::Claude),
            State::Succeeded,
        )
    }

    fn with_account_and_result(
        root: std::path::PathBuf,
        account: &crate::account::Account,
        result: State,
    ) -> Self {
        assert!(matches!(result, State::Succeeded | State::Failed));
        install_provider(&root.join("bin/claude"));
        let proof = HostSessionProof {
            host_session_id: "archived-history-runtime".into(),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: "actual-current-owner".into(),
        };
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        let mut state = HostState::new(proof.clone());
        state.register_client(17);
        let database = if result == State::Failed {
            "failed-registry.sqlite3"
        } else {
            "registry.sqlite3"
        };
        state.install_model(Registry::at(&root.join(database)).unwrap());
        let host = Arc::new(HostLock::new(state));
        let mut source = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            root.join("project"),
            account,
        )
        .unwrap();
        source.proof = Some(proof.clone());
        assert_eq!(
            source.native_continuity,
            chat::NativeContinuity::ReconnectOnly
        );
        let mut historical = source.clone();
        historical.proof.as_mut().unwrap().boot_id = "synthetic-previous-boot-fixture".into();
        let registry = super::super::super::agent_execution::registry(&host).unwrap();
        let (agent, run) = {
            let mut store = registry.lock().unwrap();
            store
                .apply(&crate::workspace_model::ModelEdit::OpenProject {
                    repository: root.join("project"),
                    name: "Archived history runtime".into(),
                    kind: crate::model::ProjectKind::Folder,
                })
                .unwrap();
            let accepted = store
                .submit_agent(
                    &Origin::User,
                    &Submit {
                        name: None,
                        destination: Destination::New(Spec {
                            provider: Provider::Claude,
                            account: account.registry_key().map(|_| account.id()),
                            target: Target {
                                repository: root.join("project"),
                                directory: root.join("project"),
                                placement: None,
                            },
                            surface: Surface::Chat,
                            model: None,
                            permission_mode: None,
                            reasoning_effort: None,
                        }),
                        prompt: "Historical fixture work; never dispatch this request".into(),
                        idempotency_key: "historical-runtime-join".into(),
                    },
                    1,
                )
                .unwrap();
            store
                .bind_agent(
                    accepted.agent.id,
                    None,
                    HostedSession::Chat(historical.clone()),
                )
                .unwrap();
            store
                .fresh_agent_authority(&HostedSession::Chat(historical))
                .unwrap();
            let started = store
                .update_agent_run(
                    accepted.run.id,
                    accepted.run.revision,
                    crate::agent_execution::Update::Started {
                        provider_key: "synthetic-previous-boot-result".into(),
                    },
                    2,
                )
                .unwrap();
            let run = store
                .update_agent_run(
                    started.id,
                    started.revision,
                    crate::agent_execution::Update::Finished {
                        state: result,
                        result: crate::agent_execution::ResultSummary {
                            text: "Preserve historical result".into(),
                            ..Default::default()
                        },
                    },
                    3,
                )
                .unwrap();
            (store.agent(accepted.agent.id).unwrap(), run)
        };
        let context =
            Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap());
        let (mut spec, adapter) = chat::protocol::create(&source.launch).unwrap();
        spec.execution = Some(context.clone());
        let prepared = adapter.prepared_launch_identity().unwrap();
        let environment = spec.environment.clone();
        let handle =
            Handle::start_remote_owned(spec, source.clone(), prepared, environment, |_| {})
                .unwrap();
        let cleanup = OwnedHelperGuard::capture(&root, &handle);
        let identity = handle.actor_identity().unwrap();
        wait(
            || handle.attachment_snapshot().ready && handle.confirmed_session_id().is_some(),
            "real provider did not confirm its initialized conversation",
        );
        source.launch.session_id = handle.confirmed_session_id();
        // The old immutable canonical row remains unnamed. The helper must
        // independently issue this confirmed ReconnectOnly cleanup descriptor
        // before enrollment; no guessed UUID or alias is admitted.
        Handle::inspect_remote(&identity.record, proof.clone()).unwrap();
        handle
            .update_native_source(source.clone(), None, None)
            .unwrap();
        let inspection = Handle::inspect_remote(&identity.record, proof.clone()).unwrap();
        assert!(!inspection.state.installed);
        enroll_pending(
            &host,
            &identity,
            &inspection.source,
            &inspection.previous_sources,
            inspection
                .launch_account_scope
                .as_ref()
                .map_err(String::as_str),
            false,
        )
        .unwrap();
        handle.commit_native_installation().unwrap();
        context.installed();
        let installed = Handle::inspect_remote(&identity.record, proof).unwrap();
        assert!(installed.state.installed);
        assert_eq!(installed.state.confirmed_session, source.launch.session_id);
        source.native_continuity = installed.state.native_continuity;
        handle
            .update_native_source(source.clone(), None, None)
            .unwrap();
        // Stop at the real ACK-before-canonical-publication cutpoint. Do not
        // promote Pending or rewrite the synthetic old canonical Agent.
        let ownership = registry
            .lock()
            .unwrap()
            .native_actor_ownership(identity.instance)
            .unwrap()
            .unwrap();
        assert!(ownership.is_pending());
        assert_eq!(
            ownership.source().native_continuity,
            chat::NativeContinuity::ReconnectOnly
        );
        assert_eq!(registry.lock().unwrap().agent(agent.id).unwrap(), agent);
        Self {
            root,
            host,
            _lease: lease,
            handle,
            identity,
            source,
            historical: agent,
            run,
            _context: context,
            _cleanup: cleanup,
        }
    }

    fn registry(&self) -> super::super::super::agent_execution::SharedRegistry {
        super::super::super::agent_execution::registry(&self.host).unwrap()
    }

    fn assert_preserved(&self) {
        let store = self.registry();
        let store = store.lock().unwrap();
        assert_eq!(store.agent(self.historical.id).unwrap(), self.historical);
        assert_eq!(store.agent_run(self.run.id).unwrap(), self.run);
        assert!(
            store
                .native_actor_retirement(self.identity.instance)
                .unwrap()
                .is_none()
        );
        assert!(self.host.lock().unwrap().chats.is_empty());
        assert!(
            !self.root.join("prompts").exists(),
            "recovery injected provider input"
        );
    }
}

#[test]
fn authenticated_legacy_archive_joins_retirement_and_prepares_explicit_resume_without_input() {
    let Some(root) = isolated() else { return };
    let fixture = Fixture::new(root);
    fixture.handle.close().unwrap();
    let closed = inspect!(fixture);
    assert!(closed.state.installed && closed.state.close.completed && closed.state.close.exited);
    assert_eq!(closed.state.outbox_ack, closed.state.outbox_latest);
    assert!(closed.state.continuity_loss.is_none());
    assert_eq!(closed.source, fixture.source);
    assert_eq!(
        closed.source.native_continuity,
        chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
    );
    reconcile_previous_boot_history(&fixture.host).unwrap();
    let store = fixture.registry();
    let (ownership, retirement) = {
        let store = store.lock().unwrap();
        let mut archived = fixture.historical.clone();
        archived.closed = true;
        assert_eq!(store.agent(archived.id).unwrap(), archived);
        assert_eq!(store.agent_run(fixture.run.id).unwrap(), fixture.run);
        let ownership = store
            .native_actor_ownership(fixture.identity.instance)
            .unwrap()
            .unwrap();
        let retirement = store
            .native_actor_retirement(fixture.identity.instance)
            .unwrap()
            .unwrap();
        assert!(ownership.is_pending() && retirement.is_pending());
        assert!(
            store
                .archived_history(
                    &fixture.source,
                    &ownership,
                    &retirement,
                    &fixture.host.lock().unwrap().proof().boot_id
                )
                .unwrap()
                .is_some()
        );
        (ownership, retirement)
    };
    let proof = fixture.host.lock().unwrap().proof().clone();
    let RetirementInspection::Verified(verified) =
        Handle::inspect_retained_retirement(&fixture.identity.record, &proof).unwrap()
    else {
        panic!("normal reconciliation produced no authenticated private retirement");
    };
    assert!(verified.receipt().installed);
    assert_eq!(verified.receipt().source, fixture.source);
    assert!(!fixture.identity.record.root.is_current());
    // This executes validate_evidence(..., Some(ArchivedHistory)) with the
    // actual VerifiedRetirement, physical account and current daemon lease.
    assert!(
        super::super::retained_resume::prepare_resume_verified(
            &fixture.host,
            &fixture.source,
            Some(verified)
        )
        .unwrap()
    );
    let (_, mut history) = chat::protocol::create_history(&fixture.source.launch).unwrap();
    let loaded = history.initialize().unwrap();
    assert!(
        loaded
            .events
            .iter()
            .any(|event| matches!(event, chat::Event::History { items, .. } if !items.is_empty()))
    );
    let prepared = super::super::process_inner(
        &fixture.host,
        17,
        ClientRequest::ChatPrepareResume {
            source: fixture.source.clone(),
        },
    )
    .unwrap();
    let ServerResponse::ChatResumePrepared { session } = prepared else {
        panic!("explicit resume was not prepared")
    };
    assert_ne!(session.id, fixture.source.id);
    assert_eq!(session.launch, fixture.source.launch);
    assert_eq!(session.proof, None);
    assert_eq!(
        session.native_continuity,
        chat::NativeContinuity::ReconnectOnly
    );
    assert!(fixture.host.lock().unwrap().chats.is_empty());
    assert_eq!(
        fs::read_to_string(fixture.root.join("launches"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(!fixture.root.join("prompts").exists());
    assert_eq!(
        store
            .lock()
            .unwrap()
            .native_actor_ownership(fixture.identity.instance)
            .unwrap(),
        Some(ownership)
    );
    assert_eq!(
        store
            .lock()
            .unwrap()
            .native_actor_retirement(fixture.identity.instance)
            .unwrap(),
        Some(retirement)
    );
    // Only a separate, explicit Start may create the new writer. It must use
    // the same confirmed conversation and must not replay the old run/prompt.
    let resumed_id = session.id;
    let started = super::super::process_inner(
        &fixture.host,
        17,
        ClientRequest::ChatStart {
            session,
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    )
    .unwrap();
    assert!(matches!(started, ServerResponse::ChatAttached { .. }));
    let resumed = fixture.host.lock().unwrap().chats[&resumed_id]
        .handle
        .clone();
    let _resumed_cleanup = OwnedHelperGuard::capture(&fixture.root, &resumed);
    let resumed_identity = resumed.actor_identity().unwrap();
    wait(
        || {
            resumed.attachment_snapshot().ready
                && resumed.confirmed_session_id() == fixture.source.launch.session_id
        },
        "explicit writer did not confirm the exact archived conversation",
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("launches"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert!(
        !fixture.root.join("prompts").exists(),
        "explicit resume replayed a historical prompt"
    );
    super::super::process_inner(
        &fixture.host,
        17,
        ClientRequest::ChatClose {
            chat_id: resumed_id,
        },
    )
    .unwrap();
    let second_source = store
        .lock()
        .unwrap()
        .native_actor_retirement(resumed_identity.instance)
        .unwrap()
        .unwrap()
        .source()
        .clone();
    let second = super::super::process_inner(
        &fixture.host,
        17,
        ClientRequest::ChatPrepareResume {
            source: second_source.clone(),
        },
    )
    .unwrap();
    let ServerResponse::ChatResumePrepared { session: second } = second else {
        panic!("a retired archived predecessor blocked the second explicit resume")
    };
    assert_ne!(second.id, resumed_id);
    assert_eq!(second.launch, fixture.source.launch);
    assert_eq!(
        fs::read_to_string(fixture.root.join("launches"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert!(!fixture.root.join("prompts").exists());

    let other_home = fixture.root.join("other-physical-home");
    fs::create_dir(&other_home).unwrap();
    let mut changed_scope = second_source.clone();
    changed_scope.launch.recipe.environment.insert(
        "CLAUDE_CONFIG_DIR".into(),
        other_home.to_string_lossy().into_owned(),
    );
    assert!(
        super::super::process_inner(
            &fixture.host,
            17,
            ClientRequest::ChatPrepareResume {
                source: changed_scope,
            }
        )
        .is_err(),
        "archived predecessor permitted a changed physical account"
    );

    // A real additional allocation with only Pending registration must still
    // block the requested B, even though archived predecessor A is fully safe.
    let mut pending_source = fixture.source.clone();
    pending_source.id = TerminalId::new();
    pending_source.native_continuity = chat::NativeContinuity::ReconnectOnly;
    let context =
        Arc::new(chat::protocol::ExecutionContext::capture(&fixture.root.join("project")).unwrap());
    let (mut spec, adapter) = chat::protocol::create(&pending_source.launch).unwrap();
    spec.execution = Some(context);
    let prepared = adapter.prepared_launch_identity().unwrap();
    let environment = spec.environment.clone();
    let pending =
        Handle::start_remote_owned(spec, pending_source, prepared, environment, |_| {}).unwrap();
    let _pending_cleanup = OwnedHelperGuard::capture(&fixture.root, &pending);
    let identity = pending.actor_identity().unwrap();
    let inspection = Handle::inspect_remote(&identity.record, proof.clone()).unwrap();
    enroll_pending(
        &fixture.host,
        &identity,
        &inspection.source,
        &inspection.previous_sources,
        inspection
            .launch_account_scope
            .as_ref()
            .map_err(String::as_str),
        false,
    )
    .unwrap();
    let prepare_again = || {
        super::super::process_inner(
            &fixture.host,
            17,
            ClientRequest::ChatPrepareResume {
                source: second_source.clone(),
            },
        )
    };
    assert!(
        prepare_again().is_err(),
        "row-only Pending became retired predecessor authority"
    );
    fs::write(fixture.root.join("emit-permission"), "emit").unwrap();
    wait(
        || pending.owns_work(),
        "additional Pending helper did not expose real work",
    );
    assert!(
        prepare_again().is_err(),
        "active Pending predecessor was ignored"
    );
    contain_process(&identity.record.holder).unwrap();
    wait(
        || {
            matches!(
                Handle::inspect_retained_retirement(&identity.record, &proof),
                Ok(RetirementInspection::ExitedUnproved)
            )
        },
        "additional helper exit was not classified unproved",
    );
    assert!(
        prepare_again().is_err(),
        "unproved Pending predecessor was ignored"
    );
    assert!(!fixture.root.join("prompts").exists());
}

fn reconciliation_authority_case(root: std::path::PathBuf, replacement: bool) {
    let fixture = Fixture::new(root);
    let store = fixture.registry();
    store
        .lock()
        .unwrap()
        .fresh_agent_authority(&HostedSession::Chat(fixture.source.clone()))
        .unwrap();
    fixture.handle.close().unwrap();
    let inspection = inspect!(fixture);
    assert!(inspection.state.close.completed && inspection.state.close.exited);
    assert_eq!(inspection.state.outbox_ack, inspection.state.outbox_latest);
    let ownership = store
        .lock()
        .unwrap()
        .native_actor_ownership(fixture.identity.instance)
        .unwrap()
        .unwrap();
    let mut issued = inspection.previous_sources;
    issued.push(inspection.source.clone());
    // Reproduce the crash cutpoint with typed APIs and authentic inspected
    // aliases: archival committed, real helper retired, SQL receipt uncommitted.
    assert!(
        store
            .lock()
            .unwrap()
            .archive_previous_boot_execution(
                &ownership,
                &fixture.host.lock().unwrap().proof().boot_id,
                &issued,
                &inspection.source
            )
            .unwrap()
    );
    let mut newer = fixture.source.clone();
    newer
        .launch
        .recipe
        .arguments
        .push("--fixture-newer-owner".into());
    if replacement {
        // Typed negative fixture metadata represents a newer exact capability;
        // it grants this test no provider execution or fabricated helper alias.
        store
            .lock()
            .unwrap()
            .fresh_agent_authority(&HostedSession::Chat(newer.clone()))
            .unwrap();
    }
    fixture.handle.finish_retained_close().unwrap();
    assert!(matches!(
        Handle::inspect_retained_retirement(
            &fixture.identity.record,
            fixture.source.proof.as_ref().unwrap()
        )
        .unwrap(),
        RetirementInspection::Verified(_)
    ));
    assert!(
        store
            .lock()
            .unwrap()
            .native_actor_retirement(fixture.identity.instance)
            .unwrap()
            .is_none()
    );
    reconcile_retired_actors(&fixture.host).unwrap();
    let store = store.lock().unwrap();
    assert!(
        store
            .native_actor_retirement(fixture.identity.instance)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        store
            .retained_agent_authority_source(fixture.source.id)
            .unwrap(),
        replacement.then_some(HostedSession::Chat(newer))
    );
    assert_eq!(store.agent_run(fixture.run.id).unwrap(), fixture.run);
    drop(store);
    let prepared = super::super::retained_resume::prepare_resume(&fixture.host, &fixture.source);
    if replacement {
        assert!(
            prepared.is_err(),
            "old retirement bypassed a newer authority"
        );
    } else {
        assert!(
            prepared.unwrap(),
            "one startup pass did not finish exact authority retirement"
        );
    }
    assert!(!fixture.root.join("prompts").exists());
}

#[test]
fn authenticated_retirement_reconciliation_retires_original_authority_in_one_pass() {
    let Some(root) = isolated() else { return };
    reconciliation_authority_case(root, false);
}

#[test]
fn authenticated_retirement_reconciliation_preserves_a_newer_authority() {
    let Some(root) = isolated() else { return };
    reconciliation_authority_case(root, true);
}

#[test]
fn original_old_boot_visual_restores_named_account_history_then_starts_twice_over_real_wire() {
    let Some(root) = isolated() else { return };
    install_provider(&root.join("bin/claude"));
    let home = root.join("named-account");
    fs::create_dir(&home).unwrap();
    let key = uuid::Uuid::new_v4();
    let account =
        crate::account::Account::linked(Provider::Claude, key, home.canonicalize().unwrap());
    let accounts = crate::account::registry::Store::installation().unwrap();
    accounts
        .update(|registry| {
            registry.register(
                crate::account::registry::Registered {
                    key,
                    provider: Provider::Claude,
                    linked_home: Some(home.clone()),
                    label: Some("Named archived fixture".into()),
                    email: Some("archived-fixture@example.invalid".into()),
                    organization: None,
                    added_at: 1,
                },
                &[],
            )
        })
        .unwrap()
        .1
        .unwrap();
    assert_eq!(
        accounts.load().unwrap().find(key).unwrap().label.as_deref(),
        Some("Named archived fixture")
    );
    // Exercise the ordinary account probe against an offline CLI, rather than
    // assigning a fabricated Ok status or changing inherited delegation.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let search = runtime.block_on(crate::account::probe::search_path());
    assert_eq!(
        crate::account::probe::find_executable("claude", search.as_deref()),
        Some(root.join("bin/claude")),
        "account probe must execute only the private offline provider"
    );
    let probed = runtime.block_on(crate::account::probe::run(account.clone(), None, true));
    let crate::account::probe::Outcome::Answered {
        identity, snapshot, ..
    } = probed
    else {
        panic!("named account fixture did not answer its ordinary probe: {probed:?}")
    };
    assert_eq!(snapshot.status, crate::account::UsageStatus::Ok);
    assert_eq!(
        identity.unwrap().email.as_deref(),
        Some("archived-fixture@example.invalid")
    );
    drop(runtime);
    let launches = || {
        fs::read_to_string(root.join("launches"))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    };
    for outcome in [State::Succeeded, State::Failed] {
        let before = launches();
        let fixture = Fixture::with_account_and_result(root.clone(), &account, outcome);
        assert_eq!(fixture.run.state, outcome);
        let inspection = inspect!(fixture);
        let scope = inspection.launch_account_scope.unwrap();
        assert_eq!(scope.registered_id, Some(account.id()));
        assert_eq!(scope.canonical_home, home);
        let Some(HostedSession::Chat(mut visual)) = fixture.historical.session.clone() else {
            panic!("canonical old-boot Claude source required")
        };
        // The original visual is named but still old-boot ReconnectOnly. It
        // was never helper-issued; canonical remains unnamed and unchanged.
        assert!(visual.launch.session_id.is_none());
        visual.launch.session_id = fixture.source.launch.session_id.clone();
        assert!(visual.is_saved_claude_history());
        assert_ne!(visual.proof, fixture.source.proof);
        assert_eq!(
            visual.native_continuity,
            chat::NativeContinuity::ReconnectOnly
        );
        assert_ne!(visual, inspection.source);
        assert!(!inspection.previous_sources.contains(&visual));
        fixture.handle.close().unwrap();
        reconcile_previous_boot_history(&fixture.host).unwrap();
        let registry = fixture.registry();
        {
            let registry = registry.lock().unwrap();
            let mut archived = fixture.historical.clone();
            archived.closed = true;
            assert_eq!(registry.agent(archived.id).unwrap(), archived);
            assert_eq!(registry.agent_run(fixture.run.id).unwrap(), fixture.run);
            assert!(
                registry
                    .retained_agent_sources(visual.id)
                    .unwrap()
                    .is_empty()
            );
        }
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let host = fixture.host.clone();
        let server = thread::spawn(move || crate::host::client::serve(server, 61, host));
        let reader = chat::client::ClientHandle::fixture_restore(client, visual.clone()).unwrap();
        wait(
            || {
                let snapshot = reader.snapshot();
                !snapshot.history_loading && !snapshot.items.is_empty()
            },
            "original old-boot visual did not load real read-only history",
        );
        let restored = reader.session();
        let mut expected = visual.clone();
        expected.proof = Some(fixture.host.lock().unwrap().proof().clone());
        assert_eq!(restored, expected);
        let witness = reader
            .authenticated_upgrade_recovery()
            .expect("actual client Resolve/Attach witness");
        assert!(witness.validate());
        assert_eq!(witness.source(), &visual);
        assert_eq!(witness.replacement(), &expected);
        let snapshot = reader.snapshot();
        assert!(snapshot.exited && snapshot.read_only_reason.is_some() && snapshot.error.is_none());
        assert_eq!(
            snapshot.native_continuity,
            chat::NativeContinuity::ReconnectOnly
        );
        assert!(snapshot.items.iter().any(|item| matches!(&item.content, chat::ItemContent::Text { text, .. } if text.contains("Saved fixture answer"))));
        assert!(
            fixture.host.lock().unwrap().chats[&visual.id]
                .handle
                .is_read_only_history()
        );
        assert_eq!(
            launches(),
            before + 1,
            "restoring an original visual started a provider"
        );
        assert!(!root.join("prompts").exists());
        drop(reader);
        server.join().unwrap();

        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let host = fixture.host.clone();
        let server = thread::spawn(move || crate::host::client::serve(server, 62, host));
        let mut client = crate::control::ControlClient::from_stream(client).unwrap();
        client.authenticate(Default::default()).unwrap(); // Synthetic fixture User, never production authority.
        let mut requested = restored;
        for start_number in 1..=2 {
            let ServerResponse::ChatResumePrepared { session: prepared } = client
                .fixture_request(ClientRequest::ChatPrepareResume {
                    source: requested.clone(),
                })
                .unwrap()
            else {
                panic!("explicit resume preparation returned no exact source")
            };
            assert_eq!(prepared.launch, visual.launch);
            assert_eq!(
                launches(),
                before + start_number,
                "Prepare started a provider"
            );
            let id = prepared.id;
            let started = client
                .fixture_request(ClientRequest::ChatStart {
                    session: prepared,
                    source: None,
                    terminate_source: false,
                    execution_environment: Default::default(),
                })
                .unwrap();
            let writer = fixture.host.lock().unwrap().chats[&id].handle.clone();
            let _cleanup = OwnedHelperGuard::capture(&root, &writer);
            let identity = writer.actor_identity().unwrap();
            assert!(matches!(started, ServerResponse::ChatAttached { .. }));
            wait(
                || {
                    writer.attachment_snapshot().ready
                        && writer.confirmed_session_id() == visual.launch.session_id
                },
                "explicit writer did not become ready on the original visual conversation",
            );
            assert_eq!(writer.launch_account_scope(), Ok(&scope));
            assert_eq!(launches(), before + start_number + 1);
            assert!(
                !root.join("prompts").exists(),
                "explicit resume submitted or replayed a prompt"
            );
            client
                .fixture_request(ClientRequest::ChatClose { chat_id: id })
                .unwrap();
            requested = registry
                .lock()
                .unwrap()
                .native_actor_retirement(identity.instance)
                .unwrap()
                .unwrap()
                .source()
                .clone();
            let proof = fixture.host.lock().unwrap().proof().clone();
            assert!(matches!(
                Handle::inspect_retained_retirement(&identity.record, &proof).unwrap(),
                RetirementInspection::Verified(_)
            ));
        }
        drop(client);
        server.join().unwrap();
        assert_eq!(
            registry.lock().unwrap().agent_run(fixture.run.id).unwrap(),
            fixture.run
        );
        assert!(fixture.host.lock().unwrap().chats.is_empty());
    }
}

#[test]
fn authenticated_legacy_archive_preserves_active_and_unacknowledged_helpers() {
    let Some(root) = isolated() else { return };
    let fixture = Fixture::new(root);
    fs::write(fixture.root.join("emit-permission"), "emit").unwrap();
    wait(
        || {
            let state = inspect!(fixture).state;
            state.owns_work && state.outbox_latest > state.outbox_ack
        },
        "real permission request did not retain work and an unacknowledged outcome",
    );
    reconcile_previous_boot_history(&fixture.host).unwrap();
    fixture.assert_preserved();
    fixture.handle.close().unwrap();
    let closed = inspect!(fixture);
    assert!(closed.state.close.completed && closed.state.close.exited);
    assert!(closed.state.outbox_latest > closed.state.outbox_ack);
    assert!(
        fixture.handle.finish_retained_close().is_err(),
        "unacknowledged helper retired"
    );
    reconcile_previous_boot_history(&fixture.host).unwrap();
    fixture.assert_preserved();
}

#[test]
fn authenticated_legacy_archive_preserves_an_unproved_helper_exit() {
    let Some(root) = isolated() else { return };
    let fixture = Fixture::new(root);
    assert!(fixture.identity.record.holder.is_current());
    contain_process(&fixture.identity.record.holder).unwrap();
    let proof = fixture.host.lock().unwrap().proof().clone();
    wait(
        || {
            matches!(
                Handle::inspect_retained_retirement(&fixture.identity.record, &proof),
                Ok(RetirementInspection::ExitedUnproved)
            )
        },
        "killed helper did not expose its unproved retirement",
    );
    reconcile_previous_boot_history(&fixture.host).unwrap();
    fixture.assert_preserved();
}

#[test]
fn authenticated_legacy_archive_preserves_actual_helper_continuity_loss() {
    let Some(root) = isolated() else { return };
    let fixture = Fixture::new(root);
    // The ordinary provider protocol generates enough distinct waiting events
    // to exceed the production journal entry bound. No journal/DB is edited.
    fs::write(fixture.root.join("emit-loss"), "emit").unwrap();
    wait(
        || inspect!(fixture).state.continuity_loss.is_some(),
        "real provider events did not produce authenticated journal overflow",
    );
    fixture.handle.close().unwrap();
    let closed = inspect!(fixture);
    assert!(closed.state.close.completed && closed.state.close.exited);
    let loss = closed.state.continuity_loss.unwrap();
    assert!(
        loss.reason
            .contains("outcome journal overflowed before the daemon committed"),
        "{}",
        loss.reason
    );
    assert!(!loss.acknowledged);
    assert!(fixture.handle.finish_retained_close().is_err());
    reconcile_previous_boot_history(&fixture.host).unwrap();
    fixture.assert_preserved();
}

fn install_provider(path: &Path) {
    fs::write(path, r#"#!/usr/bin/python3
import json, os, pathlib, select, sys
root=pathlib.Path(os.environ['HOME']).parent
args=sys.argv[1:]
if args == ['auth', 'status', '--json']:
    print(json.dumps({'loggedIn':True,'email':'archived-fixture@example.invalid','subscriptionType':'max'}))
    sys.exit(0)
if '--no-session-persistence' in args:
    for line in sys.stdin:
        frame=json.loads(line)
        response={'models':[]} if frame['request']['subtype']=='initialize' else {
            'subscription_type':'max','rate_limits':{'limits':[{'kind':'session','group':'session','percent':25,'resets_at':None,'is_active':True}]}}
        print(json.dumps({'type':'control_response','response':{'subtype':'success','request_id':frame['request_id'],'response':response}}),flush=True)
    sys.exit(0)
with (root/'launches').open('a') as log: log.write('launch\n')
session=next((a.split('=',1)[1] for a in args if a.startswith(('--session-id=', '--resume='))), None)
if session is None:
    raise ValueError('fixture requires an exact --session-id= or --resume= conversation')
initialized=False
sent=False
loss_sent=False
def send(value): print(json.dumps(value),flush=True)
while True:
    if initialized and not loss_sent and (root/'emit-loss').exists():
        for index in range(4100):
            request='archive-overflow-'+str(index)
            send({'type':'control_request','request_id':request,'request':{'subtype':'can_use_tool','tool_name':'Read','tool_use_id':request,'input':{'file_path':'/fixture/read-only'}}})
            send({'type':'control_cancel_request','request_id':request})
        loss_sent=True
    if initialized and not sent and (root/'emit-permission').exists():
        send({'type':'control_request','request_id':'archive-permission','request':{'subtype':'can_use_tool','tool_name':'Read','tool_use_id':'archive-read','input':{'file_path':'/fixture/read-only'}}})
        sent=True
    ready,_,_=select.select([sys.stdin],[],[],0.02)
    if not ready: continue
    line=sys.stdin.readline()
    if not line: break
    frame=json.loads(line)
    if frame.get('type')=='control_request':
        send({'type':'control_response','response':{'subtype':'success','request_id':frame['request_id'],'response':{}}})
        if frame.get('request',{}).get('subtype')=='initialize':
            project=''.join(c if c.isascii() and c.isalnum() else '-' for c in os.getcwd())
            directory=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'])/'projects'/project
            directory.mkdir(parents=True,exist_ok=True)
            records=[{'type':'user','uuid':'archive-user','sessionId':session,'message':{'role':'user','content':'Saved fixture question'}},
                     {'type':'assistant','uuid':'archive-assistant','parentUuid':'archive-user','sessionId':session,'message':{'id':'archive-message','role':'assistant','content':[{'type':'text','text':'Saved fixture answer'}]}}]
            (directory/(session+'.jsonl')).write_text(''.join(json.dumps(v)+'\n' for v in records))
            send({'type':'system','subtype':'init','session_id':session})
            initialized=True
    elif frame.get('type')=='user':
        with (root/'prompts').open('a') as log: log.write('prompt\n')
"#).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
