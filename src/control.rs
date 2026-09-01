use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{ChildStdout, Command, ExitCode, Stdio},
    thread,
    time::{Duration, Instant},
};

use clap::{Parser, Subcommand, error::ErrorKind as ClapErrorKind};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;

use crate::{
    instance::{self, UI_LAUNCH_ERROR_PREFIX, UI_LAUNCH_HANDSHAKE_ENV, UI_LAUNCH_READY},
    model::{HostSessionProof, LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    terminal::{
        self, TerminalSpawnOptions,
        ipc::{
            ClientEnvelope, ClientRequest, FrameError, PROTOCOL_VERSION, ProtocolErrorCode,
            ServerEnvelope, ServerMessage, ServerResponse, TerminalDescriptor, TerminalSnapshot,
            TerminalVisibility, daemon_socket_path, read_frame, write_frame,
        },
    },
};

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const SNAPSHOT_RETRY_INTERVAL: Duration = Duration::from_millis(25);
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const UI_LAUNCH_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_UI_LAUNCH_HANDSHAKE_BYTES: usize = 4096;
const MAX_WAIT_SECONDS: f64 = 24.0 * 60.0 * 60.0;

#[derive(Debug, Parser)]
#[command(
    name = "cagents",
    version,
    about = "Control cagents terminals and processes"
)]
pub struct Cli {
    /// Emit stable JSON envelopes for automation.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: TopCommand,
}

