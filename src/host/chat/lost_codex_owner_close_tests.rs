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
            .env("XDG_RUNTIME_DIR", &runtime)
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
        assert!(has_native_owner(&self.source).unwrap());
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
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap_err();
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
    struct Killed(std::process::Child);
    impl Drop for Killed {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let named = Killed(
        std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30; :"])
            .arg(fixture.socket())
            .spawn()
            .unwrap(),
    );
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap_err();
    assert!(fixture.socket().parent().unwrap().exists());
    drop(named);
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
}

/// A receipt shows how this terminal's owner was closed. A pane asking to close
/// another conversation there is answered by exact cleanup, which refuses it;
/// the receipt is not evidence that the request's owner was lost.
#[test]
fn a_receipt_for_another_conversation_keeps_the_refusal() {
    let Some(root) = isolated("a_receipt_for_another_conversation_keeps_the_refusal") else {
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
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap_err();
    assert!(fixture.socket().parent().unwrap().exists());
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
            program: "/bin/sleep".into(),
            arguments: vec!["60".into()],
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
