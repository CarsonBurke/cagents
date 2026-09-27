use super::*;
use crate::{
    agent::Provider,
    agent_execution::{Operation, Reply, Target},
    host::state::HostState,
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, ProjectKind},
    workspace_model::ModelEdit,
};
use std::{os::unix::fs::PermissionsExt, path::Path};

// Environment isolation also proves the service does not depend on any user's
// account, project, provider installation, or running daemon.
fn isolated(name: &str) -> Option<tempfile::TempDir> {
    const CHILD: &str = "CAGENTS_EXECUTION_SERVICE_TEST";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return None;
    }
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    for path in [
        "bin",
        "home/.claude",
        "home/.codex",
        "data",
        "config",
        "cache",
        "tmp",
        "project",
    ] {
        std::fs::create_dir_all(root.join(path)).unwrap();
    }
    install_claude(&root.join("bin/claude"));
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("host::agent_execution::tests::{name}"),
            "--nocapture",
        ])
        .env_clear()
        .env(CHILD, name)
        .env("CAGENTS_EXECUTION_FIXTURE", root)
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root.join("tmp"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .status()
        .unwrap();
    assert!(result.success(), "isolated service fixture failed");
    Some(directory)
}

fn install_claude(path: &Path) {
    std::fs::write(path, r#"#!/bin/sh
set -eu
printf 'spawn\n' >> "$HOME/../spawns"
printf '%s\n' "$@" > "$HOME/../claude-arguments"
n=0
session=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa
while [ "$#" -gt 0 ]; do
  case "$1" in
    --session-id|--resume) shift; session=$1 ;;
    --session-id=*|--resume=*) session=${1#*=} ;;
  esac
  shift
done
while IFS= read -r frame; do
  case "$frame" in
    *'"type":"control_request"'*)
      id=${frame#*'"request_id":"'}
      id=${id%%'"'*}
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      ;;
    *'"type":"user"'*)
      n=$((n + 1))
      printf 'prompt\n' >> "$HOME/../prompts"
      printf '{"type":"assistant","uuid":"message-%s","message":{"id":"message-%s","role":"assistant","content":[{"type":"text","text":"SERVICE_RESULT_%s"}],"usage":{"input_tokens":7,"output_tokens":3}}}\n' "$n" "$n" "$n"
      printf '{"type":"result","uuid":"result-%s","subtype":"success","is_error":false,"session_id":"%s","usage":{"input_tokens":7,"output_tokens":3}}\n' "$n" "$session"
      ;;
  esac
done
"#).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn fixture() -> (SharedHost, Submit, std::path::PathBuf) {
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_EXECUTION_FIXTURE").unwrap());
    let path = root.join("registry.sqlite3");
    let mut registry = Registry::at(&path).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: root.join("project"),
            name: "Service fixture".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("execution-fixture"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    });
    state.install_model(registry);
    let request = Submit {
        destination: Destination::New(Spec {
            provider: Provider::Claude,
            account: None,
            target: Target {
                repository: root.join("project"),
                directory: root.join("project"),
                channel: None,
            },
            surface: Surface::Chat,
            model: None,
            permission_mode: None,
        }),
        prompt: "Read the owned fixture".into(),
        idempotency_key: "first".into(),
    };
    (Arc::new(Mutex::new(state)), request, path)
}

