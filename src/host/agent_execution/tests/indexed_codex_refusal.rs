//! Offline real HOST/actor/Codex flow, enclosed by the existing exact-family controller.
use super::*;
use serde_json::{Value, json};
use std::os::unix::net::{UnixListener, UnixStream};

fn send(socket: &mut tungstenite::WebSocket<UnixStream>, frame: Value) {
    socket.send(tungstenite::Message::Text(frame.to_string().into())).unwrap();
}

fn approval(id: u64) -> Value {
    json!({"id":id,"method":"item/commandExecution/requestApproval","params":{
        "threadId":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","turnId":"owned-turn",
        "itemId":format!("background-tool-{id}"),"command":"echo never execute",
        "availableDecisions":if id == 92 { json!(["decline","accept"]) } else { json!(["accept","decline"]) }
    }})
}

pub(super) fn body() {
    // This test-worker WebSocket exercises admission, not holder ancestry or
    // durable continuity. Never advertise a verified holder-owned app-server.
    assert!(chat::native_holder::verified_codex_version("codex/0.0.0").is_none());
    let (host, mut request, path) = fixture();
    let root = path.parent().unwrap().to_owned();
    let program = root.join("bin/codex");
    std::fs::write(&program, "#!/bin/sh\nset -eu\nprintf 'spawn\\n' >> \"$HOME/../codex-spawns\"\nprintf '%s\\n' \"$@\" > \"$HOME/../codex-arguments\"\nexec /bin/sleep 60\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let Destination::New(spec) = &mut request.destination else { unreachable!() };
    spec.provider = Provider::Codex;
    spec.model = Some("fixture-model".into());
    spec.permission_mode = Some("on-request".into());
    let server_root = root.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let socket = loop {
            if let Ok(arguments) = std::fs::read_to_string(server_root.join("codex-arguments"))
                && let Some(socket) = arguments.lines().find_map(|arg| arg.strip_prefix("unix://"))
            {
                break std::path::PathBuf::from(socket);
            }
            assert!(Instant::now() < deadline, "Codex fixture did not start");
            std::thread::sleep(Duration::from_millis(10));
        };
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "actor did not join the exact listener");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut websocket = tungstenite::accept(stream).unwrap();
        let mut decisions = Vec::new();
        let mut prompts = 0;
        loop {
            let message = match websocket.read() {
                Ok(message) => message,
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
                Err(tungstenite::Error::Io(error)) if matches!(error.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset) => break,
                Err(tungstenite::Error::Protocol(tungstenite::error::ProtocolError::ResetWithoutClosingHandshake)) => break,
                Err(error) => panic!("fixture websocket: {error}"),
            };
            if message.is_close() { break }
            let frame: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            let Some(id) = frame.get("id") else { continue };
            let Some(method) = frame["method"].as_str() else {
                assert_eq!(frame["result"]["decision"], "decline", "settled acceptance reached provider");
                assert!(decisions.len() < 2, "takeover bypass reached provider");
                assert_eq!(id, &json!(91 + decisions.len()));
                decisions.push(frame.clone());
                send(&mut websocket, approval(91 + decisions.len() as u64));
                continue;
            };
            let result = match method {
                "initialize" => json!({"userAgent":"codex/0.0.0","codexHome":server_root.join("home/.codex")}),
                "thread/start" => json!({"thread":{"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","canAcceptDirectInput":true,"status":{"type":"idle"}},"model":"fixture-model"}),
                "model/list" | "skills/list" | "thread/queue/list" | "thread/list" => json!({"data":[],"nextCursor":null}),
                "thread/read" => json!({"thread":{"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","turns":[]}}),
                "turn/start" => { prompts += 1; json!({"turn":{"id":"owned-turn"}}) }
                method => panic!("unexpected Codex request {method}"),
            };
            send(&mut websocket, json!({"id":id,"result":result}));
            if method == "turn/start" {
                send(&mut websocket, json!({"method":"turn/completed","params":{"threadId":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","turn":{"id":"owned-turn","status":"completed","items":[{"id":"answer","type":"agentMessage","text":"INDEXED_PARENT_DONE"}]}}}));
                send(&mut websocket, approval(91));
            }
        }
        assert_eq!(prompts, 1, "refusal must not start another managed turn");
        decisions
    });
    let owner = Authority::default();
    let accepted = submit(&host, &owner, request).unwrap();
    let agent = accepted.agent.id;
    let answer = |id: &str, action: &str| Control::Answer { request_id:id.into(), action:action.into(), answers:Vec::new() };
    let control = |authority: &Authority, command| -> Result<Vec<chat::Request>, String> {
        match process(&host, authority.clone(), Operation::Control { agent, command })? {
            Reply::Requests(requests) => Ok(requests),
            reply => panic!("{reply:?}"),
        }
    };
    let run = settled(&host, accepted.run.id);
    assert_eq!(run.state, State::Succeeded);
    assert_eq!(run.result.text, "INDEXED_PARENT_DONE");
    let asked = pending_requests(&host, agent);
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].id, "91");
    assert_eq!(asked[0].actions.iter().map(|choice| choice.id.as_str()).collect::<Vec<_>>(), vec!["decision-0", "decision-1"]);
    for command in [answer("91", "decision-0"), Control::Interrupt { including_subagents:true }] {
        let error = control(&owner, command).unwrap_err();
        assert!(error.contains("no active run"), "{error}");
    }
    assert!(control(&owner, answer("91", "decline")).unwrap_err().contains("accepts the actions"));
    assert!(control(&owner, answer("missing", "decision-1")).unwrap_err().contains("No pending request"));
    let (current, _) = status(&host, &owner, agent).unwrap();
    let Some(HostedSession::Chat(source)) = &current.session else { panic!("expected live Codex") };
    assert_eq!(source.native_continuity, chat::NativeContinuity::ReconnectOnly);
    let inherited = registry(&host).unwrap().lock().unwrap().retained_agent_authority(current.session.as_ref().unwrap().id()).unwrap();
    assert!(control(&inherited, answer("91", "decision-1")).is_err(), "seeing the request does not confer ownership");
    control(&owner, answer("91", "decision-1")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let asked = loop {
        let asked = pending_requests(&host, agent);
        if asked.iter().any(|request| request.id == "92") { break asked }
        assert!(Instant::now() < deadline, "second request did not arrive");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(asked.len(), 1);
    assert!(control(&owner, answer("91", "decision-1")).unwrap_err().contains("No pending request"));
    let error = control(&owner, answer("92", "decision-1")).unwrap_err();
    assert!(error.contains("no active run"), "the index changed meaning: {error}");
    control(&owner, answer("92", "decision-0")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let asked = pending_requests(&host, agent);
        if asked.iter().any(|request| request.id == "93") { break }
        assert!(Instant::now() < deadline, "third request did not arrive");
        std::thread::sleep(Duration::from_millis(10));
    }
    registry(&host).unwrap().lock().unwrap().claim_agent_interaction(agent, None).unwrap();
    let error = control(&owner, answer("93", "decision-1")).unwrap_err();
    assert!(error.contains("took this agent over"), "{error}");
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    super::super::super::finish_chat_shutdown(&host).unwrap();
    assert!(control(&owner, answer("93", "decision-1")).unwrap_err().contains("closed"));
    assert_eq!(server.join().unwrap(), vec![json!({"id":91,"result":{"decision":"decline"}}),json!({"id":92,"result":{"decision":"decline"}})]);
    assert_eq!(std::fs::read_to_string(root.join("codex-spawns")).unwrap(), "spawn\n");
}