#[derive(Debug, Subcommand)]
pub enum TopCommand {
    /// Launch the native cagents UI and return immediately.
    Ui,
    /// Manage the persistent cagents daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Manage daemon-hosted terminal sessions.
    Terminal {
        #[command(subcommand)]
        command: TerminalCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Start the daemon if it is not already running.
    Start,
    /// Report whether the daemon is running.
    Status,
    /// Stop the daemon and its hosted resources.
    Stop,
    /// Stop any development daemon without relying on its wire protocol.
    #[command(hide = true)]
    StopForRebuild,
}

#[derive(Debug, Subcommand)]
pub enum TerminalCommand {
    /// List terminal descriptors known to the daemon.
    List,
    /// Spawn a terminal with an optional argv-based command.
    Spawn {
        /// Working directory for the terminal (defaults to the current directory).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Program and arguments after `--`. Omit to launch the user's standard shell.
        #[arg(last = true, num_args = 1.., value_name = "COMMAND")]
        argv: Vec<String>,
    },
    /// Write UTF-8 text to a terminal.
    Write {
        terminal_id: TerminalId,
        text: String,
    },
    /// Explicitly terminate a terminal.
    Close { terminal_id: TerminalId },
    /// Read a current immutable terminal snapshot.
    Snapshot { terminal_id: TerminalId },
    /// Wait until a current terminal snapshot contains text.
    Wait {
        terminal_id: TerminalId,
        #[arg(long)]
        text: String,
        /// Maximum time to wait, in seconds.
        #[arg(long, default_value_t = 30.0, value_parser = parse_positive_seconds)]
        timeout: f64,
    },
}

fn parse_positive_seconds(value: &str) -> Result<f64, String> {
    let seconds = value
        .parse::<f64>()
        .map_err(|_| "timeout must be a number of seconds".to_owned())?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err("timeout must be a finite number greater than zero".to_owned());
    }
    if seconds > MAX_WAIT_SECONDS {
        return Err("timeout must not exceed 86400 seconds".to_owned());
    }
    Ok(seconds)
}

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("could not connect to the daemon: {0}")]
    Connect(#[source] io::Error),
    #[error("could not configure the daemon connection: {0}")]
    Configure(#[source] io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("the daemon closed the connection before responding")]
    ConnectionClosed,
    #[error("daemon protocol version {actual} is unsupported; expected {PROTOCOL_VERSION}")]
    UnsupportedVersion { actual: u16 },
    #[error("the daemon returned an incomplete session proof")]
    IncompleteProof,
    #[error("the daemon response proof does not match the pinned session proof")]
    ProofMismatch,
    #[error("daemon response request ID {actual:?} does not match {expected}")]
    RequestIdMismatch { expected: u64, actual: Option<u64> },
    #[error("unexpected daemon message while waiting for {expected}: {actual}")]
    UnexpectedMessage {
        expected: &'static str,
        actual: &'static str,
    },
    #[error("unexpected daemon response while waiting for {expected}: {actual}")]
    UnexpectedResponse {
        expected: &'static str,
        actual: &'static str,
    },
    #[error("daemon rejected the request ({code:?}): {message}")]
    Remote {
        code: ProtocolErrorCode,
        message: String,
        retryable: bool,
    },
    #[error("daemon returned terminal {actual} for requested terminal {expected}")]
    TerminalMismatch {
        expected: TerminalId,
        actual: TerminalId,
    },
    #[error("the daemon did not publish a current terminal snapshot before the deadline")]
    SnapshotTimeout,
    #[error("timed out waiting for terminal {terminal_id} to contain {text:?}")]
    WaitTimeout {
        terminal_id: TerminalId,
        text: String,
    },
    #[error("the wait timeout is too large for the monotonic clock")]
    InvalidTimeout,
    #[error("daemon answered a full-snapshot request with an incremental one it could not resolve")]
    IncrementalSnapshot,
}

/// A bounded, synchronous client for the version 3 cagents daemon protocol.
pub struct ControlClient {
    stream: UnixStream,
    proof: HostSessionProof,
    next_request_id: u64,
}

impl ControlClient {
    /// Connect and complete a proof-pinning handshake with no expected proof.
    pub fn connect(socket_path: &Path) -> Result<Self, ControlError> {
        let stream = UnixStream::connect(socket_path).map_err(ControlError::Connect)?;
        Self::from_stream(stream)
    }

    fn from_stream(stream: UnixStream) -> Result<Self, ControlError> {
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .map_err(ControlError::Configure)?;
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .map_err(ControlError::Configure)?;
        let mut client = HandshakeClient {
            stream,
            next_request_id: 1,
        };
        let proof = client.handshake()?;
        Ok(Self {
            stream: client.stream,
            proof,
            next_request_id: client.next_request_id,
        })
    }

    pub fn proof(&self) -> &HostSessionProof {
        &self.proof
    }

    pub fn list(&mut self) -> Result<Vec<TerminalDescriptor>, ControlError> {
        match self.request_strict(ClientRequest::List)? {
            ServerResponse::Listed { terminals } => Ok(terminals),
            response => Err(unexpected_response("a terminal list", &response)),
        }
    }

    pub fn spawn(
        &mut self,
        session: TerminalSession,
        options: TerminalSpawnOptions,
    ) -> Result<TerminalDescriptor, ControlError> {
        let expected = session.id;
        let response =
            self.request_attached(ClientRequest::Spawn { session, options }, expected)?;
        let terminal = match response {
            ServerResponse::Spawned { terminal } if terminal.session.id == expected => terminal,
            ServerResponse::Spawned { terminal } => {
                return Err(ControlError::TerminalMismatch {
                    expected,
                    actual: terminal.session.id,
                });
            }
            response => return Err(unexpected_response("a spawned terminal", &response)),
        };
        self.read_attachment_replay(expected)?;
        Ok(terminal)
    }

    pub fn write(&mut self, terminal_id: TerminalId, bytes: Vec<u8>) -> Result<(), ControlError> {
        self.attach_and_synchronize(terminal_id)?;
        let action = self.expect_accepted(ClientRequest::Input { terminal_id, bytes }, terminal_id);
        self.finish_attachment(terminal_id, action)
    }

    pub fn close_terminal(&mut self, terminal_id: TerminalId) -> Result<(), ControlError> {
        self.attach_and_synchronize(terminal_id)?;
        let action =
            self.expect_accepted(ClientRequest::ShutdownTerminal { terminal_id }, terminal_id);
        self.finish_attachment(terminal_id, action)
    }

    pub fn snapshot(&mut self, terminal_id: TerminalId) -> Result<RenderedSnapshot, ControlError> {
        self.attach_and_synchronize(terminal_id)?;
        let visible = self.expect_accepted(
            ClientRequest::SetVisibility {
                terminal_id,
                visibility: TerminalVisibility::Visible,
            },
            terminal_id,
        );
        let result = match visible {
            Ok(()) => self.snapshot_until(terminal_id, Instant::now() + IO_TIMEOUT),
            Err(error) => Err(error),
        };
        self.finish_visible_attachment(terminal_id, result)
    }

    pub fn wait_for_text(
        &mut self,
        terminal_id: TerminalId,
        text: &str,
        timeout: Duration,
    ) -> Result<RenderedSnapshot, ControlError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(ControlError::InvalidTimeout)?;
        self.attach_and_synchronize(terminal_id)?;
        let visible = self.expect_accepted(
            ClientRequest::SetVisibility {
                terminal_id,
                visibility: TerminalVisibility::Visible,
            },
            terminal_id,
        );
        let result = match visible {
            Ok(()) => self.wait_attached(terminal_id, text, deadline),
            Err(error) => Err(error),
        };
        self.finish_visible_attachment(terminal_id, result)
    }

    pub fn shutdown_daemon(&mut self) -> Result<(), ControlError> {
        match self.request_strict(ClientRequest::Shutdown)? {
            ServerResponse::ShutdownAccepted => Ok(()),
            response => Err(unexpected_response("daemon shutdown acceptance", &response)),
        }
    }

    fn wait_attached(
        &mut self,
        terminal_id: TerminalId,
        text: &str,
        deadline: Instant,
    ) -> Result<RenderedSnapshot, ControlError> {
        loop {
            if Instant::now() >= deadline {
                return Err(ControlError::WaitTimeout {
                    terminal_id,
                    text: text.to_owned(),
                });
            }
            self.limit_read_timeout(deadline)?;
            match self.request_snapshot(terminal_id) {
                Ok(Some(snapshot)) if snapshot.text.contains(text) => {
                    self.restore_read_timeout()?;
                    return Ok(snapshot);
                }
                Ok(_) => {}
                Err(error) if is_timeout_error(&error) => {
                    return Err(ControlError::WaitTimeout {
                        terminal_id,
                        text: text.to_owned(),
                    });
                }
                Err(error) => return Err(error),
            }
            let now = Instant::now();
            if now < deadline {
                thread::sleep(WAIT_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
            }
        }
    }

    fn snapshot_until(
        &mut self,
        terminal_id: TerminalId,
        deadline: Instant,
    ) -> Result<RenderedSnapshot, ControlError> {
        loop {
            if Instant::now() >= deadline {
                return Err(ControlError::SnapshotTimeout);
            }
            self.limit_read_timeout(deadline)?;
            match self.request_snapshot(terminal_id) {
                Ok(Some(snapshot)) => {
                    self.restore_read_timeout()?;
                    return Ok(snapshot);
                }
                Ok(None) => {}
                Err(error) if is_timeout_error(&error) => {
                    return Err(ControlError::SnapshotTimeout);
                }
                Err(error) => return Err(error),
            }
            thread::sleep(SNAPSHOT_RETRY_INTERVAL);
        }
    }

    fn request_snapshot(
        &mut self,
        terminal_id: TerminalId,
    ) -> Result<Option<RenderedSnapshot>, ControlError> {
        let response = self.request_attached(
            ClientRequest::Snapshot {
                terminal_id,
                after_generation: 0,
            },
            terminal_id,
        )?;
        match response {
            ServerResponse::Snapshot {
                terminal_id: actual,
                snapshot,
            } if actual == terminal_id => {
                if let Some(snapshot) = snapshot {
                    let generation = snapshot.generation;
                    let rendered = RenderedSnapshot::from_snapshot(terminal_id, snapshot)?;
                    self.expect_accepted(
                        ClientRequest::AcknowledgeRedraw {
                            terminal_id,
                            generation,
                        },
                        terminal_id,
                    )?;
                    Ok(Some(rendered))
                } else {
                    Ok(None)
                }
            }
            ServerResponse::Snapshot {
                terminal_id: actual,
                ..
            } => Err(ControlError::TerminalMismatch {
                expected: terminal_id,
                actual,
            }),
            response => Err(unexpected_response("a terminal snapshot", &response)),
        }
    }

    fn attach_and_synchronize(
        &mut self,
        terminal_id: TerminalId,
    ) -> Result<TerminalDescriptor, ControlError> {
        let response = self.request_attached(
            ClientRequest::Attach {
                terminal_id,
                after_generation: 0,
                after_event_sequence: 0,
            },
            terminal_id,
        )?;
        let terminal = match response {
            ServerResponse::Attached { terminal } if terminal.session.id == terminal_id => terminal,
            ServerResponse::Attached { terminal } => {
                return Err(ControlError::TerminalMismatch {
                    expected: terminal_id,
                    actual: terminal.session.id,
                });
            }
            response => return Err(unexpected_response("a terminal attachment", &response)),
        };
        self.read_attachment_replay(terminal_id)?;
        Ok(terminal)
    }

    fn read_attachment_replay(&mut self, terminal_id: TerminalId) -> Result<(), ControlError> {
        loop {
            let envelope = self.read_envelope()?;
            self.validate_pinned_envelope(&envelope)?;
            if envelope.request_id.is_some() {
                return Err(ControlError::RequestIdMismatch {
                    expected: self.next_request_id,
                    actual: envelope.request_id,
                });
            }
            match envelope.message {
                ServerMessage::Redraw {
                    terminal_id: actual,
                    ..
                }
                | ServerMessage::Snapshot {
                    terminal_id: actual,
                    ..
                }
                | ServerMessage::Event {
                    terminal_id: actual,
                    ..
                } if actual == terminal_id => {}
                ServerMessage::Synchronized {
                    terminal_id: actual,
                    ..
                } if actual == terminal_id => return Ok(()),
                message => {
                    return Err(ControlError::UnexpectedMessage {
                        expected: "the terminal attachment replay",
                        actual: message_name(&message),
                    });
                }
            }
        }
    }

    fn finish_attachment<T>(
        &mut self,
        terminal_id: TerminalId,
        result: Result<T, ControlError>,
    ) -> Result<T, ControlError> {
        let detached = self.detach(terminal_id);
        match result {
            Err(error) => Err(error),
            Ok(value) => detached.map(|()| value),
        }
    }

    fn finish_visible_attachment<T>(
        &mut self,
        terminal_id: TerminalId,
        result: Result<T, ControlError>,
    ) -> Result<T, ControlError> {
        let restored = self.restore_read_timeout();
        let hidden = self.expect_accepted(
            ClientRequest::SetVisibility {
                terminal_id,
                visibility: TerminalVisibility::Hidden,
            },
            terminal_id,
        );
        let detached = self.detach(terminal_id);
        match result {
            Err(error) => Err(error),
            Ok(value) => {
                restored?;
                hidden?;
                detached?;
                Ok(value)
            }
        }
    }

    fn detach(&mut self, terminal_id: TerminalId) -> Result<(), ControlError> {
        match self.request_attached(ClientRequest::Detach { terminal_id }, terminal_id)? {
            ServerResponse::Detached => Ok(()),
            response => Err(unexpected_response("a terminal detach", &response)),
        }
    }

    fn expect_accepted(
        &mut self,
        request: ClientRequest,
        terminal_id: TerminalId,
    ) -> Result<(), ControlError> {
        match self.request_attached(request, terminal_id)? {
            ServerResponse::Accepted => Ok(()),
            response => Err(unexpected_response("request acceptance", &response)),
        }
    }

    fn request_strict(&mut self, request: ClientRequest) -> Result<ServerResponse, ControlError> {
        self.request(request, None)
    }

    fn request_attached(
        &mut self,
        request: ClientRequest,
        terminal_id: TerminalId,
    ) -> Result<ServerResponse, ControlError> {
        self.request(request, Some(terminal_id))
    }

    fn request(
        &mut self,
        request: ClientRequest,
        streaming_terminal: Option<TerminalId>,
    ) -> Result<ServerResponse, ControlError> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        if self.next_request_id == 0 {
            self.next_request_id = 1;
        }
        write_frame(
            &mut self.stream,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id,
                proof: Some(self.proof.clone()),
                request,
            },
        )?;

        loop {
            let envelope = self.read_envelope()?;
            self.validate_pinned_envelope(&envelope)?;
            match envelope.request_id {
                Some(actual) if actual != request_id => {
                    return Err(ControlError::RequestIdMismatch {
                        expected: request_id,
                        actual: Some(actual),
                    });
                }
                Some(_) => return response_from_message(envelope.message),
                None if streaming_terminal
                    .is_some_and(|id| message_targets(&envelope.message, id)) => {}
                None => {
                    return Err(ControlError::RequestIdMismatch {
                        expected: request_id,
                        actual: None,
                    });
                }
            }
        }
    }

    fn read_envelope(&mut self) -> Result<ServerEnvelope, ControlError> {
        read_frame(&mut self.stream)?.ok_or(ControlError::ConnectionClosed)
    }

    fn validate_pinned_envelope(&self, envelope: &ServerEnvelope) -> Result<(), ControlError> {
        if envelope.version != PROTOCOL_VERSION {
            return Err(ControlError::UnsupportedVersion {
                actual: envelope.version,
            });
        }
        if !envelope.proof.is_complete() {
            return Err(ControlError::IncompleteProof);
        }
        if envelope.proof != self.proof {
            return Err(ControlError::ProofMismatch);
        }
        Ok(())
    }

    fn limit_read_timeout(&self, deadline: Instant) -> Result<(), ControlError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        self.stream
            .set_read_timeout(Some(
                remaining.max(Duration::from_millis(1)).min(IO_TIMEOUT),
            ))
            .map_err(ControlError::Configure)
    }

    fn restore_read_timeout(&self) -> Result<(), ControlError> {
        self.stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .map_err(ControlError::Configure)
    }
}