fn settled(host: &SharedHost, run: RunId) -> Run {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let run = registry(host)
            .unwrap()
            .lock()
            .unwrap()
            .agent_run(run)
            .unwrap();
        if run.state.is_terminal() {
            return run;
        }
        assert!(Instant::now() < deadline, "run never settled: {run:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn committed_native_dispatch_is_idempotent_and_children_have_real_scoped_authority() {
    if isolated("committed_native_dispatch_is_idempotent_and_children_have_real_scoped_authority")
        .is_some()
    {
        return;
    }
    let (host, mut request, path) = fixture();
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.model = Some("fixture-model".into());
    spec.permission_mode = Some("plan".into());
    let accepted = submit(&host, &Authority::default(), request.clone()).unwrap();
    // A second connection sees the accepted durable identity independently of
    // provider progress; no child is launched inside the acceptance transaction.
    assert_eq!(
        Registry::at(&path)
            .unwrap()
            .agent_run(accepted.run.id)
            .unwrap()
            .agent,
        accepted.agent.id
    );
    let result = settled(&host, accepted.run.id);
    assert_eq!(result.state, State::Succeeded, "{result:?}");
    assert_eq!(result.result.text, "SERVICE_RESULT_1");
    assert!(result.provider_key.is_some());
    let retry = submit(&host, &Authority::default(), request.clone()).unwrap();
    assert!(!retry.created);
    assert_eq!(retry.run.id, accepted.run.id);
    let root = path.parent().unwrap();
    let arguments = std::fs::read_to_string(root.join("claude-arguments")).unwrap();
    assert!(arguments.contains("--model\nfixture-model\n"));
    assert!(arguments.contains("--permission-mode\nplan\n"));
    assert_eq!(
        std::fs::read_to_string(root.join("spawns")).unwrap(),
        "spawn\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("prompts")).unwrap(),
        "prompt\n"
    );

    let parent = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap();
    let authority = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .retained_agent_authority(parent.session.as_ref().unwrap().id())
        .unwrap();
    let mut child_request = request;
    child_request.idempotency_key = "child".into();
    let child = submit(&host, &authority, child_request).unwrap();
    assert_eq!(
        child.agent.origin,
        Origin::Child {
            parent: parent.session.as_ref().unwrap().id()
        }
    );
    assert_eq!(settled(&host, child.run.id).state, State::Succeeded);
    assert!(status(&host, &Authority::default(), child.agent.id).is_err());
    assert!(close(&host, &Authority::default(), child.agent.id).is_err());
    assert!(close(&host, &authority, child.agent.id).unwrap().closed);

    // An ordinary UI ChatClose uses the same retirement transaction as the
    // execution CLI. Afterwards its capability cannot create another child.
    let parent_session = parent.session.as_ref().unwrap().id();
    super::super::chat::process(
        &host,
        EXECUTION_CLIENT,
        ClientRequest::ChatClose {
            chat_id: parent_session,
        },
    )
    .unwrap();
    assert!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(parent.id)
            .unwrap()
            .closed
    );
    assert!(origin(&host, &authority).is_err());
    let Reply::Listed(agents) = process(&host, Authority::default(), Operation::List).unwrap()
    else {
        panic!()
    };
    assert!(agents.is_empty());
}

