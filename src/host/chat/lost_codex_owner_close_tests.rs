//! Closing a Codex conversation whose native owner died without leaving a
//! close receipt. Its pane must close once nothing could still be running for
//! it, and only then.
use super::*;
use crate::daemon::holder_registry::{self as registry, Kind, Record};
use crate::model::{HostIncarnation, HostSessionId};
use std::os::unix::fs::PermissionsExt;

const MODE: &str = "CAGENTS_TEST_LOST_CODEX_OWNER_ROOT";

/// Reruns `test` with a private runtime directory, so its daemon lease and
/// holder registry are its own. Returns the fixture root in that rerun, and
/// `None` in the outer process once the rerun has passed.
fn isolated(test: &str) -> Option<std::path::PathBuf> {
    if let Some(root) = std::env::var_os(MODE) {
        let root = std::path::PathBuf::from(root);
        assert_eq!(
            std::env::var_os("XDG_RUNTIME_DIR"),
            Some(root.join("run").into_os_string())
        );
        return Some(root);
    }
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("run");
    crate::secure_file::prepare_private_directory(&runtime).unwrap();
    let stdout = directory.path().join("child.stdout");
    let stderr = directory.path().join("child.stderr");
    struct OriginalChild(std::process::Child);
    impl Drop for OriginalChild {
        fn drop(&mut self) {
            if let Ok(None) = self.0.try_wait() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }
    let mut child = OriginalChild(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("host::chat::lost_codex_owner_close_tests::{test}"),
                "--color=never",
            ])
            .env(MODE, directory.path())
            .env("HOME", directory.path())
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_DATA_HOME", directory.path().join("data"))
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .env("XDG_CACHE_HOME", directory.path().join("cache"))
            .stdout(std::fs::File::create(&stdout).unwrap())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "lost owner fixture did not settle"
        );
        thread::sleep(Duration::from_millis(5));
    };
    let stdout = std::fs::read_to_string(stdout).unwrap();
    assert!(
        status.success() && stdout.contains("1 passed"),
        "private lost owner fixture failed:\n{stdout}\n{}",
        std::fs::read_to_string(stderr).unwrap()
    );
    None
}

struct Fixture {
    host: SharedHost,
    source: Session,
    _lease: crate::daemon::HostLease,
}

