//! A durable Codex pane whose daemon stopped in the ordinary way, so that its
//! holder closed with the daemon, reopens its exact thread under the next
//! daemon as a proof-checked successor (#1513).
use super::*;
use crate::{
    chat::{NativeContinuity, Session},
    model::{HostIncarnation, HostSessionId},
    terminal::ipc::{ClientRequest, ServerResponse},
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, process::Command, time::Duration};

const MODE: &str = "CAGENTS_CODEX_DURABLE_RESUME_TEST";
const THREAD: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const PREFIX: &str = "host::state::codex_durable_resume_tests::";
/// A Codex version whose holder is durable.
const VERIFIED: &str = "0.157.1";
const NAME: &str = "durable_codex_pane_reopens_its_thread_after_an_ordinary_daemon_stop";

#[test]
fn durable_codex_pane_reopens_its_thread_after_an_ordinary_daemon_stop() {
    match std::env::var(MODE).as_deref() {
        Ok("provider") => return provider(),
        Ok("fixture") => {}
        _ => return run_isolated(&format!("{PREFIX}{NAME}")),
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_UPGRADE_ROOT").unwrap());
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let daemon = |incarnation: &str| HostSessionProof {
        host_session_id: HostSessionId::new("codex-durable-resume"),
        host_incarnation: HostIncarnation::new(incarnation),
        boot_id: crate::daemon::read_boot_id().unwrap(),
    };
    let ownership = root.join("ownership.sqlite3");
    let program = install_provider(&root, NAME);
    let first = daemon("daemon-a");
    let lease = crate::daemon::HostLease::acquire(&paths, &first).unwrap();
    let mut state = HostState::new(first.clone());
    state.install_model(crate::host::model::Registry::at(&ownership).unwrap());
    let host = Arc::new(crate::host::state::HostLock::new(state));
    let source = provider_session(&root, &program, &first);
    let id = source.id;
    super::super::chat::start_execution(
        &host,
        0,
        source,
        Arc::new(crate::chat::protocol::ExecutionContext::default()),
    )
    .unwrap();
    let handle = host.lock().unwrap().chats[&id].handle.clone();
    wait_confirmed(&handle);
    // What the window saved: the hosted descriptor with its confirmed thread.
    let mut saved = host.lock().unwrap().chats[&id].session.clone();
    saved.launch.session_id = handle.session_id();
    saved.native_continuity = handle.native_continuity();
    assert!(
        matches!(saved.native_continuity, NativeContinuity::Durable { .. }),
        "{:?}",
        saved.native_continuity
    );
    assert!(saved.owns_codex_listener());
    super::super::finish_chat_shutdown(&host).unwrap();
    drop(handle);
    drop(host);
    drop(lease);
    assert!(
        registry_record(id).is_none_or(|record| !record.root.is_current()),
        "an ordinary stop closes the holder's server"
    );

    let second = daemon("daemon-b");
    let _lease = crate::daemon::HostLease::acquire(&paths, &second).unwrap();
    let mut state = HostState::new(second.clone());
    state.install_model(crate::host::model::Registry::at(&ownership).unwrap());
    state.register_client(1);
    let successor = Arc::new(crate::host::state::HostLock::new(state));
    successor.lock().unwrap().enable_holder_recovery();
    super::restore_orphaned_holders(&successor).unwrap();
    super::super::chat::restore_orphaned_native(&successor, &[]).unwrap();
    assert!(!successor.lock().unwrap().chats.contains_key(&id));
    let request = |request: ClientRequest| {
        super::super::client::process_request(&successor, 1, request)
            .map(|(response, _, _)| response)
    };
    // The requests `chat::client::restore` sends for this card.
    let refusal = request(ClientRequest::ResolveUpgradeChat {
        source: saved.clone(),
    })
    .unwrap_err();
    assert!(
        refusal
            .message
            .contains("durable native holder is unavailable"),
        "{}",
        refusal.message
    );
    let Ok(ServerResponse::ChatResumePrepared { session: prepared }) =
        request(ClientRequest::ChatPrepareResume {
            source: saved.clone(),
        })
    else {
        panic!("the closed holder's receipt proves the old owner ended");
    };
    assert!(
        crate::chat::is_retained_resume_successor(&saved, &prepared),
        "{prepared:?}"
    );
    let Ok(ServerResponse::ChatAttached {
        session: started, ..
    }) = request(ClientRequest::ChatStart {
        session: prepared.clone(),
        source: None,
        terminate_source: false,
        execution_environment: Default::default(),
    })
    else {
        panic!("the successor starts on the saved thread");
    };
    assert!(
        crate::chat::is_retained_resume_commit(&saved, &started),
        "{started:?}"
    );
    let handle = successor.lock().unwrap().chats[&started.id].handle.clone();
    wait_confirmed(&handle);
    assert_eq!(started.launch.session_id.as_deref(), Some(THREAD));
    let _ = handle.close();
    let requests = fs::read_to_string(root.join("provider-requests")).unwrap();
    let opened = requests
        .lines()
        .filter(|line| line.starts_with("thread/start") || line.starts_with("thread/resume"))
        .collect::<Vec<_>>();
    assert_eq!(
        opened,
        [
            "thread/start ".to_owned(),
            format!("thread/resume {THREAD}")
        ],
        "the successor resumes the exact thread once and resends nothing"
    );
}

fn registry_record(id: TerminalId) -> Option<crate::daemon::holder_registry::Record> {
    use crate::daemon::holder_registry::{self as registry, Kind};
    let socket = registry::socket(&registry::directory(Kind::Native).unwrap(), id);
    registry::evidence(&socket, Kind::Native).ok().flatten()
}

fn wait_confirmed(handle: &crate::chat::runtime::Handle) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while handle.confirmed_session_id().as_deref() != Some(THREAD)
        || handle.check_update_readiness() != Ok(crate::chat::runtime::UpdateReadiness::Idle)
    {
        assert!(
            Instant::now() < deadline,
            "provider did not confirm: failure={:?}",
            handle.failure()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn run_isolated(test: &str) {
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let root = directory.path();
    for path in [
        "home/.codex",
        "project",
        "run",
        "data",
        "config",
        "cache",
        "bin",
    ] {
        fs::create_dir_all(root.join(path)).unwrap();
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env_clear()
        .env(MODE, "fixture")
        .env("CAGENTS_UPGRADE_ROOT", root)
        .env("CAGENTS_UPGRADE_EXE", std::env::current_exe().unwrap())
        .env("CAGENTS_PROBE_TEST", test)
        .env("TMPDIR", root)
        .env("HOME", root.join("home"))
        .env("CODEX_HOME", root.join("home/.codex"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "probe failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn install_provider(root: &std::path::Path, name: &str) -> std::path::PathBuf {
    let program = root.join("bin/codex");
    fs::write(
        &program,
        format!(
            r#"#!/bin/sh
set -eu
for arg in "$@"; do
  case "$arg" in unix://*) export CAGENTS_UPGRADE_SOCKET="${{arg#unix://}}" ;; esac
done
echo launch >> "$CAGENTS_UPGRADE_ROOT/provider-launches"
export {MODE}=provider
exec "$CAGENTS_UPGRADE_EXE" --exact '{PREFIX}{name}' --nocapture
"#
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    program
}

fn provider_session(
    root: &std::path::Path,
    program: &std::path::Path,
    proof: &HostSessionProof,
) -> Session {
    use crate::{agent::Provider, chat::Launch, model::LaunchRecipe};
    let mut source = Session {
        id: TerminalId::new(),
        launch: Launch {
            provider: Provider::Codex,
            recipe: LaunchRecipe {
                program: program.to_string_lossy().into_owned(),
                arguments: vec!["--remote".into(), "unix://".into()],
                working_directory: root.join("project"),
                environment: std::env::vars().collect(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: None,
        terminal: None,
        owned_codex_listener: None,
        native_continuity: NativeContinuity::default(),
    };
    assert!(source.claim_owned_codex_listener(root, proof));
    source
}

fn provider() {
    let version = VERIFIED;
    let socket = std::env::var_os("CAGENTS_UPGRADE_SOCKET").unwrap();
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    for stream in listener.incoming() {
        let mut websocket = tungstenite::accept(stream.unwrap()).unwrap();
        while let Ok(message) = websocket.read() {
            if message.is_close() {
                break;
            }
            let Ok(text) = message.to_text() else {
                continue;
            };
            let frame: Value = serde_json::from_str(text).unwrap();
            let Some(id) = frame.get("id") else { continue };
            {
                use std::io::Write;
                let root = std::env::var_os("CAGENTS_UPGRADE_ROOT").unwrap();
                let mut log = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(std::path::Path::new(&root).join("provider-requests"))
                    .unwrap();
                let thread = frame["params"]["threadId"].as_str().unwrap_or_default();
                writeln!(
                    log,
                    "{} {thread}",
                    frame["method"].as_str().unwrap_or_default()
                )
                .unwrap();
            }
            let result = match frame["method"].as_str().unwrap() {
                "initialize" => json!({
                    "userAgent": format!("cagents/{version} (Linux)"),
                    "codexHome": std::env::var("CODEX_HOME").unwrap()
                }),
                "thread/start" | "thread/resume" => json!({
                    "thread": {"id": THREAD, "canAcceptDirectInput": true, "status": {"type": "idle"}},
                    "model": "fixture-model"
                }),
                "thread/read" => json!({"thread": {"id": THREAD, "turns": []}}),
                "thread/turns/list" | "thread/queue/list" | "thread/list" => {
                    json!({"data": [], "nextCursor": null})
                }
                "model/list" | "skills/list" => json!({"data": []}),
                method => panic!("unexpected provider request {method}"),
            };
            if websocket
                .send(tungstenite::Message::Text(
                    json!({"id": id, "result": result}).to_string().into(),
                ))
                .is_err()
            {
                break;
            }
        }
    }
}