struct HandshakeClient {
    stream: UnixStream,
    next_request_id: u64,
}

impl HandshakeClient {
    fn handshake(&mut self) -> Result<HostSessionProof, ControlError> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        write_frame(
            &mut self.stream,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id,
                proof: None,
                request: ClientRequest::Connect {
                    expected_host: None,
                    resume: Vec::new(),
                },
            },
        )?;
        let envelope: ServerEnvelope =
            read_frame(&mut self.stream)?.ok_or(ControlError::ConnectionClosed)?;
        if envelope.version != PROTOCOL_VERSION {
            return Err(ControlError::UnsupportedVersion {
                actual: envelope.version,
            });
        }
        if envelope.request_id != Some(request_id) {
            return Err(ControlError::RequestIdMismatch {
                expected: request_id,
                actual: envelope.request_id,
            });
        }
        if !envelope.proof.is_complete() {
            return Err(ControlError::IncompleteProof);
        }
        match envelope.message {
            ServerMessage::Response {
                response: ServerResponse::Connected { proof, .. },
            } if !proof.is_complete() => Err(ControlError::IncompleteProof),
            ServerMessage::Response {
                response: ServerResponse::Connected { proof, .. },
            } if proof != envelope.proof => Err(ControlError::ProofMismatch),
            ServerMessage::Response {
                response: ServerResponse::Connected { proof, .. },
            } => Ok(proof),
            ServerMessage::Error(error) => Err(ControlError::Remote {
                code: error.code,
                message: error.message,
                retryable: error.retryable,
            }),
            message => Err(ControlError::UnexpectedMessage {
                expected: "the connect response",
                actual: message_name(&message),
            }),
        }
    }
}
fn is_timeout_error(error: &ControlError) -> bool {
    matches!(
        error,
        ControlError::Frame(FrameError::Io(error))
            if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
    )
}