#[test]
fn fresh_codex_execution_uses_owned_listener_and_correlated_provider_result() {
    if isolated("fresh_codex_execution_uses_owned_listener_and_correlated_provider_result")
        .is_some()
    {
        return;
    }
    use serde_json::{Value, json};
    let (host, mut request, path) = fixture();
    let root = path.parent().unwrap().to_owned();
    let program = root.join("bin/codex");
    std::fs::write(&program, "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > \"$HOME/../codex-arguments\"\nexec /bin/sleep 60\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.provider = Provider::Codex;
    spec.model = Some("fixture-model".into());
    spec.permission_mode = Some("on-request".into());
    let server_root = root.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let socket = loop {
            if let Ok(arguments) = std::fs::read_to_string(server_root.join("codex-arguments"))
                && let Some(socket) = arguments
                    .lines()
                    .find_map(|arg| arg.strip_prefix("unix://"))
            {
                assert!(arguments.lines().any(|arg| arg == "app-server"));
                assert!(arguments.lines().any(|arg| arg == "--listen"));
                break std::path::PathBuf::from(socket);
            }
            assert!(Instant::now() < deadline, "Codex server was not launched");
            std::thread::sleep(Duration::from_millis(10));
        };
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "native adapter did not join owned listener"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        let mut prompts = 0;
        loop {
            let message = match websocket.read() {
                Ok(message) => message,
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    break;
                }
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                    ) =>
                {
                    break;
                }
                Err(tungstenite::Error::Protocol(
                    tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                )) => break,
                Err(error) => panic!("fixture websocket: {error}"),
            };
            if message.is_close() {
                break;
            }
            let frame: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            let Some(id) = frame.get("id") else { continue };
            let method = frame["method"].as_str().unwrap();
            let result = match method {
                "initialize" => {
                    json!({"userAgent":"codex/0.157.1", "codexHome":server_root.join("home/.codex")})
                }
                "thread/start" => {
                    assert_eq!(frame["params"]["model"], "fixture-model");
                    assert_eq!(frame["params"]["approvalPolicy"], "on-request");
                    json!({"thread":{"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","canAcceptDirectInput":true,"status":{"type":"idle"}},"model":"fixture-model"})
                }
                "model/list" | "skills/list" => json!({"data":[]}),
                "turn/start" => {
                    prompts += 1;
                    json!({"turn":{"id":"owned-turn"}})
                }
                "thread/read" => {
                    json!({"thread":{"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","turns":[]}})
                }
                method => panic!("unexpected Codex request {method}"),
            };
            websocket
                .send(tungstenite::Message::Text(
                    json!({"id":id,"result":result}).to_string().into(),
                ))
                .unwrap();
            if method == "turn/start" {
                websocket.send(tungstenite::Message::Text(json!({"method":"turn/completed","params":{"threadId":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","turn":{"id":"owned-turn","status":"completed","items":[{"id":"answer","type":"agentMessage","text":"OWNED_CODEX_RESULT"}]}}}).to_string().into())).unwrap();
            }
        }
        prompts
    });
    let accepted = submit(&host, &Authority::default(), request.clone()).unwrap();
    let run = settled(&host, accepted.run.id);
    assert_eq!(run.state, State::Succeeded, "{run:?}");
    assert_eq!(run.result.text, "OWNED_CODEX_RESULT");
    let (agent, _) = status(&host, &Authority::default(), accepted.agent.id).unwrap();
    let Some(HostedSession::Chat(session)) = agent.session else {
        panic!()
    };
    assert!(session.owned_codex_listener.is_some());
    assert!(session.owns_codex_listener());
    assert!(
        session
            .launch
            .recipe
            .arguments
            .iter()
            .any(|argument| argument.starts_with("unix://") && argument != "unix://")
    );
    assert!(
        !submit(&host, &Authority::default(), request)
            .unwrap()
            .created
    );
    close(&host, &Authority::default(), accepted.agent.id).unwrap();
    assert_eq!(server.join().unwrap(), 1);
}

#[test]
fn failed_post_spawn_binding_retires_exact_provider_without_losing_launch_identity() {
    if isolated("failed_post_spawn_binding_retires_exact_provider_without_losing_launch_identity")
        .is_some()
    {
        return;
    }
    let (host, request, path) = fixture();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_binding BEFORE UPDATE OF record ON agents
         WHEN json_extract(NEW.record, '$.session') IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'injected descriptor write failure'); END;",
        )
        .unwrap();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    let run = settled(&host, accepted.run.id);
    assert_eq!(run.state, State::Cancelled, "{run:?}");
    assert!(run.provider_key.is_none(), "no prompt reached the provider");
    assert!(
        run.result
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("no prompt was submitted")
    );
    let registry = registry(&host).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .agent(accepted.agent.id)
            .unwrap()
            .closed
    );
    assert!(
        registry
            .lock()
            .unwrap()
            .pending_agent_launch(accepted.agent.id)
            .unwrap()
            .is_none()
    );
    assert!(
        host.lock().unwrap().chats.is_empty(),
        "exact provider teardown precedes retirement"
    );
    assert!(!path.parent().unwrap().join("prompts").exists());
}

#[test]
fn unavailable_console_keeps_failed_idle_provider_bound_and_explicitly_closeable() {
    if isolated("unavailable_console_keeps_failed_idle_provider_bound_and_explicitly_closeable")
        .is_some()
    {
        return;
    }
    // The isolated unit-test executable has no matched CLI sibling. Exercise
    // the actual allocation failure without spawning any user installation.
    assert!(
        !std::env::current_exe()
            .unwrap()
            .with_file_name("cagents")
            .exists()
    );
    let (host, mut request, path) = fixture();
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.surface = Surface::Terminal;
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    let run = settled(&host, accepted.run.id);
    assert_eq!(run.state, State::Failed, "{run:?}");
    assert!(
        run.result
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("matched cagents CLI")
    );
    let (agent, _) = status(&host, &Authority::default(), accepted.agent.id).unwrap();
    assert!(agent.session.is_some());
    assert!(agent.console.is_none());
    assert!(!agent.closed);
    let repeated = submit(
        &host,
        &Authority::default(),
        Submit {
            destination: Destination::Existing(agent.id),
            prompt: "A follow-up".into(),
            idempotency_key: "second".into(),
        },
    )
    .unwrap();
    assert_eq!(settled(&host, repeated.run.id).state, State::Failed);
    assert_eq!(
        std::fs::read_to_string(path.parent().unwrap().join("spawns")).unwrap(),
        "spawn\n",
        "retry keeps the same actor"
    );
    assert!(!path.parent().unwrap().join("prompts").exists());
    assert!(
        close(&host, &Authority::default(), agent.id)
            .unwrap()
            .closed
    );
    assert!(host.lock().unwrap().chats.is_empty());
}

