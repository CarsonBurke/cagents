//! Public Agent CLI over a private framed peer. A lost reply proves only
//! transport uncertainty; no actual durable acceptance/provider run is claimed.
use super::*;
use crate::{
    agent_execution::{Authority, Destination, Operation, Reply, Submit},
    model::{AgentId, BootId, HostIncarnation, HostSessionId},
    terminal::ipc::ProtocolError,
};
use std::{
    cell::Cell,
    fs::{self, File},
    os::unix::{ffi::OsStringExt, fs::PermissionsExt, net::UnixListener},
    thread::JoinHandle,
};
const ROLE: &str = "CAGENTS_TEST_AGENT_START_ROLE";
const ROOT: &str = "CAGENTS_TEST_AGENT_START_ROOT";
const TOKEN: &str = "synthetic-agent-original-scope";
const KEY: &str = "private-agent-start-original-operation";
const PROMPT: &str = "Retain this exact synthetic prompt λ";
const REFUSAL: &str = "Private Agent peer refused the original scope";
#[derive(Clone, Copy, Debug)]
enum Business {
    List,
    Send,
}
fn root() -> PathBuf {
    PathBuf::from(std::env::var_os(ROOT).unwrap())
}
fn agent() -> AgentId {
    "00000000-0000-4000-8000-000000000041".parse().unwrap()
}
fn original_submit() -> Submit {
    Submit {
        name: None,
        destination: Destination::Existing(agent()),
        prompt: PROMPT.into(),
        idempotency_key: KEY.into(),
    }
}
fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("agent-start-fixture"),
        boot_id: BootId::new("agent-start-boot"),
        host_incarnation: HostIncarnation::new("agent-start-incarnation"),
    }
}
fn reply(peer: &mut UnixStream, request: &ClientEnvelope, message: ServerMessage) {
    write_frame(
        peer,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request.request_id),
            proof: proof(),
            message,
        },
    )
    .unwrap();
}
fn response(peer: &mut UnixStream, request: &ClientEnvelope, response: ServerResponse) {
    reply(peer, request, ServerMessage::Response { response });
}
fn next(peer: &mut UnixStream) -> ClientEnvelope {
    let request: ClientEnvelope = read_frame(peer).unwrap().unwrap();
    assert_eq!(request.proof.as_ref(), Some(&proof()));
    request
}
fn assert_operation(request: &ClientEnvelope, authority: &Authority, expected: &Operation) {
    assert!(
        matches!(&request.request, ClientRequest::Agent { authority: actual, operation } if actual == authority && operation == expected),
        "exact Agent operation with original authority; never Authenticate or User fallback"
    );
}
struct Peer(Option<JoinHandle<(UnixListener, usize, usize)>>);
impl Peer {
    fn new(business: Business, authority: Authority) -> Self {
        let socket = daemon_socket_path().unwrap();
        assert!(socket.starts_with(root()));
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        if socket.exists() {
            fs::remove_file(&socket).unwrap();
        }
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self(Some(thread::spawn(move || {
            let mut readiness = 0;
            let mut raw = 0;
            let attempts = if matches!(business, Business::Send) {
                2
            } else {
                1
            };
            for attempt in 0..attempts {
                let deadline = Instant::now() + Duration::from_secs(5);
                let (mut peer, request) = loop {
                    let mut peer = loop {
                        match listener.accept() {
                            Ok((peer, _)) => break peer,
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                assert!(
                                    Instant::now() < deadline,
                                    "private Agent invocation did not connect"
                                );
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(error) => panic!("private listener: {error}"),
                        }
                    };
                    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    peer.set_write_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let Some(hello) = read_frame::<ClientEnvelope>(&mut peer).unwrap() else {
                        raw += 1;
                        assert!(raw <= attempts, "no repeated User no-start probe");
                        continue;
                    };
                    assert!(matches!(hello.request, ClientRequest::Connect { .. }));
                    assert!(hello.proof.is_none());
                    response(
                        &mut peer,
                        &hello,
                        ServerResponse::Connected {
                            proof: proof(),
                            resumed: vec![],
                        },
                    );
                    if let Some(request) = read_frame::<ClientEnvelope>(&mut peer).unwrap() {
                        assert_eq!(request.proof.as_ref(), Some(&proof()));
                        break (peer, request);
                    }
                    // Accept the predecessor's valid initial/retry readiness,
                    // then reach actual business before asserting routing.
                    readiness += 1;
                    assert!(readiness <= attempts, "no repeated readiness/fallback");
                };
                if attempt == 0 {
                    assert_operation(&request, &authority, &Operation::List);
                    response(
                        &mut peer,
                        &request,
                        ServerResponse::Agent(Reply::Listed(vec![])),
                    );
                    if matches!(business, Business::List) {
                        assert!(read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none());
                        continue;
                    }
                    // Even a full UUID is resolved against this caller's list
                    // first. Empty is a valid result, not an invalid fixture.
                    let request = next(&mut peer);
                    assert_operation(&request, &authority, &Operation::Submit(original_submit()));
                    // Lose precisely the first Submit reply. Do not fabricate
                    // an acceptance, receipt, or backend side effect.
                    peer.shutdown(std::net::Shutdown::Both).unwrap();
                } else {
                    assert_operation(&request, &authority, &Operation::Submit(original_submit()));
                    reply(
                        &mut peer,
                        &request,
                        ServerMessage::Error(ProtocolError::new(
                            ProtocolErrorCode::PermissionDenied,
                            REFUSAL,
                            false,
                        )),
                    );
                    assert!(
                        read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                        "one exact retry; no third Submit or User fallback"
                    );
                }
            }
            (listener, readiness, raw)
        })))
    }
    fn finished(mut self) -> (usize, usize) {
        let (listener, readiness, raw) = self.0.take().unwrap().join().unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "no extra connection after the definitive refusal"
        );
        (readiness, raw)
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(worker) = self.0.take() {
            let _ = worker.join();
        }
    }
}
fn exercise(role: &str) {
    let authority = Authority::from_environment();
    let scoped = authority.token().is_some();
    let no_start = role == "user-no-start";
    let mut routing = Vec::new();
    for business in [Business::List, Business::Send] {
        let peer = Peer::new(business, authority.clone());
        let mut args: Vec<OsString> = vec!["cagents".into(), "--json".into(), "agent".into()];
        match business {
            Business::List => args.push("list".into()),
            Business::Send => args.extend([
                "send".into(),
                agent().to_string().into(),
                PROMPT.into(),
                "--idempotency-key".into(),
                KEY.into(),
            ]),
        }
        if no_start {
            args.insert(1, "--no-start".into());
        }
        let staged = Cell::new(0);
        let result = run_from_with_staged_lookup(args, |_, recover| {
            staged.set(staged.get() + 1);
            assert_eq!(recover, !no_start);
            Ok(None)
        });
        assert_eq!(
            result,
            if matches!(business, Business::List) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        );
        let (ready, raw) = peer.finished();
        routing.push((business, staged.get(), ready, raw));
    }
    for (business, staged, ready, raw) in routing {
        let connections = if matches!(business, Business::Send) {
            2
        } else {
            1
        };
        assert_eq!(
            staged,
            usize::from(!scoped),
            "{business:?}: scoped Agent requests cannot delegate before capture"
        );
        assert_eq!(
            ready,
            if !scoped && !no_start { connections } else { 0 },
            "{business:?}: both initial and exact Submit retry must retain startup policy"
        );
        assert_eq!(raw, if !scoped && no_start { connections } else { 0 });
    }
}
#[test]
fn public_agent_list_and_exact_submit_retry_keep_scope_before_staged_owner_and_readiness() {
    if let Some(role) = std::env::var_os(ROLE) {
        exercise(role.to_str().unwrap());
        return;
    }
    let selector = thread::current().name().unwrap().to_owned();
    for role in ["user", "user-no-start", "present", "empty", "non-unicode"] {
        let private = tempfile::Builder::new().prefix("as-").tempdir().unwrap();
        let base = private.path().canonicalize().unwrap();
        for leaf in [
            "home", "runtime", "config", "data", "cache", "state", "tmp", "bin",
        ] {
            let path = base.join(leaf);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let stdout = base.join("stdout");
        let stderr = base.join("stderr");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
            .current_dir(&base)
            .env(ROLE, role)
            .env(ROOT, &base)
            .env("PATH", base.join("bin"))
            .env("HOME", base.join("home"))
            .env("XDG_RUNTIME_DIR", base.join("runtime"))
            .env("XDG_CONFIG_HOME", base.join("config"))
            .env("XDG_DATA_HOME", base.join("data"))
            .env("XDG_CACHE_HOME", base.join("cache"))
            .env("XDG_STATE_HOME", base.join("state"))
            .env("TMPDIR", base.join("tmp"))
            .stdin(Stdio::null())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap());
        if !role.starts_with("user") {
            command.env(
                crate::agent_execution::AUTHORITY_ENV,
                match role {
                    "present" => OsString::from(TOKEN),
                    "empty" => OsString::new(),
                    "non-unicode" => OsString::from_vec(vec![0xff]),
                    _ => unreachable!(),
                },
            );
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("private Agent child exceeded bound: {role}");
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert!(fs::metadata(&stdout).unwrap().len() <= 1024 * 1024);
        assert!(fs::metadata(&stderr).unwrap().len() <= 1024 * 1024);
        let stdout = fs::read_to_string(stdout).unwrap();
        let stderr = fs::read_to_string(stderr).unwrap();
        assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
        assert!(status.success(), "{role}: {stdout}\n{stderr}");
        assert!(stdout.contains("1 passed; 0 failed; 0 ignored;"));
        let errors = stderr
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), 1);
        let error = &errors[0];
        assert_eq!(error["ok"], false);
        assert_eq!(error["error"]["kind"], "protocol");
        assert_eq!(
            error["error"]["details"]["code"],
            serde_json::to_value(ProtocolErrorCode::PermissionDenied).unwrap()
        );
        assert_eq!(error["error"]["details"]["idempotency_key"], KEY);
        let guidance = error["error"]["message"].as_str().unwrap();
        assert!(
            guidance.contains(REFUSAL) && guidance.contains(&format!("--idempotency-key {KEY}"))
        );
        let successes = stdout
            .lines()
            .filter_map(|line| line.find('{').map(|start| &line[start..]))
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect::<Vec<_>>();
        assert_eq!(successes.len(), 1);
        assert_eq!(successes[0]["ok"], true);
        assert_eq!(successes[0]["command"], "agent.list");
        assert!(
            successes[0]["data"]["agents"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