fn response_from_message(message: ServerMessage) -> Result<ServerResponse, ControlError> {
    match message {
        ServerMessage::Response { response } => Ok(response),
        ServerMessage::Error(error) => Err(ControlError::Remote {
            code: error.code,
            message: error.message,
            retryable: error.retryable,
        }),
        message => Err(ControlError::UnexpectedMessage {
            expected: "a correlated response",
            actual: message_name(&message),
        }),
    }
}

fn message_targets(message: &ServerMessage, expected: TerminalId) -> bool {
    match message {
        ServerMessage::Redraw { terminal_id, .. }
        | ServerMessage::Snapshot { terminal_id, .. }
        | ServerMessage::Event { terminal_id, .. } => *terminal_id == expected,
        ServerMessage::Response { .. }
        | ServerMessage::Synchronized { .. }
        | ServerMessage::HostShuttingDown
        | ServerMessage::Error(_) => false,
    }
}

fn message_name(message: &ServerMessage) -> &'static str {
    match message {
        ServerMessage::Response { .. } => "response",
        ServerMessage::Redraw { .. } => "redraw",
        ServerMessage::Snapshot { .. } => "snapshot",
        ServerMessage::Event { .. } => "event",
        ServerMessage::Synchronized { .. } => "synchronized",
        ServerMessage::HostShuttingDown => "host_shutting_down",
        ServerMessage::Error(_) => "error",
    }
}

