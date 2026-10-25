//! Real CLI/framed-private-peer controls; no daemon or provider is launched.
use super::*;
use crate::{
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
    terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, TerminalDescriptor, daemon_socket_path,
        read_frame, write_frame,
    },
    work::{Column, Item, ItemKey, RemovalPlan, Removed, Request, Response},
    worktree_content::{Inspection, Verdict},
};
use std::{
    ffi::OsString,
    fs::{self, File},
    os::unix::{
        ffi::OsStringExt,
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::Path,
    process::{Command, ExitCode, Stdio},
    thread,
    time::{Duration, Instant},
};

const ROLE: &str = "CAGENTS_TEST_AUTHORITY_ENVIRONMENT_ROLE";
// This is inert and accepted ONLY by the private fake peer below.
const FAKE_AUTHORITY: &str = "d261c6ec-12f5-4ba6-a5bc-1277849491a3";

#[test]
fn present_authority_never_downgrades_terminal_or_work_cli_to_user() {
    let selector = thread::current().name().unwrap().to_owned();
    let Some(role) = std::env::var_os(ROLE) else {
        for role in ["non-unicode", "empty", "utf8", "user"] {
            let private = tempfile::Builder::new().prefix("ae-").tempdir().unwrap();
            for leaf in ["home", "runtime", "config", "data", "cache"] {
                let path = private.path().join(leaf);
                fs::create_dir(&path).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            let stdout = private.path().join("stdout");
            let stderr = private.path().join("stderr");
            let mut command = Command::new(std::env::current_exe().unwrap());
            // Fixed fixture environments exist only in these self-exec fake-
            // peer children. Never inspect/copy/clear the parent's capability.
            command
                .env_clear()
                .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
                .current_dir(private.path())
                .env(ROLE, role)
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", private.path().join("home"))
                .env("XDG_RUNTIME_DIR", private.path().join("runtime"))
                .env("XDG_CONFIG_HOME", private.path().join("config"))
                .env("XDG_DATA_HOME", private.path().join("data"))
                .env("XDG_CACHE_HOME", private.path().join("cache"))
                .stdin(Stdio::null())
                .stdout(File::create(&stdout).unwrap())
                .stderr(File::create(&stderr).unwrap());
            match role {
                "non-unicode" => {
                    command.env(AUTHORITY_ENV, OsString::from_vec(vec![0xff]));
                }
                "empty" => {
                    command.env(AUTHORITY_ENV, "");
                }
                "utf8" => {
                    command.env(AUTHORITY_ENV, FAKE_AUTHORITY);
                }
                "user" => {}
                _ => unreachable!(),
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
                    panic!("private authority fixture timed out: {role}");
                }
                thread::sleep(Duration::from_millis(5));
            };
            assert!(fs::metadata(&stdout).unwrap().len() <= 1024 * 1024);
            assert!(fs::metadata(&stderr).unwrap().len() <= 1024 * 1024);
            let stdout = fs::read_to_string(stdout).unwrap();
            let stderr = fs::read_to_string(stderr).unwrap();
            assert!(status.success(), "{role}: {stdout}\n{stderr}");
            assert!(stdout.contains(&format!("test {selector} ...")));
            assert!(stdout.contains("1 passed; 0 failed; 0 ignored;"));
            assert!(!stdout.contains(FAKE_AUTHORITY));
            assert!(!stderr.contains(FAKE_AUTHORITY));
            let errors = stderr
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .collect::<Vec<_>>();
            assert_eq!(
                errors.len(),
                match role {
                    "user" => 0,
                    "utf8" => 3,
                    _ => 6,
                }
            );
            assert!(errors.iter().all(|error| error["ok"] == false
                && error["error"]["details"]["code"] == "permission_denied"));
        }
        return;
    };
    let role = role.to_str().unwrap().to_owned();
    let socket = daemon_socket_path().unwrap();
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("fixture-host"),
        boot_id: BootId::new("fixture-boot"),
        host_incarnation: HostIncarnation::new("fixture-incarnation"),
    };
    let root = std::env::current_dir().unwrap();
    let peer_role = role.clone();
    let server = thread::spawn(move || {
        let mut authenticated = 0;
        let mut operations = 0;
        let mut reads = 0;
        for operation in ["spawn", "write", "close", "pin", "unpin", "plan", "remove"] {
            // --no-start first checks socket liveness with one bare connection.
            // Require its clean EOF, then keep the actual framed session strict.
            let mut probe = accept(&listener);
            assert!(
                read_frame::<ClientEnvelope>(&mut probe).unwrap().is_none(),
                "the liveness probe unexpectedly sent a request"
            );
            drop(probe);
            let mut stream = accept(&listener);
            let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert_eq!(connect.version, PROTOCOL_VERSION);
            assert_eq!(connect.request_id, 1);
            assert_eq!(connect.proof, None);
            assert!(matches!(connect.request, ClientRequest::Connect { .. }));
            respond(
                &mut stream,
                &proof,
                1,
                ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: Vec::new(),
                },
            );
            let mut next = 2;
            let mut request = receive(&mut stream, &proof, &mut next);
            if peer_role != "user" && operation != "plan" {
                let ClientRequest::Authenticate { authority } = &request.request else {
                    panic!("mutation skipped authentication");
                };
                authenticated += 1;
                assert_eq!(
                    authority.token(),
                    Some(if peer_role == "utf8" {
                        FAKE_AUTHORITY
                    } else {
                        ""
                    })
                );
                if peer_role != "utf8" {
                    refuse(&mut stream, &proof, request.request_id);
                    assert!(
                        read_frame::<ClientEnvelope>(&mut stream).unwrap().is_none(),
                        "invalid authority proceeded to mutation"
                    );
                    continue;
                }
                respond(
                    &mut stream,
                    &proof,
                    request.request_id,
                    ServerResponse::Accepted,
                );
                request = receive(&mut stream, &proof, &mut next);
            } else {
                assert!(!matches!(
                    request.request,
                    ClientRequest::Authenticate { .. }
                ));
            }
            let attached = if matches!(operation, "write" | "close") {
                let ClientRequest::Attach {
                    terminal_id,
                    after_generation,
                    after_event_sequence,
                } = request.request
                else {
                    panic!("terminal command omitted attachment");
                };
                assert_eq!((after_generation, after_event_sequence), (0, 0));
                let recipe = crate::model::LaunchRecipe::shell(root.clone()).unwrap();
                respond(
                    &mut stream,
                    &proof,
                    request.request_id,
                    ServerResponse::Attached {
                        terminal: TerminalDescriptor::relaunchable(terminal_id, recipe, None),
                    },
                );
                synchronize(&mut stream, &proof, terminal_id);
                request = receive(&mut stream, &proof, &mut next);
                Some(terminal_id)
            } else {
                None
            };
            let response = match (operation, request.request) {
                ("spawn", ClientRequest::Spawn { session, .. }) => {
                    let mut terminal = TerminalDescriptor::relaunchable(
                        session.id,
                        session.launch.clone(),
                        session.transcript_path.clone(),
                    );
                    terminal.session = session;
                    ServerResponse::Spawned { terminal }
                }
                ("write", ClientRequest::Input { terminal_id, bytes }) => {
                    assert_eq!(Some(terminal_id), attached);
                    assert_eq!(bytes, b"fixture-input");
                    ServerResponse::Accepted
                }
                ("close", ClientRequest::ShutdownTerminal { terminal_id }) => {
                    assert_eq!(Some(terminal_id), attached);
                    ServerResponse::Accepted
                }
                ("pin" | "unpin", ClientRequest::Work(Request::Pin { item, column, .. })) => {
                    assert_eq!(item, "fixture-branch");
                    assert_eq!(
                        column,
                        if operation == "pin" {
                            Some(Column::Active)
                        } else {
                            None
                        }
                    );
                    ServerResponse::Work(Response::Pinned(Box::new(item_fixture(&root))))
                }
                ("plan", ClientRequest::Work(Request::RemovalPlan { item, fetch, .. })) => {
                    assert_eq!(item, "fixture-branch");
                    assert!(!fetch);
                    reads += 1;
                    ServerResponse::Work(Response::RemovalPlan(Box::new(plan_fixture(&root))))
                }
                ("remove", ClientRequest::Work(Request::Remove(removal))) => {
                    assert_eq!(removal.item, "fixture-branch");
                    assert_eq!(removal.token, "fixture-content");
                    ServerResponse::Work(Response::Removed(Box::new(Removed {
                        project: root.clone(),
                        path: root.join("uncreated-worktree"),
                        snapshot: None,
                        deleted_branch: None,
                        branch_error: None,
                    })))
                }
                _ => panic!("unexpected CLI operation"),
            };
            if operation != "plan" {
                operations += 1;
                if peer_role == "utf8" && matches!(operation, "pin" | "unpin" | "remove") {
                    // The fake actor can use its terminal controls, but cannot
                    // mutate Work. This proves framing/refusal, not a daemon grant.
                    refuse(&mut stream, &proof, request.request_id);
                    assert!(read_frame::<ClientEnvelope>(&mut stream).unwrap().is_none());
                    continue;
                }
            }
            let spawned = match &response {
                ServerResponse::Spawned { terminal } => Some(terminal.session.id),
                _ => None,
            };
            respond(&mut stream, &proof, request.request_id, response);
            if let Some(terminal_id) = spawned {
                synchronize(&mut stream, &proof, terminal_id);
            }
            if let Some(expected) = attached {
                let detach = receive(&mut stream, &proof, &mut next);
                assert!(
                    matches!(detach.request, ClientRequest::Detach { terminal_id } if terminal_id == expected)
                );
                respond(
                    &mut stream,
                    &proof,
                    detach.request_id,
                    ServerResponse::Detached,
                );
            }
            assert!(read_frame::<ClientEnvelope>(&mut stream).unwrap().is_none());
        }
        assert_eq!(reads, 1);
        assert_eq!(authenticated, if peer_role == "user" { 0 } else { 6 });
        assert_eq!(
            operations,
            if matches!(peer_role.as_str(), "non-unicode" | "empty") {
                0
            } else {
                6
            }
        );
    });
    let id = TerminalId::new().to_string();
    let cwd = std::env::current_dir().unwrap();
    let cwd = cwd.to_str().unwrap();
    let commands = [
        vec!["terminal", "spawn", "--cwd", cwd, "--", "/bin/true"],
        vec!["terminal", "write", &id, "fixture-input"],
        vec!["terminal", "close", &id],
        vec!["work", "pin", "fixture-branch", "active"],
        vec!["work", "unpin", "fixture-branch"],
        vec!["work", "rm", "fixture-branch", "--yes"],
    ];
    for command in commands {
        let mut argv = vec!["cagents", "--json", "--no-start"];
        let permitted = role == "user" || (role == "utf8" && command[0] == "terminal");
        argv.extend(command);
        assert_eq!(
            crate::control::run_from(argv),
            if permitted {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        );
    }
    server.join().unwrap();
    let authority = Authority::from_environment();
    assert_eq!(
        format!("{authority:?}"),
        if role == "user" {
            "Authority::User"
        } else {
            "Authority::Session(<redacted>)"
        }
    );
}

