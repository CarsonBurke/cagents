use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    io::{self, IsTerminal, Read, Write},
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
            SHUTDOWN_VERSION, ServerEnvelope, ServerMessage, ServerResponse, ShutdownCapability,
            ShutdownOperation, ShutdownOutcome, ShutdownPreflight, TerminalDescriptor,
            TerminalSnapshot, TerminalVisibility, daemon_socket_path, read_frame, write_frame,
        },
    },
};

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const SNAPSHOT_RETRY_INTERVAL: Duration = Duration::from_millis(25);
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const UI_LAUNCH_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
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
    Ui {
        /// Open another durable window slot instead of activating a running UI.
        #[arg(long, conflicts_with = "window")]
        new_window: bool,
        /// Reopen or activate a named window slot.
        #[arg(long, value_parser = parse_window_slot)]
        window: Option<String>,
    },
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
    /// Run the detached updater started by an explicit application Update action.
    #[command(hide = true)]
    UpdateWorker { job_id: String },
}

fn parse_window_slot(value: &str) -> Result<String, String> {
    instance::validate_window_slot(value).map_err(|error| error.to_string())?;
    Ok(value.to_owned())
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Start the daemon if it is not already running.
    Start,
    /// Report whether the daemon is running.
    Status,
    /// Request daemon termination (use --confirm for a counted interactive confirmation).
    Stop {
        #[arg(long, conflicts_with = "confirm")]
        require_empty: bool,
        #[arg(long, conflicts_with = "confirm")]
        expected_generation: Option<u64>,
        #[arg(long)]
        confirm: bool,
    },
    /// Checkpoint resumable sessions and stop for a full application upgrade.
    PrepareUpgrade {
        /// Authorize interruption and resumption of all supported hosted sessions.
        #[arg(long)]
        yes: bool,
    },
    /// Archive an interrupted upgrade checkpoint without resuming its processes.
    AbandonUpgrade {
        /// Require explicit manual recovery of conversations in the archived checkpoint.
        #[arg(long)]
        yes: bool,
    },
    /// Print this executable's wire protocol version without contacting a daemon.
    ProtocolVersion,
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
    #[error("shutdown request rejected: {0:?}")]
    ShutdownRejected(ShutdownOutcome),
}

