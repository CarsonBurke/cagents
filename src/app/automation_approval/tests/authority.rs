//! Owning App tasks over a private framed peer. Synthetic scope proves client
//! transport fidelity and refusal handling, not production Host authorization.
use super::*;
use crate::{
    agent_execution::Authority,
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
    terminal_ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
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

const ROLE: &str = "CAGENTS_TEST_APPROVAL_AUTHORITY_ROLE";
const ROOT: &str = "CAGENTS_TEST_APPROVAL_PRIVATE_ROOT";
const TOKEN: &str = "synthetic-approval-original-scope";
const REFUSAL: &str = "Synthetic approval peer refused the captured scope";

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("approval-authority-fixture"),
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

fn respond(peer: &mut UnixStream, request: &ClientEnvelope, response: Response) {
    reply(
        peer,
        request,
        ServerMessage::Response {
            response: ServerResponse::Automation(response),
        },
    );
}

fn operation(request: &ClientEnvelope, authority: &Authority) -> Request {
    assert_eq!(request.proof.as_ref(), Some(&proof()));
    let ClientRequest::Automation {
        authority: actual,
        operation,
    } = &request.request
    else {
        panic!("the App must send an automation request");
    };
    assert_eq!(
        actual, authority,
        "approval transport must retain inherited scope instead of User"
    );
    operation.clone()
}

struct Peer {
    worker: Option<JoinHandle<UnixListener>>,
}

impl Peer {
    fn new(proposal: Proposal, approved: Option<bool>, authority: Authority) -> Self {
        let socket = crate::terminal::ipc::daemon_socket_path().unwrap();
        assert!(socket.starts_with(PathBuf::from(std::env::var_os(ROOT).unwrap())));
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        // Each effect finishes before the next fixture binds the same private
        // endpoint. No listener, production socket or detached worker survives.
        if socket.exists() {
            fs::remove_file(&socket).unwrap();
        }
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut probes = 0;
            let (mut peer, request) = loop {
                let (mut peer, _) = loop {
                    match listener.accept() {
                        Ok(accepted) => break accepted,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "App did not connect");
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("private listener: {error}"),
                    }
                };
                peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                peer.set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                assert!(matches!(hello.request, ClientRequest::Connect { .. }));
                assert!(hello.proof.is_none());
                reply(
                    &mut peer,
                    &hello,
                    ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof(),
                            resumed: Vec::new(),
                        },
                    },
                );
                if let Some(request) = read_frame::<ClientEnvelope>(&mut peer).unwrap() {
                    break (peer, request);
                }
                // Baseline ensure_daemon_ready verifies the protocol on a
                // separate Connect/EOF stream. Accept it so a before failure
                // exposes discarded authority, rather than missing setup.
                probes += 1;
                assert!(probes <= 1, "no repeated readiness or User fallback");
            };
            let expected = match approved {
                None => Request::PendingProposals {
                    repository: proposal.definition.agent.target.repository.clone(),
                    after: None,
                    limit: 1,
                },
                Some(approved) => Request::ResolveProposal {
                    id: proposal.id,
                    revision: proposal.revision,
                    approved,
                },
            };
            assert_eq!(operation(&request, &authority), expected);
            if authority.token().is_some() && approved.is_some() {
                reply(
                    &mut peer,
                    &request,
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::PermissionDenied,
                        REFUSAL,
                        false,
                    )),
                );
            } else if let Some(approved) = approved {
                let mut receipt = proposal;
                receipt.state = if approved {
                    ProposalState::Approved {
                        automation: uuid::Uuid::new_v4(),
                        execution_revision: 1,
                    }
                } else {
                    ProposalState::Declined
                };
                respond(&mut peer, &request, Response::Proposal(Box::new(receipt)));
            } else {
                // Accept even scoped listing synthetically to exercise detail
                // transport authority. This does not simulate Host policy.
                respond(
                    &mut peer,
                    &request,
                    Response::Proposals(vec![(&proposal).into()]),
                );
                // Detail must use this same business connection and authority,
                // rather than recapturing scope or reconnecting as User.
                let detail: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                assert_eq!(
                    operation(&detail, &authority),
                    Request::Proposal { id: proposal.id }
                );
                if authority.token().is_some() {
                    reply(
                        &mut peer,
                        &detail,
                        ServerMessage::Error(ProtocolError::new(
                            ProtocolErrorCode::PermissionDenied,
                            REFUSAL,
                            false,
                        )),
                    );
                } else {
                    respond(&mut peer, &detail, Response::Proposal(Box::new(proposal)));
                }
            }
            assert!(
                read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                "no request or User retry after the expected completion"
            );
            assert_eq!(
                probes,
                usize::from(authority.token().is_none()),
                "scoped effects must not run readiness or autostart"
            );
            listener
        });
        Self {
            worker: Some(worker),
        }
    }

    fn finished(mut self) {
        let listener = self.worker.take().unwrap().join().unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "the App must not open a fallback connection"
        );
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn complete(runtime: &tokio::runtime::Runtime, task: Task<Message>) -> Action {
    runtime.block_on(async {
        let mut stream = iced::runtime::task::into_stream(task).expect("App task must run");
        tokio::time::timeout(Duration::from_secs(8), async {
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(
                    Message::AutomationApproval(action),
                )) = action
                {
                    return action;
                }
            }
            panic!("approval task did not produce its owning completion");
        })
        .await
        .expect("private approval task exceeded its bound")
    })
}