fn response_name(response: &ServerResponse) -> &'static str {
    match response {
        ServerResponse::Connected { .. } => "connected",
        ServerResponse::Listed { .. } => "listed",
        ServerResponse::Spawned { .. } => "spawned",
        ServerResponse::Attached { .. } => "attached",
        ServerResponse::Detached => "detached",
        ServerResponse::Accepted => "accepted",
        ServerResponse::Snapshot { .. } => "snapshot",
        ServerResponse::ShutdownAccepted => "shutdown_accepted",
    }
}

fn unexpected_response(expected: &'static str, response: &ServerResponse) -> ControlError {
    ControlError::UnexpectedResponse {
        expected,
        actual: response_name(response),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RenderedSnapshot {
    pub terminal_id: TerminalId,
    pub generation: u64,
    pub columns: u16,
    pub rows: u16,
    pub text: String,
    pub lines: Vec<String>,
}

impl RenderedSnapshot {
    /// The CLI always asks for a full repaint (`after_generation: 0`), so the
    /// resolved snapshot never depends on prior state; `apply(None)` therefore
    /// cannot fail for a full payload, and a delta arriving here is a protocol
    /// violation the caller must report rather than silently render blank.
    fn from_snapshot(
        terminal_id: TerminalId,
        snapshot: TerminalSnapshot,
    ) -> Result<Self, ControlError> {
        let generation = snapshot.generation;
        let columns = snapshot.columns;
        let rows = snapshot.rows;
        let resolved = snapshot
            .apply(None)
            .map_err(|_| ControlError::IncrementalSnapshot)?;
        let lines = resolved
            .lines
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        let text = lines.join("\n");
        Ok(Self {
            terminal_id,
            generation,
            columns,
            rows,
            text,
            lines,
        })
    }
}

struct CommandOutput {
    command: &'static str,
    data: Value,
    human: String,
}

#[derive(Debug)]
struct AppError {
    kind: &'static str,
    message: String,
    details: Option<Value>,
}

impl AppError {
    fn new(kind: &'static str, error: impl std::fmt::Display) -> Self {
        Self {
            kind,
            message: error.to_string(),
            details: None,
        }
    }
}

pub fn run_from<I, T>(arguments: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let arguments = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
    let wants_json = arguments
        .iter()
        .any(|argument| argument.as_os_str() == OsStr::new("--json"));
    let cli = match Cli::try_parse_from(arguments) {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ClapErrorKind::DisplayHelp | ClapErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            emit_error(wants_json, &AppError::new("usage", error));
            return ExitCode::from(2);
        }
    };

    match execute(cli.command) {
        Ok(output) => {
            emit_success(cli.json, output);
            ExitCode::SUCCESS
        }
        Err(error) => {
            emit_error(cli.json, &error);
            ExitCode::FAILURE
        }
    }
}

fn execute(command: TopCommand) -> Result<CommandOutput, AppError> {
    match command {
        TopCommand::Ui => launch_ui(),
        TopCommand::Daemon { command } => execute_daemon(command),
        TopCommand::Terminal { command } => execute_terminal(command),
    }
}

fn launch_ui() -> Result<CommandOutput, AppError> {
    if instance::activate_running_ui().map_err(|error| AppError::new("launch", error))? {
        return Ok(already_running_ui_output());
    }
    let executable =
        sibling_executable("cagents-ui").map_err(|error| AppError::new("launch", error))?;
    let mut command = Command::new(&executable);
    command
        .env(UI_LAUNCH_HANDSHAKE_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| AppError::new("launch", error))?;
    let launch_result = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("cagents-ui handshake pipe was not created"))
        .and_then(|mut stdout| read_ui_launch_handshake(&mut stdout));
    if let Err(error) = launch_result {
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
        if handshake_reports_already_running(&error)
            && instance::activate_running_ui().map_err(|error| AppError::new("launch", error))?
        {
            return Ok(already_running_ui_output());
        }
        return Err(AppError::new("launch", error));
    }
    Ok(CommandOutput {
        command: "ui",
        data: json!({ "launched": true, "executable": executable }),
        human: "cagents-ui started".to_owned(),
    })
}

fn already_running_ui_output() -> CommandOutput {
    CommandOutput {
        command: "ui",
        data: json!({ "launched": true, "already_running": true }),
        human: "cagents is already running".to_owned(),
    }
}

fn handshake_reports_already_running(error: &io::Error) -> bool {
    error.to_string() == instance::ApplicationLeaseError::AlreadyRunning.to_string()
}