/// A bounded, synchronous client for the current cagents daemon protocol.
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

    pub fn list_chats(&mut self) -> Result<Vec<crate::chat::Session>, ControlError> {
        match self.request_strict(ClientRequest::ChatList)? {
            ServerResponse::ChatListed { sessions } => Ok(sessions),
            response => Err(unexpected_response("a native chat list", &response)),
        }
    }

    pub fn resolve_upgrade_chat(
        &mut self,
        source: crate::chat::Session,
    ) -> Result<crate::chat::Session, ControlError> {
        match self.request_strict(ClientRequest::ResolveUpgradeChat { source })? {
            ServerResponse::UpgradeChatResolved { session } => Ok(session),
            response => Err(unexpected_response("a restored native chat", &response)),
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

    pub fn close_terminal_checked(
        &mut self,
        terminal_id: TerminalId,
        allow_active: bool,
    ) -> Result<terminal::ipc::TerminalCloseResult, ControlError> {
        self.attach_and_synchronize(terminal_id)?;
        let action = self
            .request_attached(
                ClientRequest::CloseTerminalChecked {
                    terminal_id,
                    allow_active,
                },
                terminal_id,
            )
            .and_then(|response| match response {
                ServerResponse::TerminalCloseChecked {
                    terminal_id: actual,
                    outcome,
                } if actual == terminal_id => Ok(outcome),
                response => Err(unexpected_response("a checked terminal close", &response)),
            });
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

    pub fn shutdown_preflight(&mut self) -> Result<ShutdownPreflight, ControlError> {
        match self.request_strict(ClientRequest::ShutdownV1 {
            version: SHUTDOWN_VERSION,
            operation: ShutdownOperation::Preflight,
        })? {
            ServerResponse::ShutdownPreflight(preflight) => Ok(preflight),
            ServerResponse::ShutdownOutcome(outcome) => {
                Err(ControlError::ShutdownRejected(outcome))
            }
            response => Err(unexpected_response("shutdown preflight", &response)),
        }
    }

    pub fn prepare_upgrade(
        &mut self,
        capability: ShutdownCapability,
    ) -> Result<(u64, u64), ControlError> {
        match self.request_strict(ClientRequest::PrepareUpgrade { capability })? {
            ServerResponse::UpgradePrepared { terminals, chats } => Ok((terminals, chats)),
            response => Err(unexpected_response("an upgrade checkpoint", &response)),
        }
    }

    pub fn shutdown_with_policy(
        &mut self,
        require_empty: bool,
        expected_generation: Option<u64>,
    ) -> Result<ShutdownOutcome, ControlError> {
        self.shutdown_operation(ShutdownOperation::Stop {
            require_empty,
            expected_generation,
        })
    }

    pub fn confirm_shutdown(
        &mut self,
        capability: ShutdownCapability,
    ) -> Result<ShutdownOutcome, ControlError> {
        self.shutdown_operation(ShutdownOperation::Confirm { capability })
    }

    fn shutdown_operation(
        &mut self,
        operation: ShutdownOperation,
    ) -> Result<ShutdownOutcome, ControlError> {
        match self.request_strict(ClientRequest::ShutdownV1 {
            version: SHUTDOWN_VERSION,
            operation,
        })? {
            ServerResponse::ShutdownOutcome(outcome) => Ok(outcome),
            response => Err(unexpected_response("shutdown outcome", &response)),
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
                | ServerMessage::TerminalObserved {
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
        | ServerMessage::TerminalObserved { terminal_id, .. }
        | ServerMessage::Event { terminal_id, .. } => *terminal_id == expected,
        ServerMessage::Response { .. }
        | ServerMessage::Synchronized { .. }
        | ServerMessage::HostShuttingDown
        | ServerMessage::ChatChanged { .. }
        | ServerMessage::ChatObserved { .. }
        | ServerMessage::ChatActivity { .. }
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
        ServerMessage::ChatChanged { .. } => "chat_changed",
        ServerMessage::ChatObserved { .. } => "chat_observed",
        ServerMessage::ChatActivity { .. } => "chat_activity",
        ServerMessage::TerminalObserved { .. } => "terminal_observed",
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
        ServerResponse::ShutdownPreflight(_) => "shutdown_preflight",
        ServerResponse::ShutdownOutcome(_) => "shutdown_outcome",
        ServerResponse::ChatAttached { .. } => "chat_attached",
        ServerResponse::ChatSnapshot { .. } => "chat_snapshot",
        ServerResponse::ChatListed { .. } => "chat_listed",
        ServerResponse::ChatClosed { .. } => "chat_closed",
        ServerResponse::ChatPreviewed { .. } => "chat_previewed",
        ServerResponse::TerminalCloseChecked { .. } => "terminal_close_checked",
        ServerResponse::AgentActivity { .. } => "agent_activity",
        ServerResponse::UpgradePrepared { .. } => "upgrade_prepared",
        ServerResponse::UpgradeChatResolved { .. } => "upgrade_chat_resolved",
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
        TopCommand::Ui { new_window, window } => launch_ui(new_window, window.as_deref()),
        TopCommand::Daemon { command } => execute_daemon(command),
        TopCommand::Terminal { command } => execute_terminal(command),
        TopCommand::UpdateWorker { job_id } => {
            crate::update::run_worker(&job_id).map_err(|error| AppError::new("update", error))?;
            Ok(CommandOutput {
                command: "update.worker",
                data: json!({ "job_id": job_id, "complete": true }),
                human: "Application update complete".into(),
            })
        }
    }
}

fn launch_ui(new_window: bool, slot: Option<&str>) -> Result<CommandOutput, AppError> {
    let activate = || match slot {
        Some(slot) => instance::activate_window(slot),
        None => instance::activate_running_ui(),
    };
    if !new_window && activate().map_err(|error| AppError::new("launch", error))? {
        return Ok(already_running_ui_output());
    }
    let executable =
        sibling_executable("cagents-ui").map_err(|error| AppError::new("launch", error))?;
    let mut command = Command::new(&executable);
    if new_window {
        command.arg("--new-window");
    }
    if let Some(slot) = slot {
        command.args(["--window", slot]);
    }
    if let Err(error) = spawn_ui_process(&mut command) {
        if !new_window
            && handshake_reports_already_running(&error)
            && activate().map_err(|error| AppError::new("launch", error))?
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

pub(crate) fn spawn_ui_process(command: &mut Command) -> io::Result<()> {
    // Capture the budget before spawning: the child can consume the checkpoint
    // while restoring, but the launcher must retain its full startup allowance.
    let timeout = crate::upgrade::handoff::startup_timeout().map_err(io::Error::other)?
        + UI_LAUNCH_HANDSHAKE_TIMEOUT;
    command
        .env(UI_LAUNCH_HANDSHAKE_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    // SAFETY: the hook performs only the close_range syscall before exec.
    unsafe {
        command.pre_exec(crate::daemon::seal_inherited_descriptors);
    }
    let mut child = command.spawn()?;
    let result = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("cagents-ui handshake pipe was not created"))
        .and_then(|mut stdout| read_ui_launch_handshake(&mut stdout, timeout));
    if let Err(error) = result {
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
        return Err(error);
    }
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
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

fn read_ui_launch_handshake(stdout: &mut ChildStdout, timeout: Duration) -> io::Result<()> {
    let descriptor = stdout.as_raw_fd();
    // SAFETY: fcntl receives a live descriptor and integer flags only.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(io::Error::last_os_error());
    }

    let deadline = Instant::now() + timeout;
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
                        format!("cagents-ui did not acknowledge initialization within {timeout:?}"),
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
            let paths = crate::daemon::RuntimePaths::discover()
                .map_err(|error| AppError::new("socket_path", error))?;
            let socket = &paths.socket;
            let running = match ControlClient::connect(socket) {
                Ok(_) => true,
                Err(ControlError::Connect(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    crate::daemon::HostLease::is_held(&paths)
                        .map_err(|error| AppError::new("daemon_status", error))?
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
        DaemonCommand::ProtocolVersion => Ok(CommandOutput {
            command: "daemon.protocol_version",
            data: json!({ "protocol_version": PROTOCOL_VERSION, "shutdown_version": SHUTDOWN_VERSION }),
            human: PROTOCOL_VERSION.to_string(),
        }),
        DaemonCommand::Stop {
            require_empty,
            expected_generation,
            confirm,
        } => {
            let socket =
                daemon_socket_path().map_err(|error| AppError::new("socket_path", error))?;
            let mut client = ControlClient::connect(&socket).map_err(protocol_error)?;
            let outcome = if confirm {
                if !io::stdin().is_terminal() {
                    return Err(AppError::new(
                        "confirmation_required",
                        "interactive confirmation requires a terminal; use the cagents UI or --require-empty",
                    ));
                }
                let preflight = client.shutdown_preflight().map_err(protocol_error)?;
                eprint!(
                    "Stop daemon and terminate {} hosted terminal/chat sessions (including pending spawns and unconfirmed provider work)? Type yes to confirm: ",
                    preflight.capability.live_terminals
                );
                io::stderr()
                    .flush()
                    .map_err(|error| AppError::new("confirmation_input", error))?;
                let mut answer = String::new();
                io::stdin()
                    .read_line(&mut answer)
                    .map_err(|error| AppError::new("confirmation_input", error))?;
                if answer.trim() != "yes" {
                    return Ok(CommandOutput {
                        command: "daemon.stop",
                        data: json!({ "outcome": "cancelled", "termination_requested": false }),
                        human: "Shutdown cancelled; no termination requested".into(),
                    });
                }
                client
                    .confirm_shutdown(preflight.capability)
                    .map_err(protocol_error)?
            } else {
                client
                    .shutdown_with_policy(require_empty, expected_generation)
                    .map_err(protocol_error)?
            };
            shutdown_output(outcome)
        }
        DaemonCommand::PrepareUpgrade { yes } => {
            if !yes {
                return Err(AppError::new(
                    "confirmation_required",
                    "full upgrade interrupts hosted processes; pass --yes to checkpoint and resume supported sessions",
                ));
            }
            let socket =
                daemon_socket_path().map_err(|error| AppError::new("socket_path", error))?;
            let mut client = ControlClient::connect(&socket).map_err(protocol_error)?;
            let preflight = client.shutdown_preflight().map_err(protocol_error)?;
            let (terminals, chats) = client
                .prepare_upgrade(preflight.capability)
                .map_err(protocol_error)?;
            Ok(CommandOutput {
                command: "daemon.prepare_upgrade",
                data: json!({ "terminals": terminals, "chats": chats }),
                human: format!(
                    "Checkpointed {terminals} terminals and {chats} chats; daemon shutdown requested for upgrade"
                ),
            })
        }
        DaemonCommand::AbandonUpgrade { yes } => {
            if !yes {
                return Err(AppError::new(
                    "confirmation_required",
                    "pass --yes to archive the interrupted checkpoint; its conversations will require explicit manual recovery",
                ));
            }
            let archive = crate::upgrade::abandon()
                .map_err(|error| AppError::new("upgrade_recovery", error))?;
            Ok(CommandOutput {
                command: "daemon.abandon_upgrade",
                data: json!({ "archive": archive, "resumed": false }),
                human: format!(
                    "Archived checkpoint at {}; no processes resumed. Recover exact conversations manually after checking their existing writers",
                    archive.display()
                ),
            })
        }
        DaemonCommand::StopForRebuild => {
            let stopped = crate::daemon::terminate_daemon_from_lock()
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

fn shutdown_output(outcome: ShutdownOutcome) -> Result<CommandOutput, AppError> {
    let data = shutdown_outcome_json(&outcome);
    let human = shutdown_outcome_message(&outcome);
    if matches!(outcome, ShutdownOutcome::Accepted { .. }) {
        Ok(CommandOutput {
            command: "daemon.stop",
            data,
            human,
        })
    } else {
        Err(AppError {
            kind: "shutdown_rejected",
            message: human,
            details: Some(data),
        })
    }
}

pub fn shutdown_outcome_json(outcome: &ShutdownOutcome) -> Value {
    match outcome {
        ShutdownOutcome::Accepted {
            live_terminals_before,
            termination_requested,
        } => json!({
            "outcome": "accepted", "live_terminals_before": live_terminals_before,
            "termination_requested": termination_requested
        }),
        ShutdownOutcome::EmptyRequired { current_count } => {
            json!({ "outcome": "empty_required", "current_count": current_count, "termination_requested": false })
        }
        ShutdownOutcome::InventoryChanged { current_count } => {
            json!({ "outcome": "inventory_changed", "current_count": current_count, "termination_requested": false })
        }
        other => json!({ "outcome": match other {
            ShutdownOutcome::Expired => "expired",
            ShutdownOutcome::DaemonRestarted => "daemon_restarted",
            ShutdownOutcome::InvalidCapability => "invalid_capability",
            ShutdownOutcome::AlreadyShuttingDown => "already_shutting_down",
            _ => unreachable!(),
        }, "termination_requested": false }),
    }
}

pub fn shutdown_outcome_message(outcome: &ShutdownOutcome) -> String {
    match outcome {
        ShutdownOutcome::Accepted {
            live_terminals_before,
            termination_requested,
        } => format!(
            "Shutdown accepted: live_terminals_before={live_terminals_before}, termination_requested={termination_requested}; termination has not been confirmed complete"
        ),
        ShutdownOutcome::EmptyRequired { current_count } => format!(
            "Shutdown rejected: empty_required, current_count={current_count}, termination_requested=false"
        ),
        ShutdownOutcome::InventoryChanged { current_count } => format!(
            "Shutdown rejected: inventory_changed, current_count={current_count}, termination_requested=false; refresh confirmation"
        ),
        ShutdownOutcome::Expired => {
            "Shutdown confirmation expired; refresh confirmation (termination_requested=false)"
                .into()
        }
        ShutdownOutcome::DaemonRestarted => {
            "Daemon restarted; refresh confirmation (termination_requested=false)".into()
        }
        ShutdownOutcome::InvalidCapability => {
            "Invalid shutdown confirmation; refresh confirmation (termination_requested=false)"
                .into()
        }
        ShutdownOutcome::AlreadyShuttingDown => {
            "Shutdown already requested; termination is not yet confirmed complete".into()
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
    #[test]
    fn require_empty_is_one_guarded_request_and_never_a_list_then_force() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let peer_proof = proof("session", "boot", "incarnation");
        let server = thread::spawn(move || {
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &connected_envelope(1, peer_proof.clone(), peer_proof.clone()),
            )
            .unwrap();
            let request: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(
                request.request,
                ClientRequest::ShutdownV1 {
                    version: SHUTDOWN_VERSION,
                    operation: ShutdownOperation::Stop {
                        require_empty: true,
                        expected_generation: Some(9)
                    },
                }
            );
            write_frame(
                &mut server_stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: peer_proof,
                    message: ServerMessage::Response {
                        response: ServerResponse::ShutdownOutcome(ShutdownOutcome::EmptyRequired {
                            current_count: 1,
                        }),
                    },
                },
            )
            .unwrap();
            assert!(
                read_frame::<ClientEnvelope>(&mut server_stream)
                    .unwrap()
                    .is_none()
            );
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        assert_eq!(
            client.shutdown_with_policy(true, Some(9)).unwrap(),
            ShutdownOutcome::EmptyRequired { current_count: 1 }
        );
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn shutdown_cli_policies_and_results_preserve_acceptance_not_completion() {
        let cli = Cli::try_parse_from([
            "cagents",
            "daemon",
            "stop",
            "--require-empty",
            "--expected-generation",
            "9",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            TopCommand::Daemon {
                command: DaemonCommand::Stop {
                    require_empty: true,
                    expected_generation: Some(9),
                    confirm: false
                }
            }
        ));
        assert!(
            Cli::try_parse_from(["cagents", "daemon", "stop", "--require-empty", "--confirm"])
                .is_err()
        );
        let output = shutdown_output(ShutdownOutcome::Accepted {
            live_terminals_before: 7,
            termination_requested: true,
        })
        .unwrap();
        assert_eq!(
            output.data,
            json!({ "outcome": "accepted", "live_terminals_before": 7, "termination_requested": true })
        );
        assert!(output.data.get("stopped").is_none());
        let error = shutdown_output(ShutdownOutcome::InventoryChanged { current_count: 1 })
            .err()
            .unwrap();
        assert_eq!(error.details.unwrap()["outcome"], "inventory_changed");
    }

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
