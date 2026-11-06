//! Actual notification App task over private IPC. Synthetic authority checks
//! client fidelity; the peer does not establish production Host authorization.
use super::*;
use crate::{
    agent_execution::Authority,
    automations::{LifecyclePage, Request, Response},
    terminal_ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
    workspace_model::WorkspaceModel,
};
use cosmic::iced::futures::StreamExt;
use std::{
    fs::{self, File},
    io,
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    process::{Command, Stdio},
    thread::JoinHandle,
};

const ROLE: &str = "CAGENTS_TEST_NOTIFICATION_AUTHORITY_ROLE";
const ROOT: &str = "CAGENTS_TEST_NOTIFICATION_PRIVATE_ROOT";
const TOKEN: &str = "synthetic-notification-original-scope";

fn reply(
    peer: &mut UnixStream,
    request: &ClientEnvelope,
    proof: &HostSessionProof,
    message: ServerMessage,
) {
    write_frame(
        peer,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request.request_id),
            proof: proof.clone(),
            message,
        },
    )
    .unwrap();
}

fn next(peer: &mut UnixStream, proof: &HostSessionProof) -> ClientEnvelope {
    let request: ClientEnvelope = read_frame(peer).unwrap().unwrap();
    assert_eq!(request.proof.as_ref(), Some(proof));
    request
}

fn read_model(peer: &mut UnixStream, proof: &HostSessionProof, model: &WorkspaceModel) {
    let request = next(peer, proof);
    assert!(matches!(request.request, ClientRequest::ReadWorkspaceModel));
    reply(
        peer,
        &request,
        proof,
        ServerMessage::Response {
            response: ServerResponse::WorkspaceModel {
                model: model.clone(),
            },
        },
    );
}

struct Peer(Option<JoinHandle<UnixListener>>);

impl Peer {
    fn new(
        host: HostSessionProof,
        registry: uuid::Uuid,
        id: uuid::Uuid,
        sequence: u64,
        authority: Authority,
    ) -> Self {
        let socket = crate::terminal::ipc::daemon_socket_path().unwrap();
        assert!(socket.starts_with(PathBuf::from(std::env::var_os(ROOT).unwrap())));
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        // Create only: no unlink or takeover of an existing socket, even in
        // this private child. The parent never opens its ambient daemon path.
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self(Some(thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut peer = loop {
                match listener.accept() {
                    Ok((peer, _)) => break peer,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "App did not connect");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("private notification listener: {error}"),
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
                &host,
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: host.clone(),
                        resumed: Vec::new(),
                    },
                },
            );
            let model = WorkspaceModel {
                identity: registry,
                ..WorkspaceModel::default()
            };
            // This observer uses an existing endpoint. A separate readiness
            // Connect/EOF probe would fail here instead of starting a daemon.
            read_model(&mut peer, &host, &model);
            let request = next(&mut peer, &host);
            let ClientRequest::Automation {
                authority: actual,
                operation,
            } = &request.request
            else {
                panic!("notification activation must read its audit receipt");
            };
            assert_eq!(
                actual, &authority,
                "notification audit must retain inherited scope instead of User"
            );
            assert_eq!(
                operation,
                &Request::Audit {
                    id,
                    before: Some(sequence + 1),
                    limit: 1,
                }
            );
            if authority.token().is_some() {
                reply(
                    &mut peer,
                    &request,
                    &host,
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::PermissionDenied,
                        "Synthetic notification scope refused",
                        false,
                    )),
                );
            } else {
                reply(
                    &mut peer,
                    &request,
                    &host,
                    ServerMessage::Response {
                        response: ServerResponse::Automation(Response::Audit(exact_notice_page(
                            id, sequence,
                        ))),
                    },
                );
                // Preserve the second registry check on the same connection.
                read_model(&mut peer, &host, &model);
            }
            assert!(
                read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                "no extra request or User retry after refusal/completion"
            );
            listener
        })))
    }

    fn finished(mut self) {
        let listener = self.0.take().unwrap().join().unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "no fallback or readiness connection"
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

fn complete(task: Task<Message>) -> (u64, Result<LifecyclePage, ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut stream = iced::runtime::task::into_stream(task).expect("App task must run");
            tokio::time::timeout(Duration::from_secs(8), async {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(
                        Message::NotificationUi(Action::LifecycleResolved { generation, result }),
                    )) = action
                    {
                        return (generation, result);
                    }
                }
                panic!("notification task did not return its owning completion");
            })
            .await
            .expect("private notification task exceeded its bound")
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
    let (mut app, id, sequence, notice) = lifecycle_notice_fixture();
    let peer = Peer::new(
        app.notification_ui.host.clone().unwrap(),
        app.workspace_model.identity,
        id,
        sequence,
        authority,
    );
    let cards = app.durable.cards.clone();
    let focused = app.workspace.focused();
    let notices = app.notification_ui.snapshot.clone();
    let task = app.update_notifications(Action::Activated(Target::Automation { id, sequence }));
    let generation = app
        .notification_ui
        .lifecycle_read
        .expect("read owns a generation");
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.notification_ui.snapshot, notices);
    let (actual_generation, result) = complete(task);
    assert_eq!(actual_generation, generation);
    assert_eq!(
        result,
        if scoped {
            Err(())
        } else {
            Ok(exact_notice_page(id, sequence))
        }
    );
    // Reading alone cannot open cards, consume a notice, or retire the pending
    // generation. The App must apply the owning completion separately.
    assert_eq!(app.notification_ui.lifecycle_read, Some(generation));
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.workspace.focused(), focused);
    assert_eq!(app.notification_ui.snapshot, notices);
    assert!(!app.notification_ui.acknowledged_for_test(notice));
    peer.finished();
}

#[test]
fn owning_notification_audit_task_preserves_inherited_authority_and_identity_checks() {
    if let Some(role) = std::env::var_os(ROLE) {
        assert!(["user", "present", "empty"].contains(&role.to_str().unwrap()));
        exercise_app();
        return;
    }
    let selector = thread::current().name().unwrap().to_owned();
    for role in ["user", "present", "empty"] {
        let private = tempfile::Builder::new()
            .prefix("notice-")
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
            .env("PATH", "/nonexistent-notification-fixture-bin")
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
                if role == "present" { TOKEN } else { "" },
            );
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("private notification child exceeded its bound: {role}");
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
