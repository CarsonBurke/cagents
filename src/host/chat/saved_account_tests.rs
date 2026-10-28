//! Real host recovery with a finite local provider. No account registry or
//! daemon-wide environment is replaced by these fixtures.
use super::*;
use crate::{
    chat::{
        Command, Event,
        protocol::{Adapter, ProcessSpec, Step},
    },
    daemon::holder_registry::{Kind, Process, Record as Holder},
    host::model::Registry,
    model::LaunchRecipe,
};
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
    sync::Mutex,
};

const THREAD: &str = "12345678-1234-4234-8234-123456789abc";

struct Idle;
impl Adapter for Idle {
    fn initialize(&mut self) -> Result<Step, String> {
        Ok(Step::default())
    }
    fn command(&mut self, _: Command) -> Result<Step, String> {
        Err("No source work is issued by this fixture".into())
    }
    fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
        Ok(Step::default())
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    host: SharedHost,
    source: Session,
    replacement: Session,
    original: Handle,
    target_transcript: PathBuf,
    confirmation: PathBuf,
    starts: PathBuf,
}
impl Fixture {
    fn new(completed: bool) -> Self {
        Self::with_ready(completed, true)
    }
    fn with_ready(completed: bool, ready: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().join("project");
        let original_home = directory.path().join("original");
        let target_home = directory.path().join("target");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&original_home).unwrap();
        std::fs::create_dir_all(&target_home).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let confirmation = directory.path().join("confirmation");
        let starts = directory.path().join("starts");
        std::fs::write(&confirmation, THREAD).unwrap();
        let provider = directory.path().join("claude");
        // The provider ignores argv like the existing handoff fixture, but its
        // confirmed identity comes from a file, independently of the resume hint.
        std::fs::write(&provider, format!(r#"#!/usr/bin/python3
import json, os, pathlib, select, time
root = pathlib.Path({root:?})
with (root / 'starts').open('a') as log:
    log.write('started\n')
thread = (root / 'confirmation').read_text().strip()
print(json.dumps({{'type': 'system', 'subtype': 'init', 'session_id': thread}}), flush=True)
end = time.monotonic() + 20
buffer = b''
while time.monotonic() < end:
    ready, _, _ = select.select([0], [], [], max(0, end - time.monotonic()))
    if not ready:
        break
    line = os.read(0, 65536)
    if not line:
        break
    buffer += line
    while b'\n' in buffer:
        raw, buffer = buffer.split(b'\n', 1)
        frame = json.loads(raw)
        if frame.get('request', {{}}).get('subtype') == 'set_permission_mode':
            (root / 'restored-mode').write_text(frame['request']['mode'])
        request = frame.get('request_id')
        if request and (root / 'fail-setup').exists() and frame.get('request', {{}}).get('subtype') == 'initialize':
            print(json.dumps({{'type': 'control_response', 'response': {{'subtype': 'error', 'request_id': request, 'error': 'fixture initial setup refused'}}}}), flush=True)
            continue
        if request:
            print(json.dumps({{'type': 'control_response', 'response': {{'subtype': 'success', 'request_id': request, 'response': {{'commands': [], 'models': []}}}}}}), flush=True)
"#, root = directory.path().to_str().unwrap())).unwrap();
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
        let project = cwd
            .to_str()
            .unwrap()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '-'
                }
            })
            .collect::<String>();
        let target_transcript = target_home
            .join("projects")
            .join(project)
            .join(format!("{THREAD}.jsonl"));
        std::fs::create_dir_all(target_transcript.parent().unwrap()).unwrap();
        std::fs::write(&target_transcript, "{}\n").unwrap();
        let proof = HostSessionProof {
            host_session_id: "saved-account-fixture".into(),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: uuid::Uuid::new_v4().to_string().into(),
        };
        let host = Arc::new(HostLock::new(HostState::new(proof.clone())));
        let registry = Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
        host.lock().unwrap().execution_registry = Some(Arc::new(Mutex::new(registry)));
        let source = Session {
            id: TerminalId::new(),
            owned_codex_listener: None,
            native_continuity: Default::default(),
            proof: Some(proof),
            terminal: None,
            launch: chat::Launch {
                provider: Provider::Claude,
                session_id: Some(THREAD.into()),
                recipe: LaunchRecipe {
                    program: provider.to_str().unwrap().into(),
                    arguments: vec![],
                    working_directory: cwd,
                    environment: BTreeMap::from([
                        ("HOME".into(), directory.path().to_str().unwrap().into()),
                        (
                            "CLAUDE_CONFIG_DIR".into(),
                            original_home.to_str().unwrap().into(),
                        ),
                    ]),
                    unset_environment: Default::default(),
                },
            },
        };
        let mut process_recipe = source.launch.recipe.clone();
        process_recipe.program = "/bin/cat".into();
        let spec = ProcessSpec::from(&process_recipe);
        let mut initial = Step {
            events: vec![
                Event::Ready(ready),
                Event::PermissionMode(Some("plan".into())),
                Event::Session {
                    id: THREAD.into(),
                    title: None,
                },
            ],
            ..Step::default()
        };
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let original = Handle::start(
            spec,
            Box::new(Idle),
            initial,
            store,
            Some(THREAD.into()),
            |_| {},
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while original.confirmed_session_id().as_deref() != Some(THREAD) {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        if completed {
            original.close().unwrap();
        }
        {
            let mut state = host.lock().unwrap();
            state.register_client(1);
            state.chats.insert(
                source.id,
                Record {
                    execution_instance: uuid::Uuid::new_v4(),
                    console_recipe: None,
                    paired_terminal: None,
                    session: source.clone(),
                    handle: original.clone(),
                    source_process: None,
                    subscribers: BTreeMap::from([(1, false)]),
                    observations: BTreeMap::new(),
                    completion_observed: completed,
                    returning_terminal: false,
                },
            );
        }
        let mut replacement = source.clone();
        replacement.id = TerminalId::new();
        replacement.proof = None;
        replacement.launch.recipe.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            target_home.to_str().unwrap().into(),
        );
        Self {
            directory,
            host,
            source,
            replacement,
            original,
            target_transcript,
            confirmation,
            starts,
        }
    }
    fn recover(&self) -> Result<ServerResponse, ProtocolError> {
        self.request(false)
    }
    fn request(&self, explicit: bool) -> Result<ServerResponse, ProtocolError> {
        use crate::terminal_ipc::{ClientEnvelope, ServerEnvelope, read_frame, write_frame};
        use std::os::unix::net::UnixStream;
        struct Serving {
            cancel: UnixStream,
            worker: Option<thread::JoinHandle<()>>,
        }
        impl Drop for Serving {
            fn drop(&mut self) {
                let _ = self.cancel.shutdown(std::net::Shutdown::Both);
                if let Some(worker) = self.worker.take() {
                    let _ = worker.join();
                }
            }
        }
        let (mut peer, server) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let host = self.host.clone();
        let _serving = Serving {
            cancel: peer.try_clone().unwrap(),
            worker: Some(thread::spawn(move || {
                super::super::client::serve(server, 1, host)
            })),
        };
        let proof = self.host.lock().unwrap().proof().clone();
        write_frame(
            &mut peer,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id: 1,
                proof: None,
                request: ClientRequest::Connect {
                    expected_host: Some(proof.clone()),
                    resume: vec![],
                },
            },
        )
        .unwrap();
        let connected: ServerEnvelope = read_frame(&mut peer).unwrap().unwrap();
        assert_eq!(connected.version, PROTOCOL_VERSION);
        assert_eq!(connected.proof, proof);
        assert_eq!(connected.request_id, Some(1));
        assert!(matches!(
            connected.message,
            ServerMessage::Response {
                response: ServerResponse::Connected { .. }
            }
        ));
        write_frame(
            &mut peer,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id: 2,
                proof: Some(proof.clone()),
                request: ClientRequest::ChatRecoverAccount {
                    source: self.source.clone(),
                    replacement: self.replacement.clone(),
                    idle_generation: None,
                    execution_environment: Default::default(),
                    explicit,
                },
            },
        )
        .unwrap();
        loop {
            let reply: ServerEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(reply.version, PROTOCOL_VERSION);
            assert_eq!(reply.proof, proof);
            if reply.request_id != Some(2) {
                continue;
            }
            return match reply.message {
                ServerMessage::Response { response } => Ok(response),
                ServerMessage::Error(error) => Err(error),
                _ => panic!("the correlated host reply must settle the actual recovery request"),
            };
        }
    }
    fn assert_no_candidate(&self) {
        {
            let state = self.host.lock().unwrap();
            assert!(!state.chats.contains_key(&self.replacement.id));
            assert!(state.chat_spawning.is_empty());
        }
        assert!(!self.starts.exists());
        assert_eq!(std::fs::read(&self.target_transcript).unwrap(), b"{}\n");
    }
    fn register_old_owner(&mut self, pending: bool) {
        self.host.lock().unwrap().chats.remove(&self.source.id);
        self.source.proof.as_mut().unwrap().boot_id = "previous-fixture-boot".into();
        self.source.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        let mut dead = Process::read(std::process::id() as i32).unwrap();
        dead.start += 1;
        let owner = Holder {
            version: 4,
            build: None,
            kind: Kind::Native,
            application: crate::instance::APPLICATION_ID.into(),
            boot: self.source.proof.as_ref().unwrap().boot_id.clone(),
            terminal: self.source.id,
            holder: dead.clone(),
            root: dead,
        };
        let home = PathBuf::from(&self.source.launch.recipe.environment["CLAUDE_CONFIG_DIR"]);
        let metadata = std::fs::metadata(&home).unwrap();
        let scope = crate::account::scope::LaunchAccountScope {
            provider: Provider::Claude,
            canonical_home: home.canonicalize().unwrap(),
            device: metadata.dev(),
            inode: metadata.ino(),
            registered_id: None,
            purge_ids: vec![],
        };
        let registry = super::super::agent_execution::registry(&self.host).unwrap();
        let mut registry = registry.lock().unwrap();
        if pending {
            registry
                .register_pending_native_actor_ownership(
                    uuid::Uuid::new_v4(),
                    &owner,
                    &self.source,
                    Ok(&scope),
                )
                .unwrap();
        } else {
            registry
                .register_native_actor_ownership(
                    uuid::Uuid::new_v4(),
                    &owner,
                    &self.source,
                    Ok(&scope),
                )
                .unwrap();
        }
    }
    fn remove_original_home(&self) {
        std::fs::remove_dir_all(&self.source.launch.recipe.environment["CLAUDE_CONFIG_DIR"])
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let handles = self
            .host
            .lock()
            .unwrap()
            .chats
            .values()
            .map(|record| record.handle.clone())
            .collect::<Vec<_>>();
        for handle in handles {
            let _ = handle.close();
        }
        let _ = self.original.close();
    }
}

