#![cfg(all(feature = "host", target_os = "linux"))]

//! A private daemon serves a client of an older protocol it lists, the way a
//! chat carried across an update keeps running the CLI it launched with.
//!
//! The client here writes and reads raw frames at protocol 128, as the
//! 0.1.3+local.c68275e94 CLI does; it never uses this release's client.
use cagents::{
    agent_execution::{Authority, Operation, Reply},
    model::HostSessionProof,
    terminal_ipc::{
        CANDIDATE_LINE_PROTOCOL, ClientEnvelope, ClientRequest, PROTOCOL_VERSION,
        ProtocolErrorCode, ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
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

/// A connection that speaks one protocol number, frame by frame.
struct RawClient {
    stream: UnixStream,
    protocol: u16,
    next_request_id: u64,
    proof: Option<HostSessionProof>,
}

impl RawClient {
    fn connect(socket: &Path, protocol: u16, daemon: &mut PrivateDaemon) -> Self {
        let deadline = Instant::now() + Duration::from_secs(20);
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(stream) => break stream,
                Err(error) => {
                    assert!(
                        daemon.0.try_wait().unwrap().is_none(),
                        "the private daemon exited before readiness"
                    );
                    assert!(Instant::now() < deadline, "daemon readiness: {error}");
                    thread::sleep(Duration::from_millis(10));
                }
            }
        };
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
            if envelope.request_id == Some(request_id) {
                return envelope.message;
            }
        }
    }

    fn handshake(&mut self) -> ServerMessage {
        let message = self.request(ClientRequest::Connect {
            expected_host: None,
            resume: Vec::new(),
        });
        if let ServerMessage::Response {
            response: ServerResponse::Connected { proof, .. },
        } = &message
        {
            self.proof = Some(proof.clone());
        }
        message
    }
}

#[test]
fn a_candidate_line_client_is_answered_in_its_own_protocol() {
    assert_ne!(CANDIDATE_LINE_PROTOCOL, PROTOCOL_VERSION);
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let environment = private_environment(root);
    for (_, path) in &environment {
        fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_cagentsd"));
    for (name, path) in &environment {
        command.env(name, path);
    }
    let mut daemon = PrivateDaemon(
        command
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(root.join("daemon.log")).unwrap())
            .spawn()
            .unwrap(),
    );
    let socket = root
        .join("runtime")
        .join(cagents::instance::RUNTIME_SUBDIRECTORY)
        .join(cagents::terminal_ipc::SOCKET_FILE_NAME);

    let mut older = RawClient::connect(&socket, CANDIDATE_LINE_PROTOCOL, &mut daemon);
    assert!(
        matches!(
            older.handshake(),
            ServerMessage::Response {
                response: ServerResponse::Connected { .. }
            }
        ),
        "{}",
        fs::read_to_string(root.join("daemon.log")).unwrap_or_default()
    );
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
    assert!(matches!(
        older.request(ClientRequest::ChatList),
        ServerMessage::Response {
            response: ServerResponse::ChatListed { .. }
        }
    ));

    // A client of this protocol shares the daemon unchanged.
    let mut current = RawClient::connect(&socket, PROTOCOL_VERSION, &mut daemon);
    assert!(matches!(
        current.handshake(),
        ServerMessage::Response {
            response: ServerResponse::Connected { .. }
        }
    ));
    assert!(matches!(
        current.request(ClientRequest::ChatList),
        ServerMessage::Response {
            response: ServerResponse::ChatListed { .. }
        }
    ));

    // A connection keeps the protocol it opened with: a frame in another
    // version is refused, in the connection's own, and ends it.
    let switched = older.send_at(PROTOCOL_VERSION, ClientRequest::ChatList);
    match older.reply(switched) {
        ServerMessage::Error(error) => {
            assert_eq!(error.code, ProtocolErrorCode::UnsupportedVersion);
            assert!(
                error
                    .message
                    .contains(&format!("expected {CANDIDATE_LINE_PROTOCOL}")),
                "{error:?}"
            );
        }
        other => panic!("a switched version was answered: {other:?}"),
    }
    assert!(
        read_frame::<ServerEnvelope>(&mut older.stream)
            .map(|frame| frame.is_none())
            .unwrap_or(true)
    );

    // A protocol the daemon does not serve is refused as before.
    let mut unserved = RawClient::connect(&socket, CANDIDATE_LINE_PROTOCOL - 1, &mut daemon);
    let request_id = unserved.send_at(
        unserved.protocol,
        ClientRequest::Connect {
            expected_host: None,
            resume: Vec::new(),
        },
    );
    let envelope: ServerEnvelope = read_frame(&mut unserved.stream).unwrap().unwrap();
    assert_eq!(envelope.version, PROTOCOL_VERSION);
    assert_eq!(envelope.request_id, Some(request_id));
    assert!(matches!(
        envelope.message,
        ServerMessage::Error(error) if error.code == ProtocolErrorCode::UnsupportedVersion
    ));
}