fn read_ui_launch_handshake(stdout: &mut ChildStdout) -> io::Result<()> {
    let descriptor = stdout.as_raw_fd();
    // SAFETY: fcntl receives a live descriptor and integer flags only.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(io::Error::last_os_error());
    }

    let deadline = Instant::now() + UI_LAUNCH_HANDSHAKE_TIMEOUT;
    let mut response = Vec::new();
    let mut buffer = [0_u8; 256];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "cagents-ui exited before acknowledging startup",
                ));
            }
            Ok(read) => {
                response.extend_from_slice(&buffer[..read]);
                if response.len() > MAX_UI_LAUNCH_HANDSHAKE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "cagents-ui startup response was too large",
                    ));
                }
                if let Some(newline) = response.iter().position(|byte| *byte == b'\n') {
                    response.truncate(newline);
                    return parse_ui_launch_handshake(&response);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "cagents-ui did not acknowledge startup within 5 seconds",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn parse_ui_launch_handshake(response: &[u8]) -> io::Result<()> {
    let response = std::str::from_utf8(response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if response == UI_LAUNCH_READY {
        return Ok(());
    }
    if let Some(message) = response.strip_prefix(UI_LAUNCH_ERROR_PREFIX) {
        return Err(io::Error::other(message.to_owned()));
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "cagents-ui returned an invalid startup response",
    ))
}

fn execute_daemon(command: DaemonCommand) -> Result<CommandOutput, AppError> {
    match command {
        DaemonCommand::Start => {
            let socket = terminal::ensure_daemon_ready()
                .map_err(|error| AppError::new("daemon_start", error))?;
            Ok(CommandOutput {
                command: "daemon.start",
                data: json!({ "instance": instance::NAME, "running": true, "socket": socket }),
                human: format!("{} daemon running ({})", instance::NAME, socket.display()),
            })
        }
        DaemonCommand::Status => {
            let socket =
                daemon_socket_path().map_err(|error| AppError::new("socket_path", error))?;
            let running = match ControlClient::connect(&socket) {
                Ok(_) => true,
                Err(ControlError::Connect(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    false
                }
                Err(error) => return Err(AppError::new("protocol", error)),
            };
            Ok(CommandOutput {
                command: "daemon.status",
                data: json!({
                    "instance": instance::NAME,
                    "running": running,
                    "socket": socket
                }),
                human: if running {
                    format!("{} daemon running ({})", instance::NAME, socket.display())
                } else {
                    format!("{} daemon stopped", instance::NAME)
                },
            })
        }
        DaemonCommand::Stop => {
            let socket =
                daemon_socket_path().map_err(|error| AppError::new("socket_path", error))?;
            let mut client = ControlClient::connect(&socket)
                .map_err(|error| AppError::new("daemon_connection", error))?;
            client
                .shutdown_daemon()
                .map_err(|error| AppError::new("protocol", error))?;
            Ok(CommandOutput {
                command: "daemon.stop",
                data: json!({ "instance": instance::NAME, "stopped": true }),
                human: format!("{} daemon stopping", instance::NAME),
            })
        }
        DaemonCommand::StopForRebuild => {
            let stopped = crate::host::terminate_daemon_from_lock()
                .map_err(|error| AppError::new("daemon_process", error))?;
            Ok(CommandOutput {
                command: "daemon.stop_for_rebuild",
                data: json!({ "instance": instance::NAME, "stopped": stopped }),
                human: if stopped {
                    format!("{} daemon stopping", instance::NAME)
                } else {
                    format!("{} daemon stopped", instance::NAME)
                },
            })
        }
    }
}

fn execute_terminal(command: TerminalCommand) -> Result<CommandOutput, AppError> {
    let socket =
        terminal::ensure_daemon_ready().map_err(|error| AppError::new("daemon_start", error))?;
    let mut client = ControlClient::connect(&socket)
        .map_err(|error| AppError::new("daemon_connection", error))?;
    match command {
        TerminalCommand::List => {
            let terminals = client.list().map_err(protocol_error)?;
            let human = terminals
                .iter()
                .map(descriptor_line)
                .collect::<Vec<_>>()
                .join("\n");
            Ok(CommandOutput {
                command: "terminal.list",
                data: json!({ "terminals": terminals }),
                human,
            })
        }
        TerminalCommand::Spawn { cwd, argv } => {
            let cwd = absolute_working_directory(cwd)
                .map_err(|error| AppError::new("working_directory", error))?;
            let recipe = if argv.is_empty() {
                LaunchRecipe::shell(cwd).map_err(|error| AppError::new("login_shell", error))?
            } else {
                LaunchRecipe {
                    program: argv[0].clone(),
                    arguments: argv[1..].to_vec(),
                    working_directory: cwd,
                    environment: BTreeMap::new(),
                }
            };
            let session = TerminalSession {
                id: TerminalId::new(),
                launch: recipe.clone(),
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe,
                    last_exit: None,
                },
            };
            let terminal = client
                .spawn(session, TerminalSpawnOptions::default())
                .map_err(protocol_error)?;
            let human = descriptor_line(&terminal);
            Ok(CommandOutput {
                command: "terminal.spawn",
                data: json!({ "terminal": terminal }),
                human,
            })
        }
        TerminalCommand::Write { terminal_id, text } => {
            let bytes = text.into_bytes();
            client.write(terminal_id, bytes).map_err(protocol_error)?;
            Ok(CommandOutput {
                command: "terminal.write",
                data: json!({ "terminal_id": terminal_id, "written": true }),
                human: format!("wrote to {terminal_id}"),
            })
        }
        TerminalCommand::Close { terminal_id } => {
            client.close_terminal(terminal_id).map_err(protocol_error)?;
            Ok(CommandOutput {
                command: "terminal.close",
                data: json!({ "terminal_id": terminal_id, "closed": true }),
                human: format!("closed {terminal_id}"),
            })
        }
        TerminalCommand::Snapshot { terminal_id } => {
            let snapshot = client.snapshot(terminal_id).map_err(protocol_error)?;
            let human = snapshot.text.clone();
            Ok(CommandOutput {
                command: "terminal.snapshot",
                data: json!({ "snapshot": snapshot }),
                human,
            })
        }
        TerminalCommand::Wait {
            terminal_id,
            text,
            timeout,
        } => {
            let snapshot = client
                .wait_for_text(terminal_id, &text, Duration::from_secs_f64(timeout))
                .map_err(protocol_error)?;
            Ok(CommandOutput {
                command: "terminal.wait",
                data: json!({ "matched": true, "text": text, "snapshot": snapshot }),
                human: format!("text found in {terminal_id}"),
            })
        }
    }
}

fn protocol_error(error: ControlError) -> AppError {
    let kind = match &error {
        ControlError::WaitTimeout { .. } | ControlError::SnapshotTimeout => "timeout",
        ControlError::Connect(_)
        | ControlError::Configure(_)
        | ControlError::Frame(_)
        | ControlError::ConnectionClosed => "connection",
        _ => "protocol",
    };
    let details = match &error {
        ControlError::Remote {
            code, retryable, ..
        } => Some(json!({ "code": code, "retryable": retryable })),
        ControlError::WaitTimeout { terminal_id, text } => {
            Some(json!({ "terminal_id": terminal_id, "text": text }))
        }
        _ => None,
    };
    AppError {
        kind,
        message: error.to_string(),
        details,
    }
}

fn descriptor_line(terminal: &TerminalDescriptor) -> String {
    format!(
        "{}\t{}\t{}\t{}",
        terminal.session.id,
        if terminal.attached {
            "attached"
        } else {
            "detached"
        },
        terminal.session.launch.program,
        terminal.session.launch.working_directory.display(),
    )
}

fn absolute_working_directory(cwd: Option<PathBuf>) -> io::Result<PathBuf> {
    let current = std::env::current_dir()?;
    Ok(match cwd {
        Some(path) if path.is_absolute() => path,
        Some(path) => current.join(path),
        None => current,
    })
}

fn sibling_executable(name: &str) -> io::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let directory = current.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the current executable has no parent directory",
        )
    })?;
    Ok(directory.join(name))
}

