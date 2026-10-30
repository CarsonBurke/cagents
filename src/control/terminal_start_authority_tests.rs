//! Public terminal entry over private framed peers. Synthetic authentication
//! proves client custody and startup order, never actual Host permissions.
//! Remote cases concern early permission guidance on the current guarded
//! device-service path; they do not claim an old SSH execution vulnerability.
use super::*;
use crate::{
    model::{BootId, HostIncarnation, HostSessionId},
    terminal::ipc::ProtocolError,
};
use std::{
    cell::Cell,
    fs::{self, File},
    os::unix::{ffi::OsStringExt, fs::PermissionsExt, net::UnixListener},
    thread::JoinHandle,
};
const ROLE: &str = "CAGENTS_TEST_TERMINAL_START_ROLE";
const ROOT: &str = "CAGENTS_TEST_TERMINAL_START_ROOT";
const MODE: &str = "CAGENTS_TEST_TERMINAL_START_MODE";
const VERB: &str = "CAGENTS_TEST_TERMINAL_START_VERB";
const MARKER: &str = "CAGENTS_TEST_TERMINAL_START_SSH";
const TOKEN: &str = "synthetic-terminal-original-scope";
const REFUSAL: &str = "Private terminal peer refused the original scope";
const PROGRAM: &str = "/fixture/never-executed";
const TEXT: &str = "literal terminal input λ\n";

#[derive(Clone, Copy, Debug)]
enum Business {
    Spawn,
    Write,
    Close,
    List,
}
impl Business {
    fn name(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Write => "write",
            Self::Close => "close",
            Self::List => "list",
        }
    }
}
fn root() -> PathBuf {
    PathBuf::from(std::env::var_os(ROOT).unwrap())
}
fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("terminal-start-fixture"),
        boot_id: BootId::new("terminal-start-boot"),
        host_incarnation: HostIncarnation::new("terminal-start-incarnation"),
    }
}
fn message(peer: &mut UnixStream, request_id: Option<u64>, message: ServerMessage) {
    write_frame(
        peer,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id,
            proof: proof(),
            message,
        },
    )
    .unwrap();
}
fn response(peer: &mut UnixStream, request: &ClientEnvelope, response: ServerResponse) {
    message(
        peer,
        Some(request.request_id),
        ServerMessage::Response { response },
    );
}
fn refuse(peer: &mut UnixStream, request: &ClientEnvelope) {
    message(
        peer,
        Some(request.request_id),
        ServerMessage::Error(ProtocolError::new(
            ProtocolErrorCode::PermissionDenied,
            REFUSAL,
            false,
        )),
    );
}
fn synchronized(peer: &mut UnixStream, terminal_id: TerminalId) {
    message(
        peer,
        None,
        ServerMessage::Synchronized {
            terminal_id,
            generation: 0,
            event_sequence: 0,
        },
    );
}
fn next(peer: &mut UnixStream) -> ClientEnvelope {
    let request: ClientEnvelope = read_frame(peer).unwrap().unwrap();
    assert_eq!(request.proof.as_ref(), Some(&proof()));
    request
}
fn descriptor(id: TerminalId) -> TerminalDescriptor {
    TerminalDescriptor::relaunchable(
        id,
        LaunchRecipe {
            program: PROGRAM.into(),
            arguments: vec!["retained-argument".into()],
            working_directory: root().join("project"),
            environment: Default::default(),
            unset_environment: Default::default(),
        },
        None,
    )
}

