//! Owning UI tasks over private framed IPC. The synthetic peer checks caller
//! fidelity and refusal handling, without validating a production capability.
use super::*;
use crate::{
    agent_execution::Authority,
    automations::{Request as AutomationRequest, Response as AutomationResponse},
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
    terminal_ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
    work::{Board, Column, Item, ItemKey, Request as WorkRequest, Response as WorkResponse, Scope},
};
use cosmic::iced::futures::StreamExt;
use std::{
    ffi::OsString,
    fs::{self, File},
    io,
    os::unix::{
        ffi::OsStringExt,
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    process::{Command, Stdio},
    thread::{self, JoinHandle},
};

const ROLE: &str = "CAGENTS_TEST_UI_REQUEST_SCOPE_ROLE";
const ROOT: &str = "CAGENTS_TEST_UI_REQUEST_SCOPE_ROOT";
const TOKEN: &str = "synthetic-ui-request-original-scope";
const REFUSAL: &str = "Synthetic request peer refused the captured scope";

#[derive(Clone, Copy)]
enum Operation {
    Automation,
    Pin { refuse_authentication: bool },
}

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("ui-request-authority-fixture"),
        boot_id: BootId::new("fixture-boot"),
        host_incarnation: HostIncarnation::new("fixture-incarnation"),
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

fn refuse(peer: &mut UnixStream, request: &ClientEnvelope) {
    reply(
        peer,
        request,
        ServerMessage::Error(ProtocolError::new(
            ProtocolErrorCode::PermissionDenied,
            REFUSAL,
            false,
        )),
    );
}

fn item() -> Item {
    Item {
        project: "/fixture/project".into(),
        project_name: "Fixture".into(),
        key: ItemKey::Branch("fix/scope".into()),
        branch: Some("fix/scope".into()),
        worktree: None,
        head: None,
        comparison: None,
        dirty: None,
        pull_request: None,
        linear: None,
        sessions: Vec::new(),
        last_activity: None,
        error: None,
        column: Column::InProgress,
        derived: Column::InProgress,
        pinned: false,
    }
}

struct Peer(Option<JoinHandle<UnixListener>>);
impl Peer {
    fn new(operation: Operation, authority: Authority) -> Self {
        let socket = crate::terminal_ipc::daemon_socket_path().unwrap();
        assert!(socket.starts_with(PathBuf::from(std::env::var_os(ROOT).unwrap())));
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        if socket.exists() {
            fs::remove_file(&socket).unwrap();
        }
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self(Some(thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut peer, mut request) = {
                let (mut peer, _) = loop {
                    match listener.accept() {
                        Ok(value) => break value,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "UI task did not connect");
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("private request listener: {error}"),
                    }
                };
                peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                peer.set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                assert!(matches!(hello.request, ClientRequest::Connect { .. }));
                assert!(hello.proof.is_none());
                response(
                    &mut peer,
                    &hello,
                    ServerResponse::Connected {
                        proof: proof(),
                        resumed: Vec::new(),
                    },
                );
                // A connection that only handshakes would be a separate
                // readiness probe; readiness hands its connection to the
                // request instead.
                let request = read_frame::<ClientEnvelope>(&mut peer)
                    .unwrap()
                    .expect("no separate readiness handshake or User fallback");
                (peer, request)
            };
            assert_eq!(request.proof.as_ref(), Some(&proof()));
            match operation {
                Operation::Automation => {
                    let ClientRequest::Automation {
                        authority: actual,
                        operation,
                    } = &request.request
                    else {
                        panic!("the owning Automation task must send its request");
                    };
                    assert_eq!(
                        actual, &authority,
                        "Automation must retain the original caller"
                    );
                    assert_eq!(
                        operation,
                        &AutomationRequest::List {
                            after: None,
                            limit: 50
                        }
                    );
                    if authority.token().is_some() {
                        refuse(&mut peer, &request);
                    } else {
                        response(
                            &mut peer,
                            &request,
                            ServerResponse::Automation(AutomationResponse::Listed(Vec::new())),
                        );
                    }
                }
                Operation::Pin {
                    refuse_authentication,
                } => {
                    if authority.token().is_some() {
                        assert!(
                            matches!(&request.request, ClientRequest::Authenticate { authority: actual } if actual == &authority),
                            "Pin must authenticate its original caller before sending Work"
                        );
                        if refuse_authentication {
                            refuse(&mut peer, &request);
                        } else {
                            response(&mut peer, &request, ServerResponse::Accepted);
                            request = read_frame(&mut peer).unwrap().unwrap();
                        }
                    }
                    if !refuse_authentication {
                        assert_eq!(request.proof.as_ref(), Some(&proof()));
                        assert!(
                            matches!(&request.request, ClientRequest::Work(actual) if actual == &WorkRequest::Pin {
                                scope: Scope::Project(item().project), item: "branch:fix/scope".into(), column: Some(Column::Stale),
                            }),
                            "Pin must preserve its exact Work envelope"
                        );
                        if authority.token().is_some() {
                            refuse(&mut peer, &request);
                        } else {
                            response(
                                &mut peer,
                                &request,
                                ServerResponse::Work(WorkResponse::Pinned(Box::new(item()))),
                            );
                        }
                    }
                }
            }
            assert!(
                read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                "no Work request after refused authentication or User retry after refusal"
            );
            listener
        })))
    }

    fn finished(mut self) {
        let listener = self.0.take().unwrap().join().unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "the task must not open a fallback connection"
        );
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(worker) = self.0.take() {
            let _ = worker.join();
        }
    }
}