#[test]
fn saved_account_recovery_uses_completed_source_and_only_target_history() {
    if super::super::exit_drain_tests::private_runtime_case(
        "host::chat::saved_account_tests::saved_account_recovery_uses_completed_source_and_only_target_history", None,
    ).is_none() { return; }
    let fixture = Fixture::new(true);
    fixture.remove_original_home();
    let ServerResponse::ChatAttached { session, snapshot } = fixture.recover().unwrap() else {
        panic!("confirmed saved recovery")
    };
    assert_eq!(session.id, fixture.replacement.id);
    assert_eq!(snapshot.session_id.as_deref(), Some(THREAD));
    assert_eq!(snapshot.permission_mode.as_deref(), Some("plan"));
    assert_eq!(
        std::fs::read_to_string(fixture.directory.path().join("restored-mode")).unwrap(),
        "plan"
    );
    assert!(fixture.original.is_completed());
    assert!(
        !PathBuf::from(&fixture.source.launch.recipe.environment["CLAUDE_CONFIG_DIR"]).exists()
    );
    assert_eq!(std::fs::read(&fixture.target_transcript).unwrap(), b"{}\n");
}

#[test]
fn saved_account_recovery_requires_registered_previous_boot_provenance() {
    if super::super::exit_drain_tests::private_runtime_case(
        "host::chat::saved_account_tests::saved_account_recovery_requires_registered_previous_boot_provenance", None,
    ).is_none() { return; }
    let mut fixture = Fixture::new(true);
    fixture
        .host
        .lock()
        .unwrap()
        .chats
        .remove(&fixture.source.id);
    fixture.source.proof.as_mut().unwrap().boot_id = "invented-boot-without-owner".into();
    assert!(
        fixture
            .recover()
            .unwrap_err()
            .message
            .contains("no authenticated completed")
    );
    fixture.assert_no_candidate();
    fixture.register_old_owner(false);
    fixture.remove_original_home();
    let ServerResponse::ChatAttached { snapshot, .. } = fixture.recover().unwrap() else {
        panic!("registered old owner permits saved recovery")
    };
    assert_eq!(snapshot.session_id.as_deref(), Some(THREAD));
}