struct Peer(Option<JoinHandle<(UnixListener, usize, usize)>>);
impl Peer {
    fn new(business: Business, id: TerminalId, authority: Authority, refuse_auth: bool) -> Self {
        let socket = daemon_socket_path().unwrap();
        assert!(socket.starts_with(root()));
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        if socket.exists() {
            fs::remove_file(&socket).unwrap();
        }
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self(Some(thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut readiness = 0;
            let mut raw = 0;
            let (mut peer, mut request) = loop {
                let mut peer = loop {
                    match listener.accept() {
                        Ok((peer, _)) => break peer,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "private terminal entry did not connect"
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
                    assert!(raw <= 1, "only the User no-start existence probe");
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
                // Let the causal predecessor finish its valid readiness probe
                // before driving the actual terminal operation.
                readiness += 1;
                assert!(readiness <= 1, "no repeated readiness or fallback");
            };
            let scoped = authority.token().is_some() && !matches!(business, Business::List);
            if scoped {
                assert!(
                    matches!(&request.request, ClientRequest::Authenticate { authority: actual } if actual == &authority)
                );
                if refuse_auth {
                    refuse(&mut peer, &request);
                    assert!(
                        read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                        "authentication refusal forbids Spawn/Attach and User fallback"
                    );
                    return (listener, readiness, raw);
                }
                response(&mut peer, &request, ServerResponse::Accepted);
                request = next(&mut peer);
            } else {
                assert!(!refuse_auth);
            }
            match business {
                Business::List => {
                    assert!(
                        matches!(request.request, ClientRequest::List),
                        "global list never authenticates even an invalid/empty inherited capability"
                    );
                    response(
                        &mut peer,
                        &request,
                        ServerResponse::Listed { terminals: vec![] },
                    );
                }
                Business::Spawn => {
                    let ClientRequest::Spawn { session, options } = &request.request else {
                        panic!("expected actual Spawn after authentication")
                    };
                    assert!(!session.id.as_uuid().is_nil());
                    assert_eq!(session.launch, descriptor(session.id).session.launch);
                    assert_eq!(options, &TerminalSpawnOptions::default());
                    if scoped {
                        refuse(&mut peer, &request);
                    } else {
                        response(
                            &mut peer,
                            &request,
                            ServerResponse::Spawned {
                                terminal: descriptor(session.id),
                            },
                        );
                        synchronized(&mut peer, session.id);
                    }
                }
                Business::Write | Business::Close => {
                    assert!(
                        matches!(request.request, ClientRequest::Attach { terminal_id, after_generation: 0, after_event_sequence: 0 } if terminal_id == id)
                    );
                    response(
                        &mut peer,
                        &request,
                        ServerResponse::Attached {
                            terminal: descriptor(id),
                        },
                    );
                    synchronized(&mut peer, id);
                    let operation = next(&mut peer);
                    match business {
                        Business::Write => assert!(
                            matches!(&operation.request, ClientRequest::Input { terminal_id, bytes } if *terminal_id == id && bytes == TEXT.as_bytes())
                        ),
                        Business::Close => assert!(
                            matches!(operation.request, ClientRequest::ShutdownTerminal { terminal_id } if terminal_id == id)
                        ),
                        _ => unreachable!(),
                    }
                    if scoped {
                        refuse(&mut peer, &operation);
                    } else {
                        response(&mut peer, &operation, ServerResponse::Accepted);
                    }
                    // ControlClient finishes attachment even when the actual
                    // Input/Shutdown was refused. This is not a mutation retry.
                    let detach = next(&mut peer);
                    assert!(
                        matches!(detach.request, ClientRequest::Detach { terminal_id } if terminal_id == id)
                    );
                    response(&mut peer, &detach, ServerResponse::Detached);
                }
            }
            assert!(
                read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                "no extra business request, User fallback, or automatic retry"
            );
            (listener, readiness, raw)
        })))
    }
    fn finished(mut self) -> (usize, usize) {
        let (listener, readiness, raw) = self.0.take().unwrap().join().unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "no fallback connection"
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

fn arguments(business: Business, id: TerminalId) -> Vec<OsString> {
    let mut args = vec![
        "cagents".into(),
        "--json".into(),
        "terminal".into(),
        business.name().into(),
    ];
    match business {
        Business::Spawn => args.extend([
            "--cwd".into(),
            root().join("project").into_os_string(),
            "--".into(),
            PROGRAM.into(),
            "retained-argument".into(),
        ]),
        Business::Write => args.extend([id.to_string().into(), TEXT.into()]),
        Business::Close => args.push(id.to_string().into()),
        Business::List => {}
    }
    args
}
fn local(role: &str) {
    fs::create_dir(root().join("project")).unwrap();
    let authority = Authority::from_environment();
    let scoped = authority.token().is_some();
    let no_start = role == "user-no-start";
    let id = TerminalId::new();
    let mut routing = Vec::new();
    for business in [
        Business::Spawn,
        Business::Write,
        Business::Close,
        Business::List,
    ] {
        let mutation = !matches!(business, Business::List);
        for refuse_auth in if scoped && mutation {
            vec![false, true]
        } else {
            vec![false]
        } {
            let peer = Peer::new(business, id, authority.clone(), refuse_auth);
            let mut args = arguments(business, id);
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
                if scoped && mutation {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
            );
            let (readiness, raw) = peer.finished();
            routing.push((business, refuse_auth, staged.get(), readiness, raw));
        }
    }
    // Reach every real command and both scoped refusal boundaries first;
    // only then diagnose the predecessor's startup/staged-owner behavior.
    for (business, refused_auth, staged, readiness, raw) in routing {
        let scoped_mutation = scoped && !matches!(business, Business::List);
        assert_eq!(
            staged,
            usize::from(!scoped_mutation),
            "{business:?} auth_refused={refused_auth}: scoped mutation bypasses staged owner"
        );
        assert_eq!(
            readiness,
            usize::from(!scoped_mutation && !no_start),
            "{business:?}: scoped mutation must not recover/start before authentication"
        );
        assert_eq!(
            raw,
            usize::from(!scoped_mutation && no_start),
            "User no-start/global-read existence probe remains unchanged"
        );
    }
}
fn proxies() -> Vec<PathBuf> {
    let runtime = root()
        .join("runtime")
        .join(crate::instance::RUNTIME_SUBDIRECTORY);
    let mut paths: Vec<_> = match fs::read_dir(runtime) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("device-")
            })
            .collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => vec![],
        Err(error) => panic!("private runtime: {error}"),
    };
    paths.sort();
    paths
}
fn remote() {
    assert!(Authority::from_environment().token().is_some());
    let configuration = crate::device::configuration_path().unwrap();
    fs::create_dir_all(configuration.parent().unwrap()).unwrap();
    fs::write(configuration, br#"{"devices":[{"name":"fixture","host":"terminal-scope.invalid","executable":"cagents"}]}"#).unwrap();
    let business = match std::env::var(VERB).unwrap().as_str() {
        "spawn" => Business::Spawn,
        "write" => Business::Write,
        "close" => Business::Close,
        _ => unreachable!(),
    };
    let before = proxies();
    let mut args = arguments(business, TerminalId::new());
    args.splice(1..1, ["--device".into(), "fixture".into()]);
    let staged = Cell::new(0);
    assert_eq!(
        run_from_with_staged_lookup(args, |_, _| {
            staged.set(staged.get() + 1);
            Ok(None)
        }),
        ExitCode::FAILURE
    );
    assert_eq!(staged.get(), 0);
    assert!(crate::device::is_remote());
    assert_eq!(crate::device::selected().unwrap().name, "fixture");
    assert_eq!(proxies(), before);
    assert!(!Path::new(&std::env::var_os(MARKER).unwrap()).exists());
    // Exactly one public invocation per child: Cleanup closes device selection
    // at return, so a second command would exercise a closed fixture instead.
}
fn outputs(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| line.find('{').map(|start| &line[start..]))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
fn child(selector: &str, role: &str, remote_verb: Option<Business>) {
    let private = tempfile::Builder::new().prefix("ts-").tempdir().unwrap();
    let base = private.path().canonicalize().unwrap();
    for leaf in [
        "home", "runtime", "config", "data", "cache", "state", "tmp", "bin",
    ] {
        let path = base.join(leaf);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let marker = base.join("ssh-called");
    let ssh = base.join("bin/ssh");
    fs::write(
        &ssh,
        b"#!/bin/sh\nprintf 'unexpected ssh\\n' >> \"$CAGENTS_TEST_TERMINAL_START_SSH\"\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let stdout = base.join("stdout");
    let stderr = base.join("stderr");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .env_clear()
        .args(["--exact", selector, "--nocapture", "--test-threads=1"])
        .current_dir(&base)
        .env(ROLE, role)
        .env(ROOT, &base)
        .env(MARKER, &marker)
        .env(
            MODE,
            if remote_verb.is_some() {
                "remote"
            } else {
                "local"
            },
        )
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
    if let Some(verb) = remote_verb {
        command.env(VERB, verb.name());
    }
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
    let mut process = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(35);
    let status = loop {
        if let Some(status) = process.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            process.kill().unwrap();
            process.wait().unwrap();
            panic!("private terminal child exceeded bound: {role}");
        }
        thread::sleep(Duration::from_millis(5));
    };
    assert!(fs::metadata(&stdout).unwrap().len() <= 1024 * 1024);
    assert!(fs::metadata(&stderr).unwrap().len() <= 1024 * 1024);
    let stdout = fs::read_to_string(stdout).unwrap();
    let stderr = fs::read_to_string(stderr).unwrap();
    assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
    assert!(
        status.success(),
        "{role} {remote_verb:?}: {stdout}\n{stderr}"
    );
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored;"));
    assert!(!marker.exists());
    let errors = outputs(&stderr);
    if remote_verb.is_some() {
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["ok"], false);
        assert_eq!(errors[0]["error"]["kind"], "permission_denied");
        let message = errors[0]["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("own terminal"),
            "remote refusal must explain the recovery action"
        );
        assert!(!message.contains("daemon start"));
        assert!(outputs(&stdout).is_empty());
    } else {
        let scoped = !role.starts_with("user");
        assert_eq!(errors.len(), if scoped { 6 } else { 0 });
        for error in errors {
            assert_eq!(error["ok"], false);
            assert_eq!(error["error"]["kind"], "protocol");
            assert_eq!(
                error["error"]["details"]["code"],
                serde_json::to_value(ProtocolErrorCode::PermissionDenied).unwrap()
            );
            assert!(
                error["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(REFUSAL)
            );
        }
        let output = outputs(&stdout);
        assert_eq!(output.len(), if scoped { 1 } else { 4 });
        for result in &output {
            assert_eq!(result["ok"], true);
        }
        let list = output
            .iter()
            .find(|value| value["command"] == "terminal.list")
            .expect("global List success");
        assert!(list["data"]["terminals"].as_array().unwrap().is_empty());
        if !scoped {
            let spawn = output
                .iter()
                .find(|value| value["command"] == "terminal.spawn")
                .unwrap();
            assert_eq!(
                spawn["data"]["terminal"]["session"]["launch"]["program"],
                PROGRAM
            );
            let write = output
                .iter()
                .find(|value| value["command"] == "terminal.write")
                .unwrap();
            assert_eq!(write["data"]["written"], true);
            let close = output
                .iter()
                .find(|value| value["command"] == "terminal.close")
                .unwrap();
            assert_eq!(close["data"]["closed"], true);
            assert_eq!(write["data"]["terminal_id"], close["data"]["terminal_id"]);
        }
    }
}
#[test]
fn public_terminal_mutations_preserve_original_scope_before_staged_lookup_and_readiness() {
    if let Some(role) = std::env::var_os(ROLE) {
        if std::env::var(MODE).unwrap() == "remote" {
            remote();
        } else {
            local(role.to_str().unwrap());
        }
        return;
    }
    let selector = thread::current().name().unwrap().to_owned();
    for role in ["user", "user-no-start", "present", "empty", "non-unicode"] {
        child(&selector, role, None);
    }
    for role in ["present", "empty", "non-unicode"] {
        for verb in [Business::Spawn, Business::Write, Business::Close] {
            child(&selector, role, Some(verb));
        }
    }
}