impl Fixture {
    /// A started Codex conversation of this daemon, with an owned listener and
    /// nothing resident: what a pane holds after its holder died.
    fn new(root: &std::path::Path) -> Self {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("lost-codex-owner"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        // The daemon prepares its holder registry as it starts.
        crate::secure_file::prepare_private_directory(&registry::directory(Kind::Native).unwrap())
            .unwrap();
        let host = Arc::new(HostLock::new(HostState::new(proof.clone())));
        let store =
            super::super::model::Registry::at(&root.join("lost-codex-owner.sqlite")).unwrap();
        host.lock().unwrap().execution_registry = Some(Arc::new(std::sync::Mutex::new(store)));
        let id = TerminalId::new();
        let allocation = root.join(format!("cagents-codex-{id}"));
        std::fs::create_dir(&allocation).unwrap();
        std::fs::set_permissions(&allocation, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = allocation.join("control.sock");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let source = Session {
            id,
            launch: chat::Launch {
                provider: Provider::Codex,
                session_id: Some(uuid::Uuid::new_v4().to_string()),
                recipe: crate::model::LaunchRecipe {
                    program: "codex".into(),
                    arguments: vec!["--remote".into(), format!("unix://{}", socket.display())],
                    working_directory: project,
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: Some(chat::OwnedCodexListener {
                socket,
                allocator: proof,
            }),
            native_continuity: chat::NativeContinuity::ReconnectOnly,
        };
        assert!(source.owns_codex_listener());
        Self {
            host,
            source,
            _lease: lease,
        }
    }

    fn socket(&self) -> &std::path::Path {
        &self.source.owned_codex_listener.as_ref().unwrap().socket
    }

    /// A discovery record for this conversation's holder, as the holder writes
    /// it, naming `holder` and the provider `root`.
    fn record(&self, holder: registry::Process, root: registry::Process) -> Record {
        let directory = registry::directory(Kind::Native).unwrap();
        crate::secure_file::prepare_private_directory(&directory).unwrap();
        let record = Record {
            version: 4,
            build: None,
            kind: Kind::Native,
            application: crate::instance::APPLICATION_ID.into(),
            boot: crate::daemon::read_boot_id().unwrap(),
            terminal: self.source.id,
            holder,
            root,
        };
        record.create(&directory).unwrap();
        assert_eq!(
            registry::recovery_candidate(
                &registry::socket(&directory, self.source.id),
                Kind::Native
            )
            .unwrap(),
            Some(record.clone())
        );
        record
    }

    fn assert_settled(&self) {
        let state = self.host.lock().unwrap();
        assert!(!state.native_closing.contains_key(&self.source.id));
        assert!(!state.chats.contains_key(&self.source.id));
    }
}

/// A finished local actor: it read its history, exited and closed, so it owns
/// no work and inventory hides it. A stand-in for a Codex actor whose provider
/// ended with its turn, which a test cannot start without a provider.
fn finished_actor(root: &std::path::Path) -> chat::runtime::Handle {
    let session_id = "12345678-1234-4234-8234-123456789abc";
    let project = root.join("missing-project");
    let launch = chat::Launch {
        provider: Provider::Claude,
        session_id: Some(session_id.into()),
        recipe: crate::model::LaunchRecipe {
            program: "claude".into(),
            arguments: Vec::new(),
            working_directory: project.clone(),
            environment: [(
                "CLAUDE_CONFIG_DIR".into(),
                root.join("claude").to_str().unwrap().into(),
            )]
            .into(),
            unset_environment: Default::default(),
        },
    };
    let encoded: String = project
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let transcript = root
        .join("claude/projects")
        .join(encoded)
        .join(format!("{session_id}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(
        &transcript,
        serde_json::json!({"type":"user","uuid":"saved-item","parentUuid":null,"sessionId":session_id,"message":{"role":"user","content":"saved turn"}}).to_string() + "\n",
    )
    .unwrap();
    let (spec, mut adapter) = chat::protocol::create(&launch).unwrap();
    let mut initial = adapter.initialize().unwrap();
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let handle = chat::runtime::Handle::start(
        spec,
        adapter,
        initial,
        store,
        launch.session_id.clone(),
        |_| {},
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle
        .snapshot(None)
        .is_some_and(|snapshot| snapshot.exited)
    {
        assert!(Instant::now() < deadline, "history reader did not finish");
        thread::sleep(Duration::from_millis(10));
    }
    handle.close().unwrap();
    assert!(handle.is_completed() && !handle.owns_work());
    handle
}

/// A provider actor that still owns its provider's work: its own process
/// has ended, and the provider it serves is left with a native holder the
/// caller hands it. A stand-in for a Codex actor, which a test cannot start
/// without a provider.
fn provider_actor(root: &std::path::Path) -> chat::runtime::Handle {
    use chat::protocol::{Adapter, ProcessSpec, Step, TransportSpec};
    struct Quiet;
    impl Adapter for Quiet {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: chat::Command) -> Result<Step, String> {
            Err("no provider work in this fixture".into())
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let spec = ProcessSpec {
        stdout_noise: None,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: root.to_owned(),
        environment: Default::default(),
    };
    let mut initial = Step::default();
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let handle =
        chat::runtime::Handle::start(spec, Box::new(Quiet), initial, store, None, |_| {}).unwrap();
    handle.close().unwrap();
    handle
}

fn current() -> registry::Process {
    registry::Process::read(std::process::id() as i32).unwrap()
}

/// A process that has exited, leaving no group or session behind.
fn exited(pid: i32) -> registry::Process {
    registry::Process { pid, start: 1 }
}

/// #2426 and the pane that could not be closed: the holder is gone and so is
/// its record, so exact cleanup has nothing to adopt. Before, every Close was
/// refused with "No exact native holder or confirmed close receipt is
/// available" and the pane stayed open for good.
#[test]
fn a_pane_whose_codex_owner_left_no_trace_closes() {
    let Some(root) = isolated("a_pane_whose_codex_owner_left_no_trace_closes") else {
        return;
    };
    let fixture = Fixture::new(&root);
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
    fixture.assert_settled();
    // The allocation it no longer needs is given back.
    assert!(!fixture.socket().parent().unwrap().exists());
    // Closing it again is not a new refusal.
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
}

/// A holder that died with its provider: its record outlives both until
/// discovery runs, and names nothing that could still be stopped.
#[test]
fn a_pane_whose_holder_and_provider_both_exited_closes_and_retires_their_record() {
    let Some(root) =
        isolated("a_pane_whose_holder_and_provider_both_exited_closes_and_retires_their_record")
    else {
        return;
    };
    let fixture = Fixture::new(&root);
    fixture.record(exited(i32::MAX - 1), exited(i32::MAX - 2));
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
    assert!(!has_native_owner(&fixture.source).unwrap());
    fixture.assert_settled();
}

/// A provider that outlived its holder may still be running a turn. Nothing
/// can stop it through the pane, so the pane keeps refusing, and its record
/// stays as the only evidence of the provider.
#[test]
fn a_provider_that_outlived_its_holder_keeps_the_refusal() {
    let Some(root) = isolated("a_provider_that_outlived_its_holder_keeps_the_refusal") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let record = fixture.record(exited(i32::MAX - 1), current());
    let refusal = close_retained_from_pane(&fixture.host, &fixture.source).unwrap_err();
    // It says what still runs; the pane names the processes and ends them
    // (see `lost_processes`), so it gives no command to run.
    assert!(
        refusal.contains("Codex is still running for this conversation"),
        "{refusal}"
    );
    assert!(!refusal.contains('`'), "{refusal}");
    assert_eq!(
        registry::recovery_candidate(
            &registry::socket(
                &registry::directory(Kind::Native).unwrap(),
                fixture.source.id
            ),
            Kind::Native,
        )
        .unwrap(),
        Some(record)
    );
}

/// A provider must not hold its holder's channel to the daemon: if it does,
/// the holder's death never reaches the daemon while the provider runs on,
/// and every Close of the pane waits on the dead holder until it times out.
#[test]
fn a_provider_inherits_nothing_from_its_holder() {
    let Some(root) = isolated("a_provider_inherits_nothing_from_its_holder") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let _spawned = Spawned::new(&fixture.source);
    let record = Spawned::record(&fixture.source);
    let descriptors = std::fs::read_dir(format!("/proc/{}/fd", record.root.pid))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|fd| !["0", "1", "2"].contains(&fd.as_str()))
        .collect::<Vec<_>>();
    assert!(descriptors.is_empty(), "inherited {descriptors:?}");
}

/// A server still accepting connections at the conversation's endpoint is
/// running, whatever happened to the record that would have named it.
#[test]
fn a_listener_still_being_served_keeps_the_refusal() {
    let Some(root) = isolated("a_listener_still_being_served_keeps_the_refusal") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let _server = std::os::unix::net::UnixListener::bind(fixture.socket()).unwrap();
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap_err();
    assert!(fixture.socket().exists());
}

/// A start or close that holds the conversation's allocation may be about to
/// publish a server. Close's claim refuses while it is held, before any loss
/// is considered, and the pane closes once it is released.
#[test]
fn a_start_holding_the_allocation_keeps_the_refusal() {
    let Some(root) = isolated("a_start_holding_the_allocation_keeps_the_refusal") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let reservation = {
        let mut state = fixture.host.lock().unwrap();
        capacity::Reservation::closing(&fixture.host, &mut state, &fixture.source).unwrap()
    };
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap_err();
    assert!(fixture.socket().parent().unwrap().exists());
    drop(reservation);
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
}

/// The state the report left: the same daemon, no record, no receipt, and the
/// allocation itself already given back.
#[test]
fn a_pane_whose_allocation_is_already_gone_closes() {
    let Some(root) = isolated("a_pane_whose_allocation_is_already_gone_closes") else {
        return;
    };
    let fixture = Fixture::new(&root);
    std::fs::remove_dir(fixture.socket().parent().unwrap()).unwrap();
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
    fixture.assert_settled();
}

/// A process that names the conversation's listener, such as a server still
/// starting there or a CLI attached to it, may yet write the conversation.
#[test]
fn a_process_naming_the_listener_keeps_the_refusal() {
    let Some(root) = isolated("a_process_naming_the_listener_keeps_the_refusal") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let named = CodexProcess::new(&root, &[fixture.socket().to_str().unwrap()]);
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap_err();
    assert!(fixture.socket().parent().unwrap().exists());
    drop(named);
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
}

/// A receipt shows how this terminal's owner closed another descriptor of
/// its conversation. It proves nothing about this one either way, so this
/// one's Close asks what can still run, as it would with no receipt: once
/// nothing can, it closes, and the receipt stays for the close it confirms.
#[test]
fn a_receipt_for_another_descriptor_does_not_hold_the_close() {
    let Some(root) = isolated("a_receipt_for_another_descriptor_does_not_hold_the_close") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let record = fixture.record(exited(i32::MAX - 1), exited(i32::MAX - 2));
    let mut other = fixture.source.clone();
    other.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
    let proof = fixture.source.proof.clone().unwrap();
    chat::native_holder::publish_closed_source(&record, &proof, &other).unwrap();
    // The holder retires its record once its receipt is published.
    record.remove(&registry::directory(Kind::Native).unwrap());
    // The receipt is well formed: it proves the other conversation's close.
    assert!(
        chat::native_holder::closed_source(&other, &proof)
            .unwrap()
            .is_some()
    );
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
    fixture.assert_settled();
    assert!(
        chat::native_holder::closed_source(&other, &proof)
            .unwrap()
            .is_some()
    );
}

/// A holder spawned for the fixture's conversation, serving a provider that
/// never listens.
struct Spawned {
    holder: Option<chat::native_holder::Holder>,
    _context: Arc<chat::protocol::ExecutionContext>,
    _working: tempfile::TempDir,
}

impl Spawned {
    /// The discovery record the holder published for `source`.
    fn record(source: &Session) -> Record {
        registry::recovery_candidate(
            &registry::socket(&registry::directory(Kind::Native).unwrap(), source.id),
            Kind::Native,
        )
        .unwrap()
        .expect("the spawned holder published its record")
    }

    /// The owner as Close holds it once it has adopted the holder.
    fn detach(&mut self) -> chat::native_holder::Detached {
        self.holder.take().unwrap().preserve()
    }

    fn new(source: &Session) -> Self {
        Self::running(source, "/bin/sleep", &["60"])
    }

    fn running(source: &Session, program: &str, arguments: &[&str]) -> Self {
        let working = tempfile::tempdir_in("/var/tmp").unwrap();
        let context = Arc::new(chat::protocol::ExecutionContext::new(Some(Arc::new(
            std::fs::File::open(working.path()).unwrap(),
        ))));
        let socket = source.owned_codex_listener.as_ref().unwrap().socket.clone();
        let spec = chat::protocol::ProcessSpec {
            stdout_noise: None,
            execution: Some(context.clone()),
            messages: None,
            transport: chat::protocol::TransportSpec::OwnedUnixWebSocket { socket },
            program: program.into(),
            arguments: arguments
                .iter()
                .map(|argument| (*argument).into())
                .collect(),
            working_directory: working.path().to_owned(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let holder = chat::native_holder::spawn(spec, source.clone()).unwrap();
        context.installed();
        Self {
            holder: Some(holder),
            _context: context,
            _working: working,
        }
    }
}

fn kill(process: &registry::Process) {
    unsafe { libc::kill(process.pid, libc::SIGKILL) };
    let deadline = Instant::now() + Duration::from_secs(10);
    while process.is_current() {
        assert!(Instant::now() < deadline, "the killed process never exited");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Close adopted a live holder, then its Release failed or was never
/// answered, and the holder died with its provider before the retry. That
/// close can never be confirmed by a receipt, and every retry refused it with
/// "The native cleanup receipt is unavailable"; it now settles as the lost
/// owner it is. `released` is whether the earlier attempt's Release had
/// already settled, leaving only the confirmation to fail.
fn pending_close_whose_owner_then_ended(root: &std::path::Path, released: bool) {
    let fixture = Fixture::new(root);
    let mut spawned = Spawned::new(&fixture.source);
    let record = Spawned::record(&fixture.source);
    let owner = spawned.detach();
    fixture.host.lock().unwrap().native_closing.insert(
        fixture.source.id,
        native_cleanup::NativeClose::adopted(fixture.source.clone(), owner, released),
    );
    // A pending close of a live owner waits for it.
    if !released {
        assert!(has_native_owner(&fixture.source).unwrap());
    }
    kill(&record.holder);
    kill(&record.root);
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
    fixture.assert_settled();
    assert!(!has_native_owner(&fixture.source).unwrap());
}

#[test]
fn a_pending_close_whose_owner_then_ended_closes() {
    let Some(root) = isolated("a_pending_close_whose_owner_then_ended_closes") else {
        return;
    };
    pending_close_whose_owner_then_ended(&root, false);
}

#[test]
fn a_released_close_whose_owner_then_ended_closes() {
    let Some(root) = isolated("a_released_close_whose_owner_then_ended_closes") else {
        return;
    };
    pending_close_whose_owner_then_ended(&root, true);
}

/// A pending close whose holder died while its provider runs on may still be
/// writing the conversation, so the close keeps refusing and stays pending,
/// and settles once the provider has ended too.
#[test]
fn a_pending_close_whose_provider_outlived_its_holder_waits_for_it() {
    let Some(root) = isolated("a_pending_close_whose_provider_outlived_its_holder_waits_for_it")
    else {
        return;
    };
    let fixture = Fixture::new(&root);
    let mut spawned = Spawned::new(&fixture.source);
    let record = Spawned::record(&fixture.source);
    // The holder dies while it still serves its provider.
    kill(&record.holder);
    assert!(record.root.is_current(), "the provider outlived its holder");
    let owner = spawned.detach();
    fixture.host.lock().unwrap().native_closing.insert(
        fixture.source.id,
        native_cleanup::NativeClose::adopted(fixture.source.clone(), owner, false),
    );
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap_err();
    assert!(
        fixture
            .host
            .lock()
            .unwrap()
            .native_closing
            .contains_key(&fixture.source.id)
    );
    assert!(has_native_owner(&fixture.source).unwrap());
    kill(&record.root);
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
    fixture.assert_settled();
}

/// The pane a person has open while its holder dies and Codex runs on: the
/// actor is still resident and asks its holder to Release. That reached only
/// the dead holder's closed channel, and Close reported "Broken pipe (os error
/// 32)" (or, while the provider still held that channel, waited until "Resource
/// temporarily unavailable"). Close now names what still runs and how to end
/// it, and the pane closes once it has ended.
#[test]
fn an_open_pane_whose_holder_died_names_what_runs_then_closes() {
    let Some(root) = isolated("an_open_pane_whose_holder_died_names_what_runs_then_closes") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let mut spawned = Spawned::new(&fixture.source);
    let record = Spawned::record(&fixture.source);
    let handle = provider_actor(&root);
    handle.fixture_retained_native_holder(
        spawned.holder.take().unwrap(),
        "The provider is still running".into(),
    );
    assert!(handle.owns_work());
    fixture.host.lock().unwrap().chats.insert(
        fixture.source.id,
        super::Record {
            execution_instance: uuid::Uuid::new_v4(),
            console_recipe: None,
            paired_terminal: None,
            session: fixture.source.clone(),
            handle,
            source_process: None,
            subscribers: BTreeMap::new(),
            observations: BTreeMap::new(),
            completion_observed: true,
            returning_terminal: false,
        },
    );
    let close = || {
        let target = {
            let state = fixture.host.lock().unwrap();
            CloseTarget::capture(&state, &state.chats[&fixture.source.id]).unwrap()
        };
        close_captured(&fixture.host, target)
    };
    kill(&record.holder);
    assert!(record.root.is_current(), "the provider outlived its holder");
    let refusal = close().err().expect("Close refused").message;
    assert!(
        refusal.contains("Codex is still running for this conversation"),
        "{refusal}"
    );
    assert!(
        fixture
            .host
            .lock()
            .unwrap()
            .chats
            .contains_key(&fixture.source.id)
    );
    // The pane names the provider, and the person ends it from there and
    // closes the pane again.
    let found = lost_processes(&fixture.host, &fixture.source, &[]).unwrap();
    assert!(
        found
            .iter()
            .any(|process| process.process.pid == record.root.pid && process.proved),
        "{found:?}"
    );
    assert_eq!(
        lost_processes(&fixture.host, &fixture.source, &shown(&found)).unwrap(),
        []
    );
    assert!(record.root.reaped(), "ending left the provider running");
    close().unwrap();
    fixture.assert_settled();
    assert!(!has_native_owner(&fixture.source).unwrap());
    assert!(!fixture.socket().parent().unwrap().exists());
}

/// A tool Codex started stays in its process group after Codex and the
/// helper die, and still holds the conversation. Its pane names it, though it
/// names no listener, and ends it, and the conversation then closes.
#[test]
fn a_codex_tool_left_after_its_provider_and_helper_died_is_ended_then_closes() {
    let Some(root) =
        isolated("a_codex_tool_left_after_its_provider_and_helper_died_is_ended_then_closes")
    else {
        return;
    };
    let fixture = Fixture::new(&root);
    let spawned = Spawned::running(
        &fixture.source,
        "/bin/sh",
        &[
            "-c",
            "/bin/sleep 120 & echo $! > \"$0\"; exec /bin/sleep 120",
            "tool-pid",
        ],
    );
    let record = Spawned::record(&fixture.source);
    let tool_pid = {
        let pid_file = spawned._working.path().join("tool-pid");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|pid| pid.trim().parse::<i32>().ok())
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "the provider never started its tool"
            );
            thread::sleep(Duration::from_millis(10));
        }
    };
    let tool = registry::Process::read(tool_pid).unwrap();
    kill(&record.holder);
    kill(&record.root);
    assert!(tool.is_current(), "the tool outlived its provider");
    let refusal = close_retained_from_pane(&fixture.host, &fixture.source).unwrap_err();
    assert!(
        refusal.contains("Codex is still running for this conversation"),
        "{refusal}"
    );
    let found = lost_processes(&fixture.host, &fixture.source, &[]).unwrap();
    assert_eq!(
        found
            .iter()
            .map(|process| (process.process.pid, process.name.as_str(), process.proved))
            .collect::<Vec<_>>(),
        [(tool_pid, "sleep", true)]
    );
    assert_eq!(
        lost_processes(&fixture.host, &fixture.source, &shown(&found)).unwrap(),
        []
    );
    assert!(tool.reaped(), "ending left the tool running");
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
    fixture.assert_settled();
    assert!(!has_native_owner(&fixture.source).unwrap());
}

/// Reopening a conversation whose pane closed this way: its owner left no
/// receipt, and the old refusal asked the user to restore the pane they had
/// just closed and close it again.
#[test]
fn a_conversation_closed_after_losing_its_owner_reopens() {
    let Some(root) = isolated("a_conversation_closed_after_losing_its_owner_reopens") else {
        return;
    };
    let fixture = Fixture::new(&root);
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
    let ServerResponse::ChatResumePrepared { session } = process_inner(
        &fixture.host,
        0,
        ClientRequest::ChatPrepareResume {
            source: fixture.source.clone(),
        },
    )
    .unwrap() else {
        panic!("expected a prepared resume");
    };
    assert_ne!(session.id, fixture.source.id);
    // The same conversation, on a listener of its own.
    assert_eq!(session.launch.provider, Provider::Codex);
    assert_eq!(session.launch.session_id, fixture.source.launch.session_id);
    assert!(session.owned_codex_listener.is_none());
}

/// While anything of the old conversation may still run, reopening it keeps
/// refusing, as it does without a receipt.
#[test]
fn a_conversation_whose_listener_is_served_does_not_reopen() {
    let Some(root) = isolated("a_conversation_whose_listener_is_served_does_not_reopen") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let _server = std::os::unix::net::UnixListener::bind(fixture.socket()).unwrap();
    process_inner(
        &fixture.host,
        0,
        ClientRequest::ChatPrepareResume {
            source: fixture.source.clone(),
        },
    )
    .err()
    .expect("a served conversation was reopened");
}

/// #2426: a turn finished and its provider exited with it, taking the holder
/// and its record. The finished actor stays resident but owns no work, so the
/// pane's inventory no longer lists it and Close goes to exact cleanup, which
/// refused it with "No exact native holder or confirmed close receipt is
/// available". The pane now closes, and the resident goes with it.
#[test]
fn a_finished_resident_whose_owner_ended_closes() {
    let Some(root) = isolated("a_finished_resident_whose_owner_ended_closes") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let handle = finished_actor(&root);
    fixture.host.lock().unwrap().chats.insert(
        fixture.source.id,
        super::Record {
            execution_instance: uuid::Uuid::new_v4(),
            console_recipe: None,
            paired_terminal: None,
            session: fixture.source.clone(),
            handle,
            source_process: None,
            subscribers: BTreeMap::new(),
            observations: BTreeMap::new(),
            completion_observed: true,
            returning_terminal: false,
        },
    );
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
    fixture.assert_settled();
    assert!(!fixture.socket().parent().unwrap().exists());
}

/// An open agent bound to `fixture`'s conversation, with its capability, as
/// `cagents agent spawn --provider codex` leaves one whose provider then died
/// in this boot with no daemon stop recorded.
fn bound_agent(fixture: &Fixture, root: &std::path::Path) -> crate::agent_execution::Accepted {
    use crate::agent_execution::{
        Destination, HostedSession, Origin, Spec, Submit, Surface, Target,
    };
    let directory = fixture.source.launch.recipe.working_directory.clone();
    let registry = fixture
        .host
        .lock()
        .unwrap()
        .execution_registry
        .clone()
        .unwrap();
    let mut registry = registry.lock().unwrap();
    registry
        .apply(&crate::workspace_model::ModelEdit::OpenProject {
            repository: directory.clone(),
            name: "Fixture".into(),
            kind: crate::model::ProjectKind::Folder,
        })
        .unwrap();
    let submit = Submit {
        name: None,
        destination: Destination::New(Spec {
            provider: Provider::Codex,
            account: None,
            target: Target {
                repository: directory.clone(),
                directory,
                placement: None,
            },
            surface: Surface::Chat,
            model: None,
            permission_mode: None,
            reasoning_effort: None,
        }),
        prompt: "List the files here".into(),
        idempotency_key: root.display().to_string(),
    };
    let accepted = registry.submit_agent(&Origin::User, &submit, 1).unwrap();
    let source = HostedSession::Chat(fixture.source.clone());
    registry
        .bind_agent(accepted.agent.id, None, source.clone())
        .unwrap();
    registry.register_agent_authority(&source).unwrap();
    assert!(!registry.agent_process_ended(accepted.agent.id).unwrap());
    accepted
}

fn agent_closed(fixture: &Fixture, id: crate::model::AgentId) -> bool {
    let registry = fixture
        .host
        .lock()
        .unwrap()
        .execution_registry
        .clone()
        .unwrap();
    let registry = registry.lock().unwrap();
    registry.agent(id).unwrap().closed
        && registry
            .retained_agent_authority_source(fixture.source.id)
            .unwrap()
            .is_none()
}

/// The pane of a Codex agent whose holder and provider died in this boot.
/// Nothing settled it: the lost-owner proof refused any conversation an
/// agent claimed, and the agent's own close found no receipt either. Its
/// Close now ends the agent with the pane.
#[test]
fn a_lost_codex_agents_pane_closes_and_ends_the_agent() {
    let Some(root) = isolated("a_lost_codex_agents_pane_closes_and_ends_the_agent") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let accepted = bound_agent(&fixture, &root);
    fixture.record(exited(i32::MAX - 1), exited(i32::MAX - 2));
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
    fixture.assert_settled();
    assert!(agent_closed(&fixture, accepted.agent.id));
    assert!(!fixture.socket().parent().unwrap().exists());
}

/// `cagents agent close` takes the same route for that agent.
#[test]
fn a_lost_codex_agent_closes_by_command() {
    let Some(root) = isolated("a_lost_codex_agent_closes_by_command") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let accepted = bound_agent(&fixture, &root);
    let reply = super::super::agent_execution::process(
        &fixture.host,
        Default::default(),
        crate::agent_execution::Operation::Close(accepted.agent.id),
    )
    .unwrap();
    assert!(matches!(reply, crate::agent_execution::Reply::Closed(agent) if agent.closed));
    assert!(agent_closed(&fixture, accepted.agent.id));
    fixture.assert_settled();
}

/// A Codex agent whose provider outlived its holder may still be working, so
/// neither its pane nor its agent closes, and both stay as they were.
#[test]
fn a_codex_agent_whose_provider_survives_keeps_the_refusal() {
    let Some(root) = isolated("a_codex_agent_whose_provider_survives_keeps_the_refusal") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let accepted = bound_agent(&fixture, &root);
    fixture.record(exited(i32::MAX - 1), current());
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap_err();
    let registry = fixture
        .host
        .lock()
        .unwrap()
        .execution_registry
        .clone()
        .unwrap();
    assert!(
        !registry
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .closed
    );
    assert!(has_native_owner(&fixture.source).unwrap());
}

/// A fake app-server records its actual launch, then waits without speaking the
/// provider protocol. These tests cover execution admission, not provider RPC.
fn codex_start_fixture(root: &std::path::Path) -> Fixture {
    let mut fixture = Fixture::new(root);
    let program = root.join("codex");
    std::fs::write(
        &program,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CAGENTS_START_PROOF_ARGV\"\nexec /bin/sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.join("codex-home");
    std::fs::create_dir(&home).unwrap();
    fixture.source.launch.recipe.program = program.to_str().unwrap().into();
    fixture.source.launch.recipe.environment = BTreeMap::from([
        ("CODEX_HOME".into(), home.to_str().unwrap().into()),
        (
            "CAGENTS_START_PROOF_ARGV".into(),
            root.join("started-argv").to_str().unwrap().into(),
        ),
    ]);
    fixture.host.lock().unwrap().codex_listeners = CodexListenerRoots::at(root.into());
    fixture
}

fn codex_start(fixture: &Fixture) -> Result<ServerResponse, ProtocolError> {
    start(
        &fixture.host,
        1,
        fixture.source.clone(),
        None,
        false,
        None,
        Default::default(),
    )
}

fn assert_codex_started(fixture: &Fixture, root: &std::path::Path) {
    let response = process_inner(
        &fixture.host,
        1,
        ClientRequest::ChatStart {
            session: fixture.source.clone(),
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    )
    .unwrap();
    assert!(matches!(response, ServerResponse::ChatAttached { .. }));
    if fixture.source.launch.recipe.arguments == ["--remote", "unix://"] {
        let ServerResponse::ChatAttached { session, .. } = &response else {
            unreachable!()
        };
        assert!(
            fixture.source.resolved_to_owned_codex_listener(session),
            "the resumed allocation must pass the saved pane's checkpoint contract"
        );
    }
    let expected = format!(
        "app-server\n--listen\nunix://{}\n",
        fixture.host.lock().unwrap().chats[&fixture.source.id]
            .session
            .owned_codex_listener
            .as_ref()
            .unwrap()
            .socket
            .display()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let argv = loop {
        if let Ok(argv) = std::fs::read_to_string(root.join("started-argv")) {
            if argv == expected {
                break argv;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the fake app-server never launched"
        );
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(
        argv.lines().collect::<Vec<_>>(),
        ["app-server", "--listen", expected.lines().nth(2).unwrap()]
    );
    let handle = fixture.host.lock().unwrap().chats[&fixture.source.id]
        .handle
        .clone();
    handle.close().unwrap();
    let record = registry::recovery_candidate(
        &registry::socket(
            &registry::directory(Kind::Native).unwrap(),
            fixture.source.id,
        ),
        Kind::Native,
    )
    .unwrap();
    assert!(
        record.is_none(),
        "closing must reap the fake app-server's holder"
    );
}

/// A readable same-user process with a Codex executable name and controlled
/// arguments. It never contacts a provider; stdin keeps it alive until drop.
struct CodexProcess(std::process::Child);

impl CodexProcess {
    fn new(root: &std::path::Path, arguments: &[&str]) -> Self {
        use std::io::Read;
        let program = root.join("writer/codex");
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::copy("/bin/bash", &program).unwrap();
        let mut child = std::process::Command::new(&program)
            .args(["-c", "printf ready; IFS= read -r release", "codex"])
            .args(arguments)
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
        Self(child)
    }

    fn refusal(&self) -> String {
        let pid = self.0.id();
        format!(
            "Codex is still running this conversation (process {pid}). Stop process {pid}, then try again."
        )
    }
}

impl Drop for CodexProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Startup and a lost/unlinked endpoint can leave a writer alive with no
/// served socket. The same ChatStart must refuse until that writer exits.
#[test]
fn codex_start_refuses_a_surviving_process_then_starts_after_it_exits() {
    let Some(root) = isolated("codex_start_refuses_a_surviving_process_then_starts_after_it_exits")
    else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    let old_socket = fixture.socket().to_owned();
    fixture.source.owned_codex_listener = None;
    let named = CodexProcess::new(&root, &[old_socket.to_str().unwrap()]);
    assert!(!old_socket.exists());
    let failure = codex_start(&fixture).unwrap_err();
    assert!(
        failure.retryable,
        "the same pane can retry after its writer exits"
    );
    assert!(
        failure.message.ends_with(&named.refusal()),
        "{}",
        failure.message
    );
    // A stale saved boot is no permission to replace this boot's orphaned
    // app-server, even after its ownership record and socket disappeared.
    fixture.source.proof.as_mut().unwrap().boot_id = crate::model::BootId::new("earlier-boot");
    let stale_failure = codex_start(&fixture).unwrap_err();
    assert!(stale_failure.retryable);
    assert!(
        stale_failure.message.ends_with(&named.refusal()),
        "{}",
        stale_failure.message
    );
    assert!(!root.join("started-argv").exists());
    assert!(fixture.host.lock().unwrap().chat_spawning.is_empty());
    drop(named);
    assert_codex_started(&fixture, &root);
}

#[test]
fn codex_start_legacy_pane_without_a_surviving_process_starts() {
    let Some(root) = isolated("codex_start_legacy_pane_without_a_surviving_process_starts") else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    fixture.source.owned_codex_listener = None;
    assert_codex_started(&fixture, &root);
}

#[test]
fn codex_start_earlier_boot_legacy_pane_starts() {
    let Some(root) = isolated("codex_start_earlier_boot_legacy_pane_starts") else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    let mut record = fixture.record(current(), current());
    record.remove(&registry::directory(Kind::Native).unwrap());
    record.boot = crate::model::BootId::new("earlier-boot");
    record
        .create(&registry::directory(Kind::Native).unwrap())
        .unwrap();
    fixture.source.proof.as_mut().unwrap().boot_id = record.boot;
    fixture.source.owned_codex_listener = None;
    assert_codex_started(&fixture, &root);
}

#[test]
fn codex_start_legacy_pane_with_an_ended_holder_record_starts() {
    let Some(root) = isolated("codex_start_legacy_pane_with_an_ended_holder_record_starts") else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    fixture.record(exited(i32::MAX - 1), exited(i32::MAX - 2));
    fixture.source.owned_codex_listener = None;
    let control = registry::socket(
        &registry::directory(Kind::Native).unwrap(),
        fixture.source.id,
    );
    drop(registry::bind(&control).unwrap());
    assert_codex_started(&fixture, &root);
}

#[test]
fn codex_start_paired_console_without_surviving_execution_starts() {
    let Some(root) = isolated("codex_start_paired_console_without_surviving_execution_starts")
    else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    fixture.source.terminal = Some(
        crate::terminal_ipc::TerminalDescriptor::relaunchable(
            fixture.source.id,
            fixture.source.launch.recipe.clone(),
            None,
        )
        .session,
    );
    std::os::unix::fs::symlink(root.join("missing-socket"), fixture.socket()).unwrap();
    assert_codex_started(&fixture, &root);
}

#[test]
fn codex_picker_resume_of_an_ended_pane_starts() {
    let Some(root) = isolated("codex_picker_resume_of_an_ended_pane_starts") else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
    let ServerResponse::ChatResumePrepared { session } = process_inner(
        &fixture.host,
        1,
        ClientRequest::ChatPrepareResume {
            source: fixture.source.clone(),
        },
    )
    .unwrap() else {
        panic!("expected a prepared resume");
    };
    assert_ne!(session.id, fixture.source.id);
    assert_eq!(session.launch.session_id, fixture.source.launch.session_id);
    fixture.source = session;
    assert_codex_started(&fixture, &root);
}

#[test]
fn codex_picker_resume_with_earlier_boot_console_evidence_starts() {
    let Some(root) = isolated("codex_picker_resume_with_earlier_boot_console_evidence_starts")
    else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    let mut proof = fixture.source.proof.clone().unwrap();
    proof.boot_id = crate::model::BootId::new("prior-boot");
    fixture.source.proof = Some(proof.clone());
    fixture
        .source
        .owned_codex_listener
        .as_mut()
        .unwrap()
        .allocator = proof.clone();
    let console_id = fixture.source.id;
    let mut console = crate::terminal_ipc::TerminalDescriptor::relaunchable(
        console_id,
        fixture.source.launch.recipe.clone(),
        None,
    )
    .session;
    console.continuity = crate::model::TerminalContinuity::Live {
        host_session_id: proof.host_session_id,
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation,
    };
    fixture.source.terminal = Some(console);
    let directory = registry::directory(Kind::Terminal).unwrap();
    crate::secure_file::prepare_private_directory(&directory).unwrap();
    Record {
        version: 4,
        build: None,
        kind: Kind::Terminal,
        application: crate::instance::APPLICATION_ID.into(),
        boot: proof.boot_id,
        terminal: console_id,
        holder: current(),
        root: current(),
    }
    .create(&directory)
    .unwrap();
    let ServerResponse::ChatResumePrepared { session } = process_inner(
        &fixture.host,
        1,
        ClientRequest::ChatPrepareResume {
            source: fixture.source.clone(),
        },
    )
    .unwrap() else {
        panic!("expected a prepared resume");
    };
    fixture.source = session;
    assert_codex_started(&fixture, &root);
}

#[test]
fn codex_picker_reports_unreadable_console_evidence() {
    let Some(root) = isolated("codex_picker_reports_unreadable_console_evidence") else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    let console = crate::terminal_ipc::TerminalDescriptor::relaunchable(
        fixture.source.id,
        fixture.source.launch.recipe.clone(),
        None,
    )
    .session;
    let directory = registry::directory(Kind::Terminal).unwrap();
    crate::secure_file::prepare_private_directory(&directory).unwrap();
    let path = registry::socket(&directory, console.id).with_extension("json");
    std::fs::write(&path, "unreadable record").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    fixture.source.terminal = Some(console);
    let failure = process_inner(
        &fixture.host,
        1,
        ClientRequest::ChatPrepareResume {
            source: fixture.source.clone(),
        },
    )
    .unwrap_err();
    assert!(
        failure.message.contains("Cannot read holder record"),
        "{}",
        failure.message
    );
    assert!(!root.join("started-argv").exists());
}

#[test]
fn codex_start_legacy_equals_endpoint_without_survivors_starts() {
    let Some(root) = isolated("codex_start_legacy_equals_endpoint_without_survivors_starts") else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    fixture.source.launch.recipe.arguments =
        vec![format!("--remote=unix://{}", fixture.socket().display())];
    fixture.source.owned_codex_listener = None;
    assert_codex_started(&fixture, &root);
}

#[test]
fn codex_picker_resume_of_an_ended_legacy_pane_starts() {
    let Some(root) = isolated("codex_picker_resume_of_an_ended_legacy_pane_starts") else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    fixture.source.owned_codex_listener = None;
    // The legacy picker performs this transformation without PrepareResume.
    fixture.source.reset_owned_codex_listener();
    fixture.source.id = TerminalId::new();
    fixture.source.proof = None;
    fixture.source.terminal = None;
    assert_codex_started(&fixture, &root);
}

#[test]
fn codex_start_brand_new_conversation_needs_no_old_execution_evidence() {
    let Some(root) = isolated("codex_start_brand_new_conversation_needs_no_old_execution_evidence")
    else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    fixture.source.launch.session_id = None;
    fixture.source.proof = None;
    assert_codex_started(&fixture, &root);
}

#[test]
fn codex_start_refuses_unreadable_listener_evidence() {
    let Some(root) = isolated("codex_start_refuses_unreadable_listener_evidence") else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    // ELOOP is unreadable evidence, rather than a missing/ended listener.
    let looped = root.join("loop");
    std::os::unix::fs::symlink(&looped, &looped).unwrap();
    let socket = looped
        .join(format!("cagents-codex-{}", fixture.source.id))
        .join("control.sock");
    fixture.source.launch.recipe.arguments[1] = format!("unix://{}", socket.display());
    fixture.source.owned_codex_listener.as_mut().unwrap().socket = socket;
    assert!(fixture.source.owns_codex_listener());
    let failure = codex_start(&fixture).unwrap_err();
    assert!(
        failure
            .message
            .contains("Cannot read Codex listener target"),
        "{}",
        failure.message
    );
    assert!(!root.join("started-argv").exists());
}

#[test]
fn codex_start_refuses_a_bound_unlinked_published_target() {
    let Some(root) = isolated("codex_start_refuses_a_bound_unlinked_published_target") else {
        return;
    };
    let fixture = codex_start_fixture(&root);
    let target = root.join("published.sock");
    let _server = std::os::unix::net::UnixListener::bind(&target).unwrap();
    std::os::unix::fs::symlink(&target, fixture.socket()).unwrap();
    std::fs::remove_file(&target).unwrap();
    let failure = codex_start(&fixture).unwrap_err();
    assert!(
        failure.message.ends_with(&format!(
            "Codex is still serving this conversation at {}. Stop that server, then try again.",
            target.display()
        )),
        "{}",
        failure.message
    );
    assert!(
        std::fs::symlink_metadata(fixture.socket())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!root.join("started-argv").exists());
}

#[test]
fn codex_start_joins_a_served_listener_without_launching() {
    let Some(root) = isolated("codex_start_joins_a_served_listener_without_launching") else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    let target = root.join("published.sock");
    let _server = std::os::unix::net::UnixListener::bind(&target).unwrap();
    std::os::unix::fs::symlink(&target, fixture.socket()).unwrap();
    // Missing old ownership evidence must not block an existing attachment.
    fixture.source.owned_codex_listener = None;
    let response = codex_start(&fixture).unwrap();
    assert!(matches!(response, ServerResponse::ChatAttached { .. }));
    assert!(!root.join("started-argv").exists());
    assert!(
        std::fs::symlink_metadata(
            root.join(format!("cagents-codex-{}", fixture.source.id))
                .join("control.sock")
        )
        .is_ok()
    );
    let handle = fixture.host.lock().unwrap().chats[&fixture.source.id]
        .handle
        .clone();
    handle.close().unwrap();
}

#[test]
fn codex_picker_resume_refuses_the_old_panes_surviving_marked_process() {
    let Some(root) = isolated("codex_picker_resume_refuses_the_old_panes_surviving_marked_process")
    else {
        return;
    };
    let fixture = codex_start_fixture(&root);
    let socket = fixture.socket().to_str().unwrap();
    let writer = CodexProcess::new(&root, &["app-server", "--listen", socket]);
    let failure = process_inner(
        &fixture.host,
        1,
        ClientRequest::ChatPrepareResume {
            source: fixture.source.clone(),
        },
    )
    .unwrap_err();
    assert!(
        failure.retryable,
        "the same saved source can retry after its writer exits"
    );
    assert!(
        failure.message.ends_with(&writer.refusal()),
        "{}",
        failure.message
    );
    assert!(!root.join("started-argv").exists());
    assert!(fixture.host.lock().unwrap().chat_spawning.is_empty());
    drop(writer);
    assert!(matches!(
        process_inner(
            &fixture.host,
            1,
            ClientRequest::ChatPrepareResume {
                source: fixture.source.clone(),
            },
        )
        .unwrap(),
        ServerResponse::ChatResumePrepared { .. }
    ));
    assert!(!root.join("started-argv").exists());
}

#[test]
fn codex_picker_resume_refuses_a_terminal_writer_by_thread_id() {
    let Some(root) = isolated("codex_picker_resume_refuses_a_terminal_writer_by_thread_id") else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    let writer = CodexProcess::new(
        &root,
        &[
            "resume",
            fixture.source.launch.session_id.as_deref().unwrap(),
        ],
    );
    // The legacy picker chooses a new pane and drops endpoint provenance. The
    // conversation ID still identifies the writer using the user's daemon.
    fixture.source.reset_owned_codex_listener();
    fixture.source.id = TerminalId::new();
    fixture.source.proof = None;
    let failure = codex_start(&fixture).unwrap_err();
    assert!(
        failure.message.ends_with(&writer.refusal()),
        "{}",
        failure.message
    );
    assert!(!root.join("started-argv").exists());
    assert!(fixture.host.lock().unwrap().chat_spawning.is_empty());
}

#[test]
fn codex_picker_resume_refuses_a_live_terminals_recorded_conversation() {
    let Some(root) = isolated("codex_picker_resume_refuses_a_live_terminals_recorded_conversation")
    else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    fixture.host.lock().unwrap().register_client(1);
    let terminal = crate::terminal_ipc::TerminalDescriptor::relaunchable(
        TerminalId::new(),
        crate::model::LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: root.join("project"),
            environment: Default::default(),
            unset_environment: Default::default(),
        },
        None,
    )
    .session;
    let terminal_id = terminal.id;
    crate::host::state::spawn_terminal(&fixture.host, 1, terminal, Default::default()).unwrap();
    let handle = {
        let mut state = fixture.host.lock().unwrap();
        let terminal = state.terminals.get_mut(&terminal_id).unwrap();
        let mut recorded = fixture.source.launch.recipe.clone();
        recorded.arguments = vec![
            "resume".into(),
            fixture.source.launch.session_id.clone().unwrap(),
        ];
        terminal.session.resume_hint = crate::model::agent_recovery::hint_from_launch(&recorded);
        assert!(terminal.session.resume_hint.is_some());
        terminal.handle.clone()
    };
    // Close uses the same recorded terminal ownership evidence; otherwise it
    // could classify this conversation as lost while the CLI still owns it.
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap_err();
    assert!(fixture.socket().parent().unwrap().exists());
    // The recorded launch can also be an interpreter, before its native
    // child publishes a marker. Exercise the recipe fallback without a hint.
    {
        let mut state = fixture.host.lock().unwrap();
        let terminal = state.terminals.get_mut(&terminal_id).unwrap();
        terminal.session.resume_hint = None;
        terminal.session.launch.program = "bun".into();
        terminal.session.launch.arguments = vec![
            "/path/codex.js".into(),
            "resume".into(),
            fixture.source.launch.session_id.clone().unwrap(),
        ];
    }
    fixture.source.reset_owned_codex_listener();
    fixture.source.id = TerminalId::new();
    fixture.source.proof = None;
    let failure = codex_start(&fixture).unwrap_err();
    assert!(
        failure.message.ends_with(&format!(
            "Codex is still running this conversation in terminal {terminal_id}. Close that terminal, then try again."
        )),
        "{}",
        failure.message
    );
    assert!(!root.join("started-argv").exists());
    assert!(fixture.host.lock().unwrap().chat_spawning.is_empty());
    handle.shutdown().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.is_completed() {
        assert!(Instant::now() < deadline, "the terminal did not exit");
        thread::sleep(Duration::from_millis(10));
    }
    assert_codex_started(&fixture, &root);
}

#[test]
fn codex_start_ignores_an_ordinary_process_merely_mentioning_the_listener() {
    let Some(root) =
        isolated("codex_start_ignores_an_ordinary_process_merely_mentioning_the_listener")
    else {
        return;
    };
    use std::io::Read;
    let fixture = codex_start_fixture(&root);
    let mut mention = CodexProcess(
        std::process::Command::new("/bin/bash")
            .args(["-c", "printf ready; IFS= read -r release"])
            .arg(fixture.socket())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut ready = [0; 5];
    mention
        .0
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut ready)
        .unwrap();
    assert_eq!(&ready, b"ready");
    assert_codex_started(&fixture, &root);
    assert!(mention.0.try_wait().unwrap().is_none());
}

#[test]
fn codex_start_waits_for_a_live_paired_console_without_a_holder_record() {
    let Some(root) =
        isolated("codex_start_waits_for_a_live_paired_console_without_a_holder_record")
    else {
        return;
    };
    let mut fixture = codex_start_fixture(&root);
    fixture.host.lock().unwrap().register_client(1);
    let terminal = crate::terminal_ipc::TerminalDescriptor::relaunchable(
        fixture.source.id,
        crate::model::LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: root.join("project"),
            environment: Default::default(),
            unset_environment: Default::default(),
        },
        None,
    )
    .session;
    let descriptor =
        crate::host::state::spawn_terminal(&fixture.host, 1, terminal, Default::default()).unwrap();
    fixture.source.terminal = Some(descriptor.session);
    let id = fixture.source.id;
    let handle = fixture.host.lock().unwrap().terminals[&id].handle.clone();
    assert!(
        registry::execution_record(&registry::socket(
            &registry::directory(Kind::Terminal).unwrap(),
            id
        ))
        .unwrap()
        .is_none(),
        "the fixture must exercise an in-process console"
    );
    let failure = codex_start(&fixture).unwrap_err();
    assert!(
        failure.message.contains("could not be checked"),
        "{}",
        failure.message
    );
    // The client descriptor can lag this daemon's live console, just as it
    // can lag an orphaned server. Its boot must not overrule the live handle.
    if let crate::model::TerminalContinuity::Live { boot_id, .. } =
        &mut fixture.source.terminal.as_mut().unwrap().continuity
    {
        *boot_id = crate::model::BootId::new("stale-console-boot");
    } else {
        panic!("spawned console must have live continuity");
    }
    let stale_failure = codex_start(&fixture).unwrap_err();
    assert!(
        stale_failure.message.contains("could not be checked"),
        "{}",
        stale_failure.message
    );
    assert!(!root.join("started-argv").exists());
    handle.shutdown().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.is_completed() {
        assert!(Instant::now() < deadline, "the paired console did not exit");
        thread::sleep(Duration::from_millis(10));
    }
    fixture.host.lock().unwrap().terminals.remove(&id);
    assert_codex_started(&fixture, &root);
}

#[test]
fn codex_retained_processes_do_not_claim_an_unbound_socket_is_serving() {
    let Some(root) = isolated("codex_retained_processes_do_not_claim_an_unbound_socket_is_serving")
    else {
        return;
    };
    let fixture = Fixture::new(&root);
    fixture.record(current(), current());
    assert!(!fixture.socket().exists());
    let failure = require_ended_codex_execution(
        &fixture.host,
        &fixture.source,
        &crate::daemon::read_boot_id().unwrap(),
    )
    .unwrap_err();
    assert_eq!(
        failure.to_string(),
        "The previous Codex execution still has live processes. Wait for them to finish, then try again."
    );
}
