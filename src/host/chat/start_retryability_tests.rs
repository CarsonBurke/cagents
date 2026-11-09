//! Startup recovery through the real HOST dispatch and framed native client.
//! HostClientWire runs the actual client server, registration and delivery.
//! Provider fixtures are offline, receive no prompts, and run in a private home.
use super::*;
use crate::{chat::client::ClientHandle, model::LaunchRecipe};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command},
};

const THREAD: &str = "92bec561-2706-4e26-a34d-468f5a67148c";

fn isolated(name: &str) -> Option<PathBuf> {
    const MODE: &str = "CAGENTS_START_RETRYABILITY_TEST";
    if std::env::var(MODE).as_deref() == Ok(name) {
        return Some(
            std::env::var_os("CAGENTS_START_RETRYABILITY_ROOT")
                .unwrap()
                .into(),
        );
    }
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    for path in [
        "home/.codex",
        "home/.claude",
        "r",
        "data",
        "config",
        "cache",
        "bin",
        "project",
    ] {
        fs::create_dir_all(root.join(path)).unwrap();
    }
    fs::set_permissions(root.join("r"), fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("host::chat::start_retryability_tests::{name}"),
            "--nocapture",
        ])
        .env_clear()
        .envs(crate::retained_fixture::owner_env())
        .env(MODE, name)
        .env("CAGENTS_START_RETRYABILITY_ROOT", root)
        .env("HOME", root.join("home"))
        .env("CODEX_HOME", root.join("home/.codex"))
        .env("XDG_RUNTIME_DIR", root.join("r"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .stdout(fs::File::create(root.join("stdout")).unwrap())
        .stderr(fs::File::create(root.join("stderr")).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        status.is_some_and(|status| status.success()),
        "isolated startup fixture failed or timed out:\n{}\n{}",
        fs::read_to_string(root.join("stdout")).unwrap(),
        fs::read_to_string(root.join("stderr")).unwrap(),
    );
    None
}

fn setup(root: &Path, name: &str) -> (SharedHost, HostSessionProof) {
    let proof = HostSessionProof {
        host_session_id: name.into(),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: "startup-retry".into(),
    };
    let mut state = HostState::new(proof.clone());
    state.enable_holder_recovery();
    state.install_model(super::super::model::Registry::at(&root.join("registry.sqlite3")).unwrap());
    (Arc::new(HostLock::new(state)), proof)
}

fn session(root: &Path, provider: Provider, arguments: Vec<String>) -> Session {
    Session {
        id: TerminalId::new(),
        proof: None,
        terminal: None,
        native_continuity: Default::default(),
        owned_codex_listener: None,
        launch: chat::Launch {
            provider,
            session_id: Some(THREAD.into()),
            recipe: LaunchRecipe {
                program: root
                    .join("bin")
                    .join(provider.executable())
                    .to_str()
                    .unwrap()
                    .into(),
                arguments,
                working_directory: root.join("project"),
                environment: BTreeMap::from([
                    ("HOME".into(), root.join("home").to_str().unwrap().into()),
                    (
                        "CODEX_HOME".into(),
                        root.join("home/.codex").to_str().unwrap().into(),
                    ),
                    ("PATH".into(), "/usr/bin:/bin".into()),
                ]),
                unset_environment: Default::default(),
            },
        },
    }
}

fn connect(session: &Session, intent: chat::Intent) -> Result<ClientHandle, chat::StartFailure> {
    ClientHandle::connect(session.clone(), intent, None, false, None, |_| {})
}

fn refused(session: &Session) -> chat::StartFailure {
    match connect(session, chat::Intent::Start) {
        Ok(handle) => {
            let _ = handle.close();
            panic!("the real daemon unexpectedly accepted the invalid start")
        }
        Err(failure) => failure,
    }
}

fn wait_for_ready(host: &SharedHost, source: &Session, client: &ClientHandle) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let handle = host.lock().unwrap().chats[&source.id].handle.clone();
        let snapshot = handle.attachment_snapshot();
        assert!(!snapshot.exited, "provider exited: {:?}", snapshot.error);
        if snapshot.ready {
            assert_eq!(snapshot.session_id, source.launch.session_id);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "provider did not become ready: {:?}",
            snapshot.error
        );
        thread::sleep(Duration::from_millis(10));
    }
    // Full HOST delivery sends the visibility edge that makes the client
    // request an authoritative unconditional snapshot on reveal.
    client.set_visible(false).unwrap();
    client.set_visible(true).unwrap();
    loop {
        if client
            .resident_snapshot_with_clock()
            .is_some_and(|(snapshot, _)| snapshot.ready)
        {
            assert_eq!(client.snapshot().session_id, source.launch.session_id);
            assert_eq!(client.session().id, source.id);
            assert_eq!(client.session().launch, source.launch);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the native client did not hydrate its exact ready conversation"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn executable_permission_repair_retries_the_same_saved_conversation() {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    let Some(root) = isolated("executable_permission_repair_retries_the_same_saved_conversation")
    else {
        return;
    };
    let provider = root.join("bin/omp");
    fs::write(
        &provider,
        r#"#!/usr/bin/python3
import json, pathlib, sys
root = pathlib.Path(__file__).resolve().parent.parent
args = sys.argv[1:]
session = args[args.index('--resume') + 1]
with (root / 'launches').open('a') as log:
    log.write(session + '\n')
print(json.dumps({'type': 'ready'}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    kind = request['type']
    assert kind not in ('prompt', 'steer', 'follow_up'), 'the fixture never submits input'
    data = {}
    if kind == 'get_state':
        data = {'sessionId': session, 'isStreaming': False, 'isCompacting': False}
    elif kind == 'get_messages_page':
        data = {'messages': [], 'nextCursor': None}
    print(json.dumps({'type': 'response', 'id': request['id'], 'command': kind,
                     'success': True, 'data': data}), flush=True)
"#,
    )
    .unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o600)).unwrap();
    let source = session(&root, Provider::Omp, vec![]);
    let (host, proof) = setup(&root, "permission-retry");
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let _wire = super::retained_admission_tests::HostClientWire::start_native(&host, &proof);
    let failure = refused(&source);
    assert_eq!(
        failure.recovery,
        chat::Recovery::Retry,
        "{}",
        failure.message
    );
    assert!(
        failure.message.contains("not executable"),
        "{}",
        failure.message
    );
    assert!(!root.join("launches").exists());
    assert!(host.lock().unwrap().chat_spawning.is_empty());
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    let client = connect(&source, chat::Intent::RetryStart).unwrap();
    wait_for_ready(&host, &source, &client);
    assert_eq!(
        fs::read_to_string(root.join("launches")).unwrap(),
        format!("{THREAD}\n")
    );
    client.close().unwrap();
    assert!(host.lock().unwrap().chats.is_empty());
}

#[test]
fn codex_recipe_thread_conflict_is_blocked_without_launching_a_provider() {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    let Some(root) =
        isolated("codex_recipe_thread_conflict_is_blocked_without_launching_a_provider")
    else {
        return;
    };
    let provider = root.join("bin/codex");
    fs::write(
        &provider,
        "#!/bin/sh\nprintf 'unexpected launch\\n' > \"$HOME/../launches\"\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    let source = session(
        &root,
        Provider::Codex,
        vec!["resume".into(), uuid::Uuid::new_v4().to_string()],
    );
    let (host, proof) = setup(&root, "recipe-conflict");
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let _wire = super::retained_admission_tests::HostClientWire::start_native(&host, &proof);
    let failure = refused(&source);
    assert_eq!(
        failure.recovery,
        chat::Recovery::Blocked,
        "{}",
        failure.message
    );
    assert_eq!(
        failure.message,
        "Codex recipe resume ID does not match the selected thread"
    );
    assert!(!root.join("launches").exists());
    assert!(host.lock().unwrap().chats.is_empty());
    assert!(host.lock().unwrap().chat_spawning.is_empty());
}

struct FixtureServer(Child);
impl Drop for FixtureServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn invalid_resume_continuity_stays_blocked_through_prepare_handshake() {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    let Some(root) = isolated("invalid_resume_continuity_stays_blocked_through_prepare_handshake")
    else {
        return;
    };
    let (host, proof) = setup(&root, "resume-conflict");
    let mut source = session(&root, Provider::Claude, vec![]);
    source.proof = Some(proof.clone());
    source.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 999 };
    assert!(!source.has_valid_native_continuity());
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let _wire = super::retained_admission_tests::HostClientWire::start_native(&host, &proof);
    let failure = match connect(&source, chat::Intent::Resume) {
        Ok(handle) => {
            let _ = handle.close();
            panic!("invalid retained continuity unexpectedly started")
        }
        Err(failure) => failure,
    };
    assert_eq!(
        failure.recovery,
        chat::Recovery::Blocked,
        "{}",
        failure.message
    );
    assert!(
        failure
            .message
            .contains("Saved conversation has invalid continuity evidence"),
        "the real preparation refusal must reach the client: {}",
        failure.message,
    );
    assert!(host.lock().unwrap().chats.is_empty());
    assert!(host.lock().unwrap().chat_spawning.is_empty());
    assert!(host.lock().unwrap().native_actor_startups.is_empty());
}