fn complete(task: Task<Message>) -> Message {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut stream = iced::runtime::task::into_stream(task).expect("owning task must run");
            tokio::time::timeout(Duration::from_secs(8), async {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                        return message;
                    }
                }
                panic!("owning task did not return its completion");
            })
            .await
            .expect("private owning task exceeded its bound")
        })
}

fn exercise_app(role: &str) {
    let authority = if matches!(role, "user" | "user-no-start") {
        Authority::default()
    } else {
        Authority::session(if role == "present" {
            TOKEN.into()
        } else {
            String::new()
        })
    };
    let scoped = authority.token().is_some();
    for operation in [
        Operation::Automation,
        Operation::Pin {
            refuse_authentication: false,
        },
        Operation::Pin {
            refuse_authentication: true,
        },
    ] {
        if !scoped
            && matches!(
                operation,
                Operation::Pin {
                    refuse_authentication: true
                }
            )
        {
            continue;
        }
        let mut app = context_inspector_test_app(false);
        let task = start(&mut app, operation);
        let peer = Peer::new(operation, authority.clone());
        let completion = complete(task);
        let result = match &completion {
            Message::AutomationUi(crate::automation_ui::Action::Received { result, .. }) => {
                if scoped {
                    assert_eq!(
                        result.as_ref().as_ref().unwrap_err(),
                        "This window's agent session isn't allowed to view automations.",
                        "Automation explains which task this window cannot perform"
                    );
                } else {
                    assert_eq!(
                        result.as_ref().as_ref().unwrap(),
                        &AutomationResponse::Listed(Vec::new())
                    );
                }
                None
            }
            Message::WorkUi(crate::work_ui::Action::Pinned(result)) => Some(result),
            _ => panic!("unexpected owning completion"),
        };
        if let Some(result) = result {
            if scoped {
                assert_eq!(
                    result.as_ref().unwrap_err(),
                    "Couldn't pin this card: this window's agent session isn't allowed to change Work."
                );
            } else {
                assert_eq!(result, &Ok(()));
            }
        }
        drop(app.update(completion));
        drop(app);
        peer.finished();
    }
    // Readiness hands its connection to the request, so each task's route
    // shows in how it reports the daemon once it is gone: only readiness
    // would start it, which the held update launch fence refuses.
    let socket = crate::terminal_ipc::daemon_socket_path().unwrap();
    for operation in [
        Operation::Automation,
        Operation::Pin {
            refuse_authentication: false,
        },
    ] {
        assert!(UnixStream::connect(&socket).is_err(), "the daemon is gone");
        let _fence = request_failure::hold_launch_fence();
        let mut app = context_inspector_test_app(false);
        let task = start(&mut app, operation);
        let error = match complete(task) {
            Message::AutomationUi(crate::automation_ui::Action::Received { result, .. }) => {
                result.as_ref().as_ref().unwrap_err().clone()
            }
            Message::WorkUi(crate::work_ui::Action::Pinned(result)) => result.unwrap_err(),
            _ => panic!("unexpected owning completion"),
        };
        assert_eq!(
            error.contains(&request_failure::update_refusal()),
            role == "user",
            "scoped/no-start tasks must not probe or autostart: {error}"
        );
        assert!(
            UnixStream::connect(&socket).is_err(),
            "no daemon was started"
        );
    }
}

/// Starts the owning task for `operation`.
fn start(app: &mut Cagents, operation: Operation) -> Task<Message> {
    match operation {
        Operation::Automation => app.update_automation_ui(crate::automation_ui::Action::Refresh),
        Operation::Pin { .. } => {
            drop(app.update_work_ui(crate::work_ui::Action::Watched(
                crate::work_ui::Watch::Board {
                    revision: 1,
                    first: true,
                    board: Arc::new(Board {
                        generated_at_unix_ms: 1_000,
                        stale_after_days: 7,
                        projects: Vec::new(),
                        items: vec![item()],
                        linear_unavailable: None,
                    }),
                },
            )));
            app.update_work_ui(crate::work_ui::Action::Pin(
                crate::work_ui::CardRef {
                    project: item().project,
                    key: item().key,
                },
                Some(Column::Stale),
            ))
        }
    }
}

#[test]
fn owning_automation_and_work_tasks_preserve_caller_authority_and_refusals() {
    if let Some(role) = std::env::var_os(ROLE) {
        exercise_app(role.to_str().unwrap());
        return;
    }
    let selector = thread::current().name().unwrap().to_owned();
    for role in ["present", "empty", "non-unicode", "user", "user-no-start"] {
        let private = tempfile::Builder::new()
            .prefix("ui-scope-")
            .tempdir_in("/var/tmp")
            .unwrap();
        for leaf in ["home", "runtime", "config", "data", "cache", "state"] {
            let path = private.path().join(leaf);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let stdout = private.path().join("stdout");
        let stderr = private.path().join("stderr");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
            .current_dir(private.path())
            .env(ROLE, role)
            .env(ROOT, private.path())
            .env("PATH", "/nonexistent-ui-scope-fixture-bin")
            .env("HOME", private.path().join("home"))
            .env("XDG_RUNTIME_DIR", private.path().join("runtime"))
            .env("XDG_CONFIG_HOME", private.path().join("config"))
            .env("XDG_DATA_HOME", private.path().join("data"))
            .env("XDG_CACHE_HOME", private.path().join("cache"))
            .env("XDG_STATE_HOME", private.path().join("state"))
            .stdin(Stdio::null())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap());
        if role == "user-no-start" {
            command.env("CAGENTS_NO_START", "YeS");
        }
        if !matches!(role, "user" | "user-no-start") {
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
                panic!("private owning-task child exceeded its bound: {role}");
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
    }
}