#[test]
fn saved_account_recovery_refuses_live_pending_unknown_and_capacity_without_effects() {
    if super::super::exit_drain_tests::private_runtime_case(
        "host::chat::saved_account_tests::saved_account_recovery_refuses_live_pending_unknown_and_capacity_without_effects", None,
    ).is_none() { return; }
    for case in [
        "live",
        "different-allocation",
        "source-pending",
        "replacement-pending",
        "terminal-start",
        "lineage-start",
        "retained-pending",
        "unknown",
        "capacity",
        "automatic-target",
        "stale-proof",
        "missing-pair",
        "live-intent-completed",
        "live-intent-absent",
    ] {
        let mut fixture = Fixture::new(!matches!(case, "live" | "different-allocation"));
        match case {
            "different-allocation" => fixture.source.id = TerminalId::new(),
            "source-pending" | "replacement-pending" => {
                let pending = if case == "source-pending" {
                    fixture.source.clone()
                } else {
                    fixture.replacement.clone()
                };
                fixture
                    .host
                    .lock()
                    .unwrap()
                    .chat_spawning
                    .insert(pending.id, pending);
            }
            "terminal-start" => fixture
                .host
                .lock()
                .unwrap()
                .reserve_spawn(fixture.replacement.id)
                .unwrap(),
            "lineage-start" => {
                let sibling = TerminalId::new();
                super::super::agent_execution::registry(&fixture.host)
                    .unwrap()
                    .lock()
                    .unwrap()
                    .succeed(sibling, fixture.source.id, None, |_| false)
                    .unwrap();
                fixture.host.lock().unwrap().reserve_spawn(sibling).unwrap();
            }
            "retained-pending" => fixture.register_old_owner(true),
            "stale-proof" => {
                fixture.source.proof.as_mut().unwrap().host_incarnation = "stale-source".into();
            }
            "missing-pair" => {
                let terminal = crate::model::TerminalSession {
                    id: fixture.source.id,
                    owned_codex_listener: None,
                    recovery_origin: None,
                    predecessor: None,
                    launch: fixture.source.launch.recipe.clone(),
                    transcript_path: None,
                    resume_hint: None,
                    continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                        recipe: fixture.source.launch.recipe.clone(),
                        last_exit: None,
                    },
                };
                fixture.source.terminal = Some(terminal);
                fixture
                    .host
                    .lock()
                    .unwrap()
                    .chats
                    .get_mut(&fixture.source.id)
                    .unwrap()
                    .session = fixture.source.clone();
            }
            "live-intent-absent" | "unknown" => {
                fixture
                    .host
                    .lock()
                    .unwrap()
                    .chats
                    .remove(&fixture.source.id);
            }
            "capacity" => {
                let mut state = fixture.host.lock().unwrap();
                state.native_chat_capacity = capacity::usage(&state);
            }
            "automatic-target" => {
                fixture
                    .replacement
                    .launch
                    .recipe
                    .environment
                    .insert(crate::account::routing::AUTOMATIC_MARKER.into(), "1".into());
            }
            _ => {}
        }
        let result = if case.starts_with("live-intent-") {
            fixture.request(true)
        } else {
            fixture.recover()
        };
        assert!(result.is_err(), "{case} must refuse before launch");
        // The admission must preserve somebody else's reservation as well.
        if matches!(case, "source-pending" | "replacement-pending") {
            let mut state = fixture.host.lock().unwrap();
            assert_eq!(
                state.chat_spawning.len(),
                1,
                "another admission remains owned"
            );
            state.chat_spawning.clear();
        }
        fixture.assert_no_candidate();
        if matches!(case, "live" | "different-allocation") {
            assert!(!fixture.original.is_completed());
        }
    }
}