fn emit_success(json_output: bool, output: CommandOutput) {
    if json_output {
        println!(
            "{}",
            json!({ "ok": true, "command": output.command, "data": output.data })
        );
    } else if !output.human.is_empty() {
        println!("{}", output.human);
    }
}

fn emit_error(json_output: bool, error: &AppError) {
    if json_output {
        eprintln!(
            "{}",
            json!({
                "ok": false,
                "error": {
                    "kind": error.kind,
                    "message": error.message,
                    "details": error.details,
                }
            })
        );
    } else {
        eprintln!("cagents: {}", error.message);
    }
}

#[cfg(test)]
mod tests {
    use std::{os::unix::net::UnixStream, thread};

    use clap::CommandFactory;

    use crate::{
        model::{BootId, HostIncarnation, HostSessionId},
        terminal::ipc::ProtocolError,
    };

    use super::*;

    fn proof(session: &str, boot: &str, incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new(session),
            boot_id: BootId::new(boot),
            host_incarnation: HostIncarnation::new(incarnation),
        }
    }

    fn connected_envelope(
        request_id: u64,
        envelope_proof: HostSessionProof,
        response_proof: HostSessionProof,
    ) -> ServerEnvelope {
        ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request_id),
            proof: envelope_proof,
            message: ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: response_proof,
                    resumed: Vec::new(),
                },
            },
        }
    }

    #[test]
    fn parses_the_complete_terminal_surface_and_trailing_argv() {
        let cli = Cli::try_parse_from([
            "cagents",
            "--json",
            "terminal",
            "spawn",
            "--cwd",
            "/tmp",
            "--",
            "bash",
            "-lc",
            "printf ok",
        ])
        .unwrap();
        assert!(cli.json);
        let TopCommand::Terminal {
            command: TerminalCommand::Spawn { cwd, argv },
        } = cli.command
        else {
            panic!("expected terminal spawn");
        };
        assert_eq!(cwd, Some(PathBuf::from("/tmp")));
        assert_eq!(argv, ["bash", "-lc", "printf ok"]);

        let wait = Cli::try_parse_from([
            "cagents",
            "terminal",
            "wait",
            "00000000-0000-0000-0000-000000000001",
            "--text",
            "ready",
            "--timeout",
            "0.5",
        ])
        .unwrap();
        assert!(matches!(
            wait.command,
            TopCommand::Terminal {
                command: TerminalCommand::Wait { timeout, .. }
            } if timeout == 0.5
        ));
        Cli::command().debug_assert();
    }

    #[test]
    fn handshake_pins_complete_matching_proof_and_correlates_requests() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let expected = proof("session", "boot", "incarnation");
        let server_proof = expected.clone();
        let server = thread::spawn(move || {
            let connect: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(connect.version, PROTOCOL_VERSION);
            assert_eq!(connect.request_id, 1);
            assert_eq!(connect.proof, None);
            assert!(matches!(
                connect.request,
                ClientRequest::Connect {
                    expected_host: None,
                    ref resume
                } if resume.is_empty()
            ));
            write_frame(
                &mut server_stream,
                &connected_envelope(1, server_proof.clone(), server_proof.clone()),
            )
            .unwrap();

            let list: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(list.request_id, 2);
            assert_eq!(list.proof.as_ref(), Some(&server_proof));
            write_frame(
                &mut server_stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(2),
                    proof: server_proof,
                    message: ServerMessage::Response {
                        response: ServerResponse::Listed {
                            terminals: Vec::new(),
                        },
                    },
                },
            )
            .unwrap();
        });

        let mut client = ControlClient::from_stream(client_stream).unwrap();
        assert_eq!(client.proof(), &expected);
        assert!(client.list().unwrap().is_empty());
        server.join().unwrap();
    }

    #[test]
    fn handshake_rejects_versions_incomplete_proofs_mismatches_and_streaming() {
        let cases = [
            connected_envelope(
                1,
                proof("session", "boot", "incarnation"),
                proof("session", "boot", "incarnation"),
            ),
            connected_envelope(
                1,
                proof("", "boot", "incarnation"),
                proof("", "boot", "incarnation"),
            ),
            connected_envelope(
                1,
                proof("session", "boot", "incarnation"),
                proof("other", "boot", "incarnation"),
            ),
            ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(1),
                proof: proof("session", "boot", "incarnation"),
                message: ServerMessage::HostShuttingDown,
            },
        ];

        for (index, mut response) in cases.into_iter().enumerate() {
            if index == 0 {
                response.version = PROTOCOL_VERSION + 1;
            }
            let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
            let server = thread::spawn(move || {
                let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                write_frame(&mut server_stream, &response).unwrap();
            });
            let error = ControlClient::from_stream(client_stream).err().unwrap();
            assert!(matches!(
                (index, error),
                (0, ControlError::UnsupportedVersion { .. })
                    | (1, ControlError::IncompleteProof)
                    | (2, ControlError::ProofMismatch)
                    | (3, ControlError::UnexpectedMessage { .. })
            ));
            server.join().unwrap();
        }
    }

    #[test]
    fn request_ids_and_remote_errors_fail_closed() {
        for remote_error in [false, true] {
            let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
            let server_proof = proof("session", "boot", "incarnation");
            let peer_proof = server_proof.clone();
            let server = thread::spawn(move || {
                let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                write_frame(
                    &mut server_stream,
                    &connected_envelope(1, peer_proof.clone(), peer_proof.clone()),
                )
                .unwrap();
                let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                let message = if remote_error {
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::InvalidRequest,
                        "no",
                        false,
                    ))
                } else {
                    ServerMessage::Response {
                        response: ServerResponse::Listed {
                            terminals: Vec::new(),
                        },
                    }
                };
                write_frame(
                    &mut server_stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(if remote_error { 2 } else { 99 }),
                        proof: peer_proof,
                        message,
                    },
                )
                .unwrap();
            });
            let mut client = ControlClient::from_stream(client_stream).unwrap();
            let error = client.list().unwrap_err();
            assert!(matches!(
                (remote_error, error),
                (false, ControlError::RequestIdMismatch { .. })
                    | (true, ControlError::Remote { .. })
            ));
            server.join().unwrap();
        }
    }

    #[test]
    fn post_handshake_proof_changes_and_unsolicited_streams_fail_closed() {
        for malformed in 0..2 {
            let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
            let pinned = proof("session", "boot", "incarnation");
            let peer_proof = pinned.clone();
            let server = thread::spawn(move || {
                let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                write_frame(
                    &mut server_stream,
                    &connected_envelope(1, peer_proof.clone(), peer_proof.clone()),
                )
                .unwrap();
                let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                let response = if malformed == 0 {
                    ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(2),
                        proof: proof("different", "boot", "incarnation"),
                        message: ServerMessage::Response {
                            response: ServerResponse::Listed {
                                terminals: Vec::new(),
                            },
                        },
                    }
                } else {
                    ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: None,
                        proof: peer_proof,
                        message: ServerMessage::Redraw {
                            terminal_id: TerminalId::new(),
                            generation: 1,
                        },
                    }
                };
                write_frame(&mut server_stream, &response).unwrap();
            });
            let mut client = ControlClient::from_stream(client_stream).unwrap();
            let error = client.list().unwrap_err();
            assert!(matches!(
                (malformed, error),
                (0, ControlError::ProofMismatch)
                    | (1, ControlError::RequestIdMismatch { actual: None, .. })
            ));
            server.join().unwrap();
        }
    }
    #[test]
    fn ui_launch_handshake_surfaces_child_startup_errors() {
        parse_ui_launch_handshake(UI_LAUNCH_READY.as_bytes()).unwrap();

        let error = parse_ui_launch_handshake(
            format!("{UI_LAUNCH_ERROR_PREFIX}upgrade the peer package").as_bytes(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "upgrade the peer package");
        assert_eq!(
            parse_ui_launch_handshake(b"unknown").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let already_running = parse_ui_launch_handshake(
            format!(
                "{UI_LAUNCH_ERROR_PREFIX}{}",
                instance::ApplicationLeaseError::AlreadyRunning
            )
            .as_bytes(),
        )
        .unwrap_err();
        assert!(handshake_reports_already_running(&already_running));
    }
}