fn accept(listener: &UnixListener) -> UnixStream {
    let mut target = libc::pollfd {
        fd: std::os::fd::AsRawFd::as_raw_fd(listener),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut target, 1, 5000) }, 1);
    let (stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
}
fn receive(stream: &mut UnixStream, proof: &HostSessionProof, next: &mut u64) -> ClientEnvelope {
    let request: ClientEnvelope = read_frame(stream).unwrap().unwrap();
    assert_eq!(request.version, PROTOCOL_VERSION);
    assert_eq!(request.proof, Some(proof.clone()));
    assert_eq!(request.request_id, *next);
    *next += 1;
    request
}
fn synchronize(stream: &mut UnixStream, proof: &HostSessionProof, terminal_id: TerminalId) {
    write_frame(
        stream,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: None,
            proof: proof.clone(),
            message: ServerMessage::Synchronized {
                terminal_id,
                generation: 0,
                event_sequence: 0,
            },
        },
    )
    .unwrap();
}

fn respond(
    stream: &mut UnixStream,
    proof: &HostSessionProof,
    request_id: u64,
    response: ServerResponse,
) {
    write_frame(
        stream,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request_id),
            proof: proof.clone(),
            message: ServerMessage::Response { response },
        },
    )
    .unwrap();
}
fn refuse(stream: &mut UnixStream, proof: &HostSessionProof, request_id: u64) {
    write_frame(
        stream,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request_id),
            proof: proof.clone(),
            message: ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                "Fixture caller is not permitted",
                false,
            )),
        },
    )
    .unwrap();
}
fn item_fixture(root: &Path) -> Item {
    Item {
        project: root.to_owned(),
        project_name: "fixture".into(),
        key: ItemKey::Branch("fixture-branch".into()),
        branch: Some("fixture-branch".into()),
        worktree: None,
        head: None,
        comparison: None,
        dirty: None,
        pull_request: None,
        linear: None,
        sessions: Vec::new(),
        last_activity: None,
        error: None,
        column: Column::Active,
        derived: Column::Active,
        pinned: true,
    }
}
fn plan_fixture(root: &Path) -> RemovalPlan {
    RemovalPlan {
        project: root.to_owned(),
        project_name: "fixture".into(),
        item: "fixture-branch".into(),
        inspection: Inspection {
            path: root.join("uncreated-worktree"),
            branch: Some("fixture-branch".into()),
            head: None,
            verdict: Verdict::CommitsKept,
            landed_in: None,
            staged: 0,
            modified: 0,
            untracked: 0,
            untracked_bytes: 0,
            filtered: Vec::new(),
            nested: Vec::new(),
            ignored: Vec::new(),
            ignored_total: 0,
            token: Some("fixture-content".into()),
        },
        merged_pull_request: None,
    }
}