#[test]
fn saved_account_recovery_retries_after_confirmed_candidate_failure_with_healthy_source_home() {
    if super::super::exit_drain_tests::private_runtime_case(
        "host::chat::saved_account_tests::saved_account_recovery_retries_after_confirmed_candidate_failure_with_healthy_source_home", None,
    ).is_none() { return; }
    {
        let not_ready = Fixture::with_ready(false, false);
        assert_eq!(
            not_ready.original.confirmed_session_id().as_deref(),
            Some(THREAD)
        );
        assert!(!not_ready.original.update_activity().1);
        assert!(
            confirm_account_conversation(&not_ready.original, THREAD, Instant::now(), || false)
                .is_err(),
            "exact thread identity cannot outrun initial readiness"
        );
    }
    let mut fixture = Fixture::new(true);
    std::fs::write(
        &fixture.confirmation,
        "98765432-1234-4234-8234-123456789abc",
    )
    .unwrap();
    let failure = fixture.recover().unwrap_err();
    assert!(
        failure.message.contains("different conversation") || failure.message.contains("confirm"),
        "{}",
        failure.message
    );
    assert!(
        !fixture
            .host
            .lock()
            .unwrap()
            .chats
            .contains_key(&fixture.replacement.id)
    );
    assert!(fixture.original.is_completed());
    std::fs::write(&fixture.confirmation, THREAD).unwrap();
    fixture.replacement.id = TerminalId::new();
    let ServerResponse::ChatAttached { snapshot, .. } = fixture.recover().unwrap() else {
        panic!("explicit stopped-source retry")
    };
    assert_eq!(snapshot.session_id.as_deref(), Some(THREAD));
    assert_eq!(
        std::fs::read_to_string(&fixture.starts)
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert!(fixture.directory.path().join("original").exists());
    drop(fixture);
    let mut unavailable_setup = Fixture::new(true);
    unavailable_setup.register_old_owner(false);
    std::fs::write(
        unavailable_setup.directory.path().join("fail-setup"),
        b"fail",
    )
    .unwrap();
    assert!(
        unavailable_setup.recover().is_err(),
        "identity-confirmed setup failure must not bind saved recovery"
    );
    assert!(
        !unavailable_setup
            .host
            .lock()
            .unwrap()
            .chats
            .contains_key(&unavailable_setup.replacement.id)
    );
    std::fs::remove_file(unavailable_setup.directory.path().join("fail-setup")).unwrap();
    unavailable_setup.replacement.id = TerminalId::new();
    assert!(matches!(
        unavailable_setup.recover().unwrap(),
        ServerResponse::ChatAttached { .. }
    ));
}