fn exercise_app() {
    let private =
        PathBuf::from(std::env::var_os(ROOT).expect("only isolated children run App tasks"));
    assert_eq!(
        std::env::var_os("XDG_RUNTIME_DIR"),
        Some(private.join("runtime").into_os_string())
    );
    let authority = Authority::from_environment();
    let scoped = authority.token().is_some();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for approved in [None, Some(false), Some(true)] {
        let (mut app, proposal) = reviewing();
        let action = if let Some(approved) = approved {
            Action::Decide {
                id: proposal.id,
                revision: proposal.revision,
                generation: app.automation_approval.generation,
                approved,
            }
        } else {
            drop(app.cancel_automation_approval());
            app.automation_approval.available = None;
            Action::Tick
        };
        let peer = Peer::new(proposal.clone(), approved, authority.clone());
        let task = app.update_automation_approval(action);
        let completion = complete(&runtime, task);
        match &completion {
            Action::Read { result, .. } => {
                assert_eq!(approved, None);
                if scoped {
                    assert!(result.as_ref().as_ref().unwrap_err().contains(REFUSAL));
                } else {
                    assert_eq!(result.as_ref().as_ref().unwrap(), &vec![proposal.clone()]);
                }
            }
            Action::Finished { result, .. } => {
                assert!(approved.is_some());
                if scoped {
                    assert!(result.as_ref().as_ref().unwrap_err().contains(REFUSAL));
                } else {
                    assert_ne!(
                        result.as_ref().as_ref().unwrap().state,
                        ProposalState::Pending
                    );
                }
            }
            _ => panic!("unexpected App completion"),
        }
        drop(app.update_automation_approval(completion));
        assert!(!app.automation_approval.reading);
        assert!(!app.automation_approval.deciding);
        if approved.is_none() {
            assert!(!app.automation_approval_open());
            assert_eq!(
                app.automation_approval.available,
                (!scoped).then_some(proposal)
            );
        } else if scoped {
            assert_eq!(app.automation_approval.proposal, Some(proposal.clone()));
            assert_eq!(app.automation_approval.available, Some(proposal));
            assert!(
                app.automation_approval
                    .error
                    .as_ref()
                    .unwrap()
                    .contains(REFUSAL)
            );
        } else {
            assert!(!app.automation_approval_open());
            assert!(app.automation_approval.available.is_none());
        }
        peer.finished();
    }
}

#[test]
fn owning_app_approval_tasks_preserve_inherited_authority_and_user_flow() {
    if let Some(role) = std::env::var_os(ROLE) {
        assert!(["user", "present", "empty", "non-unicode"].contains(&role.to_str().unwrap()));
        exercise_app();
        return;
    }
    let selector = thread::current().name().unwrap().to_owned();
    for role in ["user", "present", "empty", "non-unicode"] {
        let private = tempfile::Builder::new()
            .prefix("approval-")
            .tempdir()
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
            .env("PATH", "/nonexistent-approval-fixture-bin")
            .env("HOME", private.path().join("home"))
            .env("XDG_RUNTIME_DIR", private.path().join("runtime"))
            .env("XDG_CONFIG_HOME", private.path().join("config"))
            .env("XDG_DATA_HOME", private.path().join("data"))
            .env("XDG_CACHE_HOME", private.path().join("cache"))
            .env("XDG_STATE_HOME", private.path().join("state"))
            .stdin(Stdio::null())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap());
        if role != "user" {
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
                panic!("private approval child exceeded its bound: {role}");
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