#[test]
fn missing_claude_history_repair_restores_the_same_read_only_card_without_launching() {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    let Some(root) = isolated(
        "missing_claude_history_repair_restores_the_same_read_only_card_without_launching",
    ) else {
        return;
    };
    let provider = root.join("bin/claude");
    fs::write(
        &provider,
        "#!/bin/sh\nprintf 'unexpected launch\\n' > \"$HOME/../launches\"\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    let (host, proof) = setup(&root, "history-repair");
    let mut source = session(&root, Provider::Claude, vec![]);
    source.proof = Some(proof.clone());
    assert!(source.is_saved_claude_history());
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let _wire = super::retained_admission_tests::HostClientWire::start_native(&host, &proof);
    let failure = match connect(&source, chat::Intent::Restore) {
        Ok(handle) => {
            let _ = handle.close();
            panic!("missing saved history unexpectedly attached")
        }
        Err(failure) => failure,
    };
    assert_eq!(
        failure.recovery,
        chat::Recovery::Retry,
        "{}",
        failure.message
    );
    assert!(!root.join("launches").exists());
    assert!(host.lock().unwrap().chats.is_empty());
    assert!(host.lock().unwrap().chat_spawning.is_empty());

    let project_key: String = source
        .launch
        .recipe
        .working_directory
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
        .collect();
    let transcript = root
        .join("home/.claude/projects")
        .join(project_key)
        .join(format!("{THREAD}.jsonl"));
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let original = serde_json::json!({
        "type": "user",
        "uuid": "original-history-item",
        "parentUuid": null,
        "sessionId": THREAD,
        "message": {"role": "user", "content": "The exact saved conversation remains readable"},
    })
    .to_string()
        + "\n";
    fs::write(&transcript, &original).unwrap();
    // This remains the cold-card Restore flow. RetryStart would grant a
    // different provider-launch operation and would not prove this recovery.
    let client = connect(&source, chat::Intent::Restore).unwrap();
    client.set_visible(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = client.snapshot();
        if snapshot.exited && snapshot.read_only_reason.is_some()
            && snapshot.items.iter().any(|item| matches!(&item.content,
                chat::ItemContent::Text { text, .. } if text.contains("The exact saved conversation remains readable")))
        {
            assert_eq!(snapshot.session_id, source.launch.session_id);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the repaired cold card did not hydrate its exact saved history"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(client.session(), source);
    assert!(
        host.lock().unwrap().chats[&source.id]
            .handle
            .is_read_only_history()
    );
    assert!(!root.join("launches").exists());
    assert_eq!(fs::read_to_string(&transcript).unwrap(), original);
    client.close().unwrap();
    assert!(host.lock().unwrap().chats.is_empty());
}

#[test]
fn codex_endpoint_repair_retries_the_same_socket_and_thread() {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    let Some(root) = isolated("codex_endpoint_repair_retries_the_same_socket_and_thread") else {
        return;
    };
    let provider = root.join("bin/codex");
    fs::write(
        &provider,
        include_str!("../../../tests/fixtures/terminal-escape-codex.py"),
    )
    .unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = root.join("r/peer.sock");
    let address = format!("unix://{}", socket.display());
    let source = session(
        &root,
        Provider::Codex,
        vec!["--remote".into(), address.clone()],
    );
    let (host, proof) = setup(&root, "endpoint-retry");
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let _wire = super::retained_admission_tests::HostClientWire::start_native(&host, &proof);
    let failure = refused(&source);
    assert_eq!(
        failure.recovery,
        chat::Recovery::Retry,
        "{}",
        failure.message
    );
    assert!(
        failure
            .message
            .contains("Cannot join existing Codex daemon"),
        "{}",
        failure.message
    );
    assert!(
        !root.join("provider.json").exists(),
        "the failed join must not launch a server"
    );
    let mut server = FixtureServer(
        Command::new("/usr/bin/python3")
            .arg(&provider)
            .args(["--listen", &address])
            .env_clear()
            .env("CAGENTS_ESCAPE_FIXTURE_ROOT", &root)
            .env("CODEX_HOME", root.join("home/.codex"))
            .env("XDG_RUNTIME_DIR", root.join("r"))
            .stdout(fs::File::create(root.join("peer.stdout")).unwrap())
            .stderr(fs::File::create(root.join("peer.stderr")).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "fixture server exited: {}",
            fs::read_to_string(root.join("peer.stderr")).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "fixture server never bound its socket"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let client = connect(&source, chat::Intent::RetryStart).unwrap();
    wait_for_ready(&host, &source, &client);
    let requests = fs::read_to_string(root.join("rpc-intake.jsonl")).unwrap();
    let requests: Vec<serde_json::Value> = requests
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        requests
            .iter()
            .any(|request| request["method"] == "thread/resume"
                && request["params"]["threadId"] == THREAD)
    );
    assert!(
        requests
            .iter()
            .all(|request| request["method"] != "turn/start")
    );
    client.close().unwrap();
    assert!(host.lock().unwrap().chats.is_empty());
}