#[test]
fn rejected_pre_spawn_launch_clears_private_intent_and_can_be_closed() {
    if isolated("rejected_pre_spawn_launch_clears_private_intent_and_can_be_closed").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    std::fs::remove_file(path.parent().unwrap().join("bin/claude")).unwrap();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    let run = settled(&host, accepted.run.id);
    assert_eq!(run.state, State::Failed, "{run:?}");
    assert!(run.provider_key.is_none());
    assert!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .pending_agent_launch(accepted.agent.id)
            .unwrap()
            .is_none()
    );
    assert!(host.lock().unwrap().chats.is_empty());
    assert!(
        close(&host, &Authority::default(), accepted.agent.id)
            .unwrap()
            .closed
    );
    assert!(!path.parent().unwrap().join("spawns").exists());
}

#[test]
fn replaced_scheduled_directory_refuses_dispatch_before_spawning() {
    if isolated("replaced_scheduled_directory_refuses_dispatch_before_spawning").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    let directory = path.parent().unwrap().join("project");
    let held = Arc::new(std::fs::File::open(&directory).unwrap());
    let accepted = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .submit_agent(&Origin::User, &request, now())
        .unwrap();
    std::fs::rename(&directory, directory.with_file_name("original-project")).unwrap();
    std::fs::create_dir(&directory).unwrap();
    dispatch_accepted_in_directory(&host, accepted.clone(), held).unwrap();
    let run = settled(&host, accepted.run.id);
    assert_eq!(run.state, State::Failed);
    assert!(run.result.diagnostic.unwrap().contains("directory changed"));
    assert!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .pending_agent_launch(accepted.agent.id)
            .unwrap()
            .is_none()
    );
    assert!(!path.parent().unwrap().join("spawns").exists());
    assert!(
        close(&host, &Authority::default(), accepted.agent.id)
            .unwrap()
            .closed
    );
}

#[test]
fn changed_target_refuses_new_work_but_keeps_interrupt_and_close_available() {
    if isolated("changed_target_refuses_new_work_but_keeps_interrupt_and_close_available").is_some()
    {
        return;
    }
    let (host, request, path) = fixture();
    let directory = path.parent().unwrap().join("project");
    let held = Arc::new(std::fs::File::open(&directory).unwrap());
    let accepted = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .submit_agent(&Origin::User, &request, now())
        .unwrap();
    dispatch_accepted_in_directory(&host, accepted.clone(), held).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let (agent, _) = status(&host, &Authority::default(), accepted.agent.id).unwrap();
    let handle = host
        .lock()
        .unwrap()
        .chats
        .get(&agent.session.as_ref().unwrap().id())
        .unwrap()
        .handle
        .clone();
    std::fs::rename(&directory, directory.with_file_name("original-project")).unwrap();
    std::fs::create_dir(&directory).unwrap();
    let error = handle
        .command(Command::Prompt {
            text: "Must not run in replacement".into(),
            attachments: Vec::new(),
            mode: PromptMode::Send,
        })
        .unwrap_err();
    assert!(error.contains("directory changed"));
    handle.command(Command::Interrupt).unwrap();
    assert!(
        close(&host, &Authority::default(), agent.id)
            .unwrap()
            .closed
    );
    assert_eq!(
        std::fs::read_to_string(path.parent().unwrap().join("prompts")).unwrap(),
        "prompt\n"
    );
}
