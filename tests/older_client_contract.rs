#![cfg(all(feature = "host", target_os = "linux"))]

//! A private daemon serves a client of an older protocol it lists, the way a
//! chat carried across an update keeps running the CLI it launched with.
//!
//! The client here writes and reads raw frames at protocol 128, as the
//! 0.1.3+local.c68275e94 CLI does; it never uses this release's client. The
//! daemon serves only the `cagents` executable at an older protocol, so that
//! client runs as this test binary linked under that name.
use cagents::{
    agent_execution::{Authority, Operation, Reply},
    automations,
    model::HostSessionProof,
    terminal_ipc::{
        CANDIDATE_LINE_PROTOCOL, ClientEnvelope, ClientRequest, PROTOCOL_VERSION,
        ProtocolErrorCode, SHUTDOWN_VERSION, ServerEnvelope, ServerMessage, ServerResponse,
        ShutdownOperation, read_frame, write_frame,
    },
};
use std::{
    fs,
    os::unix::{fs::DirBuilderExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct PrivateDaemon(Child);

impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn private_environment(root: &Path) -> Vec<(&'static str, PathBuf)> {
    [
        ("HOME", "home"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("CLAUDE_CONFIG_DIR", "claude"),
        ("CODEX_HOME", "codex"),
    ]
    .into_iter()
    .map(|(name, relative)| (name, root.join(relative)))
    .collect()
}

/// Names the older client's socket in the child process the main test starts.
const CHILD_SOCKET: &str = "CAGENTS_TEST_OLDER_CLIENT_SOCKET";

/// This test binary, linked as `cagents` so the daemon sees the older CLI.
struct CliImage {
    directory: PathBuf,
    executable: PathBuf,
}

impl CliImage {
    fn link() -> Self {
        let test = std::env::current_exe().unwrap();
        let directory = test
            .parent()
            .unwrap()
            .join(format!("older-client-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).unwrap();
        let executable = directory.join("cagents");
        fs::hard_link(&test, &executable)
            .or_else(|_| fs::copy(&test, &executable).map(drop))
            .unwrap();
        Self {
            directory,
            executable,
        }
    }
}

impl Drop for CliImage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// A connection that speaks one protocol number, frame by frame.
struct RawClient {
    stream: UnixStream,
    protocol: u16,
    next_request_id: u64,
    proof: Option<HostSessionProof>,
}

impl RawClient {
    fn connect(socket: &Path, protocol: u16) -> Self {
        let stream = UnixStream::connect(socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Self {
            stream,
            protocol,
            next_request_id: 1,
            proof: None,
        }
    }

    /// Connects once the daemon listens.
    fn when_ready(socket: &Path, protocol: u16, daemon: &mut PrivateDaemon, log: &Path) -> Self {
        let deadline = Instant::now() + Duration::from_secs(20);
        while UnixStream::connect(socket).is_err() {
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "the private daemon exited before readiness: {}",
                fs::read_to_string(log).unwrap_or_default()
            );
            assert!(Instant::now() < deadline, "daemon readiness");
            thread::sleep(Duration::from_millis(10));
        }
        Self::connect(socket, protocol)
    }

    fn send_at(&mut self, version: u16, request: ClientRequest) -> u64 {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        write_frame(
            &mut self.stream,
            &ClientEnvelope {
                version,
                request_id,
                proof: self.proof.clone(),
                request,
            },
        )
        .unwrap();
        request_id
    }

    /// The reply to `request`; every frame on the way must be in this
    /// connection's version.
    fn request(&mut self, request: ClientRequest) -> ServerMessage {
        let request_id = self.send_at(self.protocol, request);
        self.reply(request_id)
    }

    fn reply(&mut self, request_id: u64) -> ServerMessage {
        loop {
            let envelope: ServerEnvelope = read_frame(&mut self.stream)
                .unwrap()
                .expect("the daemon closed the connection before replying");
            assert_eq!(envelope.version, self.protocol, "{envelope:?}");
            if self.protocol == CANDIDATE_LINE_PROTOCOL {
                assert!(
                    !matches!(
                        &envelope.message,
                        ServerMessage::WorkspaceModelEdited { .. }
                    ),
                    "an older watch must be refused before subscribing: {envelope:?}"
                );
            }
            if envelope.request_id == Some(request_id) {
                return envelope.message;
            }
        }
    }

    fn connect_request() -> ClientRequest {
        ClientRequest::Connect {
            expected_host: None,
            resume: Vec::new(),
        }
    }

    fn handshake(&mut self) {
        match self.request(Self::connect_request()) {
            ServerMessage::Response {
                response: ServerResponse::Connected { proof, .. },
            } => self.proof = Some(proof),
            other => panic!("unexpected handshake reply: {other:?}"),
        }
    }

    /// The daemon's answer to a first frame at a protocol it does not serve
    /// this peer: an error in its own version, after which it closes.
    fn refused_handshake(mut self) -> ServerEnvelope {
        let request_id = self.send_at(self.protocol, Self::connect_request());
        let envelope: ServerEnvelope = read_frame(&mut self.stream).unwrap().unwrap();
        assert_eq!(envelope.request_id, Some(request_id));
        envelope
    }
}

fn unsupported_version(message: &ServerMessage) -> &str {
    match message {
        ServerMessage::Error(error) if error.code == ProtocolErrorCode::UnsupportedVersion => {
            &error.message
        }
        other => panic!("expected an unsupported-version error: {other:?}"),
    }
}

/// The older CLI's side, run only in the child the main test starts.
#[test]
fn older_cli_session() {
    let Some(socket) = std::env::var_os(CHILD_SOCKET) else {
        return;
    };
    let socket = PathBuf::from(socket);
    let mut older = RawClient::connect(&socket, CANDIDATE_LINE_PROTOCOL);
    older.handshake();
    // What `cagents agent list` sends from a session outside any agent scope.
    assert!(matches!(
        older.request(ClientRequest::Authenticate {
            authority: Authority::default(),
        }),
        ServerMessage::Response {
            response: ServerResponse::Accepted
        }
    ));
    match older.request(ClientRequest::Agent {
        authority: Authority::default(),
        operation: Operation::List,
    }) {
        ServerMessage::Response {
            response: ServerResponse::Agent(Reply::Listed(agents)),
        } => assert!(agents.is_empty()),
        other => panic!("unexpected agent list reply: {other:?}"),
    }

    // A model watch now delivers a snapshot with closed identities and edges
    // with scope revisions. Refuse that watch before it subscribes, while
    // keeping the old explicit read/write requests and full replies intact.
    let refused = older.request(ClientRequest::WatchWorkspaceModel);
    assert!(
        unsupported_version(&refused).contains("Start a new session"),
        "{refused:?}"
    );
    let repository = socket.parent().unwrap().to_owned();
    let edited = match older.request(ClientRequest::EditWorkspaceModel {
        edit: cagents::workspace_model::ModelEdit::OpenProject {
            repository: repository.clone(),
            name: "Legacy client workspace".into(),
            kind: cagents::model::ProjectKind::Folder,
        },
    }) {
        ServerMessage::Response {
            response: ServerResponse::WorkspaceModel { model },
        } => model,
        other => panic!("the older edit must retain its full-model reply: {other:?}"),
    };
    assert_eq!(edited.projects.len(), 1);
    assert_eq!(edited.projects[0].repository, repository);
    match older.request(ClientRequest::ReadWorkspaceModel) {
        ServerMessage::Response {
            response: ServerResponse::WorkspaceModel { model },
        } => assert_eq!(model, edited),
        other => panic!("the older read must retain its full-model reply: {other:?}"),
    }
    // A different client commits through the new delta API. The refused old
    // watch must leave no changed notification ahead of its next legacy reply.
    let mut current = RawClient::connect(&socket, PROTOCOL_VERSION);
    current.handshake();
    assert!(matches!(
        current.request(ClientRequest::ApplyWorkspaceEdit {
            edit: cagents::workspace_model::ModelEdit::OpenProject {
                repository,
                name: "Updated by the current client".into(),
                kind: cagents::model::ProjectKind::Folder,
            },
        }),
        ServerMessage::Response {
            response: ServerResponse::WorkspaceEdit(_)
        }
    ));
    match older.request(ClientRequest::ReadWorkspaceModel) {
        ServerMessage::Response {
            response: ServerResponse::WorkspaceModel { model },
        } => assert_eq!(model.projects[0].name, "Updated by the current client"),
        other => panic!("the older read must survive a concurrent current write: {other:?}"),
    }

    // It may not stop or update a daemon it was not built for; the refusal
    // answers that request and the connection stays usable.
    let refused = older.request(ClientRequest::ShutdownV1 {
        version: SHUTDOWN_VERSION,
        operation: ShutdownOperation::Preflight,
    });
    assert!(
        unsupported_version(&refused).contains("Start a new session"),
        "{refused:?}"
    );

    // A reply holding a Firing changed shape after this protocol. A run-now
    // refused only at its reply would have started anyway.
    for operation in [
        automations::Request::RunNow {
            id: uuid::Uuid::nil(),
            execution_revision: 0,
            request: uuid::Uuid::nil(),
            confirmed: true,
        },
        automations::Request::History {
            id: None,
            before: None,
            limit: 1,
        },
    ] {
        let refused = older.request(ClientRequest::Automation {
            authority: Authority::default(),
            operation,
        });
        assert!(
            unsupported_version(&refused).contains("Start a new session"),
            "{refused:?}"
        );
    }
    assert!(matches!(
        older.request(ClientRequest::ChatList),
        ServerMessage::Response {
            response: ServerResponse::ChatListed { .. }
        }
    ));

    // A connection keeps the protocol it opened with: a frame in another
    // version is refused, in the connection's own, and ends it.
    let switched = older.send_at(PROTOCOL_VERSION, ClientRequest::ChatList);
    let reply = older.reply(switched);
    assert!(
        unsupported_version(&reply).contains(&format!("expected {CANDIDATE_LINE_PROTOCOL}")),
        "{reply:?}"
    );
    assert!(
        read_frame::<ServerEnvelope>(&mut older.stream)
            .map(|frame| frame.is_none())
            .unwrap_or(true)
    );
}

#[test]
fn a_candidate_line_cli_is_answered_in_its_own_protocol() {
    if std::env::var_os(CHILD_SOCKET).is_some() {
        return;
    }
    assert_ne!(CANDIDATE_LINE_PROTOCOL, PROTOCOL_VERSION);
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let environment = private_environment(root);
    for (_, path) in &environment {
        fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    }
    let log = root.join("daemon.log");
    let mut command = Command::new(env!("CARGO_BIN_EXE_cagentsd"));
    command.env_clear();
    for (name, path) in &environment {
        command.env(name, path);
    }
    let mut daemon = PrivateDaemon(
        command
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let socket = root
        .join("runtime")
        .join(cagents::instance::RUNTIME_SUBDIRECTORY)
        .join(cagents::terminal_ipc::SOCKET_FILE_NAME);

    // A client of this protocol is served as before.
    let mut current = RawClient::when_ready(&socket, PROTOCOL_VERSION, &mut daemon, &log);
    current.handshake();
    assert!(matches!(
        current.request(ClientRequest::ChatList),
        ServerMessage::Response {
            response: ServerResponse::ChatListed { .. }
        }
    ));

    // The older CLI, running as `cagents`, alongside it.
    let image = CliImage::link();
    let child = Command::new(&image.executable)
        .env_clear()
        .env(CHILD_SOCKET, &socket)
        .args([
            "--exact",
            "older_cli_session",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(
        child.status.success() && output.contains("1 passed"),
        "{output}\n{}",
        fs::read_to_string(&log).unwrap_or_default()
    );

    // The same protocol from any other executable, such as a window an update
    // superseded, is refused at its handshake, as it was before.
    let window = RawClient::connect(&socket, CANDIDATE_LINE_PROTOCOL).refused_handshake();
    assert_eq!(window.version, PROTOCOL_VERSION);
    assert!(unsupported_version(&window.message).contains(&format!("expected {PROTOCOL_VERSION}")));

    // So is a protocol the daemon does not list.
    let unserved = RawClient::connect(&socket, CANDIDATE_LINE_PROTOCOL - 1).refused_handshake();
    assert_eq!(unserved.version, PROTOCOL_VERSION);
    unsupported_version(&unserved.message);

    // The current client was undisturbed throughout.
    assert!(matches!(
        current.request(ClientRequest::ChatList),
        ServerMessage::Response {
            response: ServerResponse::ChatListed { .. }
        }
    ));
}
