use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
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
    model::{
        ChannelId, HostSessionProof, LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession,
    },
    terminal::{
        self, TerminalSpawnOptions,
        ipc::{
            ClientEnvelope, ClientRequest, FrameError, PROTOCOL_VERSION, ProtocolErrorCode,
            SHUTDOWN_VERSION, ServerEnvelope, ServerMessage, ServerResponse, ShutdownCapability,
            ShutdownOperation, ShutdownOutcome, ShutdownPreflight, TerminalDescriptor,
            TerminalSnapshot, TerminalVisibility, daemon_socket_path, read_frame, write_frame,
        },
    },
    workspace_model::{
        ModelChannel, ModelEdit, ModelProject, WorkspaceModel, canonical_repository,
        repository_display_name,
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
    version = crate::VERSION,
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
    /// Manage the projects every window shares.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Manage the channels every window shares.
    Channel {
        #[command(subcommand)]
        command: ChannelCommand,
    },
    /// Run the detached updater started by an explicit application Update action.
    #[command(hide = true)]
    UpdateWorker { job_id: String },
    /// Save, close and reopen every window with this installation's executables.
    #[command(hide = true)]
    RestartWindows {
        /// The instance the asking window belongs to.
        #[arg(long)]
        instance: String,
    },
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
    /// Request daemon termination (use --confirm for a counted confirmation).
    Stop {
        #[arg(long, conflicts_with = "confirm")]
        require_empty: bool,
        #[arg(long, conflicts_with = "confirm")]
        expected_generation: Option<u64>,
        /// Confirm termination of the counted hosted sessions. Bare, this
        /// prompts on the terminal. A service unit or script has no terminal,
        /// so it may pass a capability from `daemon preflight` instead — the
        /// same counted authority `cagents-ui --confirm-daemon-stop` uses.
        #[arg(long, value_name = "CAPABILITY_JSON", num_args = 0..=1)]
        confirm: Option<Option<String>>,
    },
    /// Print a short-lived shutdown capability for `daemon stop --confirm`.
    Preflight,
    /// Checkpoint resumable sessions and stop for a full application upgrade.
    PrepareUpgrade {
        /// Authorize interruption and resumption of all supported hosted sessions.
        #[arg(long)]
        yes: bool,
        /// Accept starting fresh conversations for identified agents without an exact resume ID.
        #[arg(long)]
        relaunch_agents: bool,
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

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Open a project in every window's sidebar, with or without a window
    /// running.
    Open {
        /// A path inside the repository (defaults to the current directory).
        path: Option<PathBuf>,
    },
    /// List shared projects, newest first.
    List,
    /// Close a project and every channel in it.
    Close {
        /// A path inside the repository (defaults to the current directory).
        path: Option<PathBuf>,
        /// Confirm deleting the channels the project still holds. Closing a
        /// project takes its channels with it in every window, and a shell has
        /// no sidebar to see them in first.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ChannelCommand {
    /// Create a channel in a repository, with or without a window running.
    Spawn {
        /// Repository the channel belongs to (defaults to the repository
        /// containing the current directory).
        #[arg(long)]
        repository: Option<PathBuf>,
        /// Channel title. Empty - the default - leaves the channel awaiting its
        /// conversation's first title, exactly as one created in a window does.
        #[arg(long, default_value = "")]
        title: String,
    },
    /// List shared channels, newest first.
    List {
        /// Only list channels in this repository.
        #[arg(long)]
        repository: Option<PathBuf>,
    },
    /// Delete a channel everywhere.
    Close { channel_id: ChannelId },
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
    /// Inventory edges that arrived while a correlated response was pending.
    ///
    /// A subscribed connection can be told about a terminal between sending a
    /// request and reading its answer. Buffering keeps the request/response
    /// discipline intact without losing the edge that motivated subscribing.
    observed: VecDeque<TerminalDescriptor>,
    /// Shared-model edges that arrived while a correlated response was pending,
    /// buffered for the same reason `observed` buffers an inventory edge.
    observed_model: VecDeque<(u64, ModelEdit)>,
    /// Native chats this connection is attached to, whose change and activity
    /// notifications the daemon interleaves with responses.
    ///
    /// Attaching is only how this client earns the right to close a chat; it
    /// reads nothing from the chat's notifications, so they are dropped.
    attached_chats: BTreeSet<TerminalId>,
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
            observed: VecDeque::new(),
            observed_model: VecDeque::new(),
            attached_chats: BTreeSet::new(),
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

    /// Subscribes this connection to terminals created by other clients.
    ///
    /// Subscribe before listing: an edge that arrives between the two is
    /// buffered rather than lost, while the reverse order would miss a spawn
    /// that happened in the gap.
    pub fn watch_terminals(&mut self) -> Result<(), ControlError> {
        match self.request_strict(ClientRequest::WatchTerminals)? {
            ServerResponse::Accepted => Ok(()),
            response => Err(unexpected_response("inventory observation", &response)),
        }
    }

    /// Blocks until another client creates a terminal.
    ///
    /// An inventory edge has no deadline, so this drops the read timeout
    /// instead of polling: a timeout that expired mid-frame would desynchronize
    /// the stream, and a subscribed connection issues no further requests.
    pub fn next_created_terminal(&mut self) -> Result<TerminalDescriptor, ControlError> {
        if let Some(terminal) = self.observed.pop_front() {
            return Ok(terminal);
        }
        self.stream
            .set_read_timeout(None)
            .map_err(ControlError::Configure)?;
        loop {
            let envelope = self.read_envelope()?;
            self.validate_pinned_envelope(&envelope)?;
            match envelope.message {
                ServerMessage::TerminalCreated { terminal } => return Ok(terminal),
                // The daemon is about to close this connection. Reporting it as
                // a closed connection is what makes an observer reconnect and
                // resynchronize against the successor's inventory.
                ServerMessage::HostShuttingDown => return Err(ControlError::ConnectionClosed),
                ServerMessage::Error(error) => {
                    return Err(ControlError::Remote {
                        code: error.code,
                        message: error.message,
                        retryable: error.retryable,
                    });
                }
                _ => {}
            }
        }
    }

    /// Subscribes to shared-model edges and returns the model in one round trip.
    ///
    /// Unlike `watch_terminals` this needs no separate read: the daemon answers
    /// the subscription with the model itself, so there is no gap between
    /// subscribing and reading for an edit to fall into.
    pub fn watch_workspace_model(&mut self) -> Result<WorkspaceModel, ControlError> {
        self.expect_model(ClientRequest::WatchWorkspaceModel)
    }

    /// Reads the shared model without subscribing, for a client whose state
    /// machine has no place for an unsolicited frame.
    pub fn read_workspace_model(&mut self) -> Result<WorkspaceModel, ControlError> {
        self.expect_model(ClientRequest::ReadWorkspaceModel)
    }

    /// Applies one edit and returns the resulting model, which is how the
    /// client that minted a channel learns the rank the registry gave it.
    pub fn edit_workspace_model(
        &mut self,
        edit: ModelEdit,
    ) -> Result<WorkspaceModel, ControlError> {
        self.expect_model(ClientRequest::EditWorkspaceModel { edit })
    }

    /// Blocks until another client edits the shared model.
    ///
    /// A model edge has no deadline, so this drops the read timeout rather than
    /// polling: a timeout expiring mid-frame would desynchronize the stream, and
    /// a subscribed connection issues no further requests.
    pub fn next_workspace_model_edit(&mut self) -> Result<(u64, ModelEdit), ControlError> {
        if let Some(edge) = self.observed_model.pop_front() {
            return Ok(edge);
        }
        self.stream
            .set_read_timeout(None)
            .map_err(ControlError::Configure)?;
        loop {
            let envelope = self.read_envelope()?;
            self.validate_pinned_envelope(&envelope)?;
            match envelope.message {
                ServerMessage::WorkspaceModelEdited { sequence, edit } => {
                    return Ok((sequence, edit));
                }
                // The daemon is about to close this connection. Reporting it as
                // a closed connection is what makes a watcher reconnect and
                // resynchronize against the successor's registry.
                ServerMessage::HostShuttingDown => return Err(ControlError::ConnectionClosed),
                ServerMessage::Error(error) => {
                    return Err(ControlError::Remote {
                        code: error.code,
                        message: error.message,
                        retryable: error.retryable,
                    });
                }
                _ => {}
            }
        }
    }

    fn expect_model(&mut self, request: ClientRequest) -> Result<WorkspaceModel, ControlError> {
        match self.request_strict(request)? {
            ServerResponse::WorkspaceModel { model } => Ok(model),
            response => Err(unexpected_response("the shared workspace model", &response)),
        }
    }

    pub fn list_chats(&mut self) -> Result<Vec<crate::chat::Session>, ControlError> {
        match self.request_strict(ClientRequest::ChatList)? {
            ServerResponse::ChatListed { sessions } => Ok(sessions),
            response => Err(unexpected_response("a native chat list", &response)),
        }
    }

    /// Closes a native chat this client did not open.
    ///
    /// The daemon honours a close only from an attached client, so this
    /// attaches first with the session as the inventory reported it. Closing
    /// interrupts the provider's work and frees the conversation; it is what
    /// worktree deletion does to the chats still running inside the directory.
    ///
    /// Attached, this connection is sent the chat's notifications until the
    /// close removes it - the interrupt the close delivers is itself reported
    /// as activity - so they are expected between the requests' responses.
    pub fn close_chat(&mut self, session: crate::chat::Session) -> Result<(), ControlError> {
        let chat_id = session.id;
        match self.request_strict(ClientRequest::ChatAttach { session })? {
            ServerResponse::ChatAttached { .. } => {}
            response => return Err(unexpected_response("a native chat attachment", &response)),
        }
        self.attached_chats.insert(chat_id);
        let closed = self
            .request_strict(ClientRequest::ChatClose { chat_id })
            .and_then(|response| match response {
                ServerResponse::ChatClosed { .. } => Ok(()),
                response => Err(unexpected_response("a native chat close", &response)),
            });
        let detached = match &closed {
            // The daemon removed the chat, and every notification about it,
            // before answering.
            Ok(()) => true,
            // The daemon answered, so the connection is still in step and can
            // stop the notifications of a chat it did not close.
            Err(ControlError::Remote { .. } | ControlError::UnexpectedResponse { .. }) => matches!(
                self.request_strict(ClientRequest::ChatDetach { chat_id }),
                Ok(ServerResponse::Detached)
            ),
            Err(_) => false,
        };
        if detached {
            self.attached_chats.remove(&chat_id);
        }
        closed
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
        relaunch_agents: bool,
    ) -> Result<(u64, u64), ControlError> {
        match self.request_strict(ClientRequest::PrepareUpgrade {
            capability,
            relaunch_agents,
        })? {
            ServerResponse::UpgradePrepared { terminals, chats } => Ok((terminals, chats)),
            response => Err(unexpected_response("an upgrade checkpoint", &response)),
        }
    }

    pub fn daemon_version(&mut self) -> Result<String, ControlError> {
        match self.request_strict(ClientRequest::BuildInfo)? {
            ServerResponse::BuildInfo { version } => Ok(version),
            response => Err(unexpected_response("daemon build information", &response)),
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
                // A subscribed connection can be told about another client's
                // terminal at any point, including mid-replay.
                message => {
                    if let Err(message) = self.absorb(message) {
                        return Err(ControlError::UnexpectedMessage {
                            expected: "the terminal attachment replay",
                            actual: message_name(&message),
                        });
                    }
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
                None => match self.absorb(envelope.message) {
                    Ok(()) => {}
                    Err(message)
                        if streaming_terminal.is_some_and(|id| message_targets(&message, id)) => {}
                    Err(message) => {
                        return Err(ControlError::UnexpectedMessage {
                            expected: "a correlated response",
                            actual: message_name(&message),
                        });
                    }
                },
            }
        }
    }

    /// Takes an unsolicited message this connection asked for, or hands it
    /// back.
    ///
    /// Only a connection that subscribed or attached receives these, at any
    /// point between a request and its response, and it must survive the
    /// request they interrupt.
    fn absorb(&mut self, message: ServerMessage) -> Result<(), ServerMessage> {
        match message {
            ServerMessage::TerminalCreated { terminal } => self.observed.push_back(terminal),
            ServerMessage::WorkspaceModelEdited { sequence, edit } => {
                self.observed_model.push_back((sequence, edit));
            }
            ServerMessage::ChatChanged { chat_id }
            | ServerMessage::ChatObserved { chat_id, .. }
            | ServerMessage::ChatActivity { chat_id, .. }
                if self.attached_chats.contains(&chat_id) => {}
            message => return Err(message),
        }
        Ok(())
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
        | ServerMessage::TerminalCreated { .. }
        | ServerMessage::WorkspaceModelEdited { .. }
        | ServerMessage::ChatAccountUsage { .. }
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
        ServerMessage::TerminalCreated { .. } => "terminal_created",
        ServerMessage::WorkspaceModelEdited { .. } => "workspace_model_edited",
        ServerMessage::ChatAccountUsage { .. } => "chat_account_usage",
    }
}

fn response_name(response: &ServerResponse) -> &'static str {
    match response {
        ServerResponse::BuildInfo { .. } => "build_info",
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
        ServerResponse::ChatTerminal { .. } => "chat_terminal",
        ServerResponse::ChatPreviewed { .. } => "chat_previewed",
        ServerResponse::ChatCandidates { .. } => "chat_candidates",
        ServerResponse::TerminalCloseChecked { .. } => "terminal_close_checked",
        ServerResponse::AgentActivity { .. } => "agent_activity",
        ServerResponse::UpgradePrepared { .. } => "upgrade_prepared",
        ServerResponse::UpgradeChatResolved { .. } => "upgrade_chat_resolved",
        ServerResponse::WorkspaceModel { .. } => "workspace_model",
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
        TopCommand::Project { command } => execute_project(command),
        TopCommand::Channel { command } => execute_channel(command),
        TopCommand::UpdateWorker { job_id } => {
            crate::update::run_worker(&job_id).map_err(|error| AppError::new("update", error))?;
            Ok(CommandOutput {
                command: "update.worker",
                data: json!({ "job_id": job_id, "complete": true }),
                human: "Application update complete".into(),
            })
        }
        TopCommand::RestartWindows { instance } => {
            crate::update::restart_windows(&instance)
                .map_err(|error| AppError::new("update", error))?;
            Ok(CommandOutput {
                command: "update.restart_windows",
                data: json!({ "instance": instance, "reopened": true }),
                human: "Saved windows reopened with the installed build".into(),
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
            let mut daemon_version = None;
            let running = match ControlClient::connect(socket) {
                Ok(mut client) => {
                    daemon_version = Some(client.daemon_version().map_err(protocol_error)?);
                    true
                }
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
                    "socket": socket,
                    "daemon_version": daemon_version,
                    "cli_version": crate::VERSION
                }),
                human: if running {
                    format!(
                        "{} daemon {} running ({}); cli {}",
                        instance::NAME,
                        daemon_version.as_deref().unwrap_or("starting"),
                        socket.display(),
                        crate::VERSION
                    )
                } else {
                    format!("{} daemon stopped; cli {}", instance::NAME, crate::VERSION)
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
            let outcome = match confirm {
                // A capability is already a counted, daemon-bound, expiring
                // authority over one observed inventory, so a script that
                // obtained one has confirmed exactly what a prompt would.
                Some(Some(capability)) => {
                    let capability: ShutdownCapability = serde_json::from_str(&capability)
                        .map_err(|error| {
                            AppError::new(
                                "invalid_capability",
                                format!(
                                    "--confirm value is not a capability from `cagents daemon preflight`: {error}"
                                ),
                            )
                        })?;
                    client
                        .confirm_shutdown(capability)
                        .map_err(protocol_error)?
                }
                Some(None) => {
                    if !io::stdin().is_terminal() {
                        return Err(AppError::new(
                            "confirmation_required",
                            "interactive confirmation requires a terminal; pass a capability from `cagents daemon preflight` to --confirm, or use --require-empty",
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
                }
                None => client
                    .shutdown_with_policy(require_empty, expected_generation)
                    .map_err(protocol_error)?,
            };
            shutdown_output(outcome)
        }
        DaemonCommand::Preflight => {
            let socket =
                daemon_socket_path().map_err(|error| AppError::new("socket_path", error))?;
            let mut client = ControlClient::connect(&socket).map_err(protocol_error)?;
            let preflight = client.shutdown_preflight().map_err(protocol_error)?;
            let capability = serde_json::to_string(&preflight.capability)
                .map_err(|error| AppError::new("serialization", error))?;
            Ok(CommandOutput {
                command: "daemon.preflight",
                data: json!({ "capability": preflight.capability }),
                // The human form is the exact argument for `stop --confirm`.
                human: capability,
            })
        }
        DaemonCommand::PrepareUpgrade {
            yes,
            relaunch_agents,
        } => {
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
                .prepare_upgrade(preflight.capability, relaunch_agents)
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
        // Exhaustive by construction: a new rejection reason must be named
        // here rather than collapse into an `unreachable!` catch-all.
        ShutdownOutcome::ConfirmationRequired { current_count } => {
            json!({ "outcome": "confirmation_required", "current_count": current_count, "termination_requested": false })
        }
        ShutdownOutcome::Expired => {
            json!({ "outcome": "expired", "termination_requested": false })
        }
        ShutdownOutcome::DaemonRestarted => {
            json!({ "outcome": "daemon_restarted", "termination_requested": false })
        }
        ShutdownOutcome::InvalidCapability => {
            json!({ "outcome": "invalid_capability", "termination_requested": false })
        }
        ShutdownOutcome::AlreadyShuttingDown => {
            json!({ "outcome": "already_shutting_down", "termination_requested": false })
        }
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
        ShutdownOutcome::ConfirmationRequired { current_count } => format!(
            "Shutdown rejected: confirmation_required, current_count={current_count}, termination_requested=false; \
             hosted sessions still own work — re-run with `cagents daemon stop --confirm`, or \
             `cagents daemon stop --confirm \"$(cagents daemon preflight)\"` when no terminal is attached"
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
                recovery_origin: None,
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

/// Runs a shared-project verb against the daemon's registry.
///
/// A project is a registry row rather than a window instruction, exactly as a
/// channel is: the sidebar shows what the registry holds, whether or not a
/// window was running when it was put there. No request of its own was needed
/// for any of this - the shared model travels as `ReadWorkspaceModel` and
/// `EditWorkspaceModel`, which carry whichever [`ModelEdit`] the verb means, so
/// the wire protocol and the daemon's handler are untouched.
fn execute_project(command: ProjectCommand) -> Result<CommandOutput, AppError> {
    // Resolved before any daemon is started, for the same reason a channel verb
    // resolves first: an argument error must not leave behind a daemon the user
    // never asked for.
    let command = command.resolved()?;
    let socket =
        terminal::ensure_daemon_ready().map_err(|error| AppError::new("daemon_start", error))?;
    let mut client = ControlClient::connect(&socket)
        .map_err(|error| AppError::new("daemon_connection", error))?;
    project_output(&mut client, command)
}

/// A project verb whose repository is already the shared key.
enum ResolvedProject {
    Open {
        repository: PathBuf,
    },
    List,
    Close {
        repository: PathBuf,
        confirmed: bool,
    },
}

impl ProjectCommand {
    fn resolved(self) -> Result<ResolvedProject, AppError> {
        Ok(match self {
            Self::Open { path } => ResolvedProject::Open {
                repository: resolve_repository(path)?,
            },
            Self::List => ResolvedProject::List,
            Self::Close { path, yes } => ResolvedProject::Close {
                repository: resolve_repository(path)?,
                confirmed: yes,
            },
        })
    }
}

fn project_output(
    client: &mut ControlClient,
    command: ResolvedProject,
) -> Result<CommandOutput, AppError> {
    match command {
        ResolvedProject::Open { repository } => {
            // The same upsert a window publishes when it opens a repository,
            // carrying the name derived from the same key, so a project opened
            // from a shell and one opened in a window are one row that no
            // sidebar can tell apart.
            let model = client
                .edit_workspace_model(ModelEdit::OpenProject {
                    repository: repository.clone(),
                    name: repository_display_name(&repository),
                })
                .map_err(protocol_error)?;
            let project = model.project(&repository).cloned().ok_or_else(|| {
                AppError::new(
                    "protocol",
                    "the daemon accepted the project but did not report it",
                )
            })?;
            let human = project_line(&project);
            Ok(CommandOutput {
                command: "project.open",
                data: json!({ "project": project, "sequence": model.sequence }),
                human,
            })
        }
        ResolvedProject::List => {
            let model = client.read_workspace_model().map_err(protocol_error)?;
            // Newest first, the order `channel list` answers in and the order
            // the sidebar renders.
            let mut projects = model.projects.iter().collect::<Vec<_>>();
            projects.sort_by(|left, right| right.ordinal.cmp(&left.ordinal));
            let human = projects
                .iter()
                .map(|project| project_line(project))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(CommandOutput {
                command: "project.list",
                data: json!({ "projects": projects, "sequence": model.sequence }),
                human,
            })
        }
        ResolvedProject::Close {
            repository,
            confirmed,
        } => close_project(client, repository, confirmed),
    }
}

/// Closes a project and reports every row that went with it.
///
/// The read comes first because the answer to the close cannot name what the
/// close destroyed: the registry removes the project's channels through its
/// foreign key's cascade, and every window folds the same edit and retires
/// those channels - tombstoning the cards inside them. That is real work
/// disappearing from windows the caller cannot see, so a project that still
/// holds channels is refused until `--yes` confirms it, and the refusal names
/// the channels rather than counting them. An empty project is not destructive
/// at all and needs no flag: a confirmation demanded where nothing can be lost
/// is a confirmation the user learns to pass without reading.
fn close_project(
    client: &mut ControlClient,
    repository: PathBuf,
    confirmed: bool,
) -> Result<CommandOutput, AppError> {
    let model = client.read_workspace_model().map_err(protocol_error)?;
    let project = model.project(&repository).cloned();
    let mut channels = model
        .channels
        .iter()
        .filter(|channel| channel.repository == repository)
        .cloned()
        .collect::<Vec<_>>();
    channels.sort_by(|left, right| right.ordinal.cmp(&left.ordinal));
    if !channels.is_empty() && !confirmed {
        let listed = channels
            .iter()
            .map(channel_line)
            .collect::<Vec<_>>()
            .join("\n");
        return Err(AppError {
            kind: "confirmation",
            message: format!(
                "closing `{}` deletes {} channel(s) in every window; pass --yes to confirm:\n{listed}",
                repository.display(),
                channels.len(),
            ),
            details: Some(json!({ "channels": channels })),
        });
    }
    client
        .edit_workspace_model(ModelEdit::CloseProject {
            repository: repository.clone(),
        })
        .map_err(protocol_error)?;
    // Exactly what was removed, in the spellings `project list` and
    // `channel list` print: the project's own line, then one line per channel
    // that went with it. A project the registry did not hold removed nothing
    // and prints nothing.
    let human = project
        .iter()
        .map(project_line)
        .chain(channels.iter().map(channel_line))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(CommandOutput {
        command: "project.close",
        data: json!({
            "repository": repository,
            "closed": project.is_some(),
            "project": project,
            "channels": channels,
        }),
        human,
    })
}

/// One project per line: repository, name.
///
/// The key comes first, following `channel_line`, so the `cut -f1` that names a
/// channel names a project - and names it in the spelling `project close`
/// takes.
fn project_line(project: &ModelProject) -> String {
    format!("{}\t{}", project.repository.display(), project.name)
}

/// Runs a shared-channel verb against the daemon's registry.
///
/// Every verb here is a registry edit or read, never a window instruction: the
/// point of the shared model is that a channel exists whether or not a window
/// is running to show it.
fn execute_channel(command: ChannelCommand) -> Result<CommandOutput, AppError> {
    // Resolved before any daemon is started. A path that names no repository is
    // an argument error, and an argument error must not leave behind a daemon
    // the user never asked for.
    let command = command.resolved()?;
    let socket =
        terminal::ensure_daemon_ready().map_err(|error| AppError::new("daemon_start", error))?;
    let mut client = ControlClient::connect(&socket)
        .map_err(|error| AppError::new("daemon_connection", error))?;
    channel_output(&mut client, command)
}

/// A channel verb whose repository is already the shared key.
///
/// A distinct type rather than a convention, so the resolution cannot drift
/// back inside the request path - where it would run after the daemon was
/// started, and where `list` would resolve a key the caller had already
/// resolved.
enum ResolvedChannel {
    Spawn { repository: PathBuf, title: String },
    List { repository: Option<PathBuf> },
    Close { channel_id: ChannelId },
}

impl ChannelCommand {
    fn resolved(self) -> Result<ResolvedChannel, AppError> {
        Ok(match self {
            Self::Spawn { repository, title } => ResolvedChannel::Spawn {
                repository: resolve_repository(repository)?,
                title,
            },
            // Only an explicit filter is resolved: no `--repository` means every
            // repository, so defaulting to the one containing the current
            // directory would silently hide channels the user asked to see.
            Self::List { repository } => ResolvedChannel::List {
                repository: repository
                    .map(|path| resolve_repository(Some(path)))
                    .transpose()?,
            },
            Self::Close { channel_id } => ResolvedChannel::Close { channel_id },
        })
    }
}

fn channel_output(
    client: &mut ControlClient,
    command: ResolvedChannel,
) -> Result<CommandOutput, AppError> {
    match command {
        ResolvedChannel::Spawn { repository, title } => {
            // The project comes first: a channel in a repository no project owns
            // is reachable from no window's sidebar. `OpenProject` is an upsert,
            // so this is a no-op when a window already has the project open.
            client
                .edit_workspace_model(ModelEdit::OpenProject {
                    repository: repository.clone(),
                    name: repository_display_name(&repository),
                })
                .map_err(protocol_error)?;
            // The id is minted here, exactly as `terminal spawn` mints its
            // `TerminalId`, so the caller names what it created without having
            // to correlate a server-assigned identity back to its request.
            let id = ChannelId::new();
            let model = client
                .edit_workspace_model(ModelEdit::CreateChannel {
                    id,
                    repository,
                    title,
                })
                .map_err(protocol_error)?;
            let channel = model.channel(id).cloned().ok_or_else(|| {
                AppError::new(
                    "protocol",
                    "the daemon accepted the channel but did not report it",
                )
            })?;
            let human = channel_line(&channel);
            Ok(CommandOutput {
                command: "channel.spawn",
                data: json!({ "channel": channel, "sequence": model.sequence }),
                human,
            })
        }
        ResolvedChannel::List { repository: filter } => {
            let model = client.read_workspace_model().map_err(protocol_error)?;
            // Newest first, which is the order the sidebar renders: a listing
            // that disagreed with the window about which channel is on top would
            // be a second answer to a question with one.
            let mut channels = model
                .channels
                .iter()
                .filter(|channel| {
                    filter
                        .as_ref()
                        .is_none_or(|repository| channel.repository == *repository)
                })
                .collect::<Vec<_>>();
            channels.sort_by(|left, right| right.ordinal.cmp(&left.ordinal));
            let human = channels
                .iter()
                .map(|channel| channel_line(channel))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(CommandOutput {
                command: "channel.list",
                data: json!({ "channels": channels, "sequence": model.sequence }),
                human,
            })
        }
        ResolvedChannel::Close { channel_id } => {
            client
                .edit_workspace_model(ModelEdit::DeleteChannel { id: channel_id })
                .map_err(protocol_error)?;
            Ok(CommandOutput {
                command: "channel.close",
                data: json!({ "channel_id": channel_id, "closed": true }),
                human: format!("closed {channel_id}"),
            })
        }
    }
}

/// One channel per line: id, repository, title.
///
/// Tab separated and in that order, following `descriptor_line`, so the same
/// `cut -f1` that names a terminal names a channel.
fn channel_line(channel: &ModelChannel) -> String {
    format!(
        "{}\t{}\t{}",
        channel.id,
        channel.repository.display(),
        channel.title,
    )
}

/// Resolves the directory a project or channel verb refers to into the shared
/// key.
///
/// Both forms go through Git, including a path the user named: a user names a
/// checkout, and the key is not the checkout's path. A path that is not inside
/// a repository is an error rather than a key, because no window could ever own
/// a project there.
fn resolve_repository(repository: Option<PathBuf>) -> Result<PathBuf, AppError> {
    let from = absolute_working_directory(repository)
        .map_err(|error| AppError::new("repository", error))?;
    repository_key(&from).map_err(|error| AppError::new("repository", error))
}

/// The shared project key for the repository containing `directory`, asked of
/// Git itself.
///
/// `--git-common-dir` and nothing else. A project's identity in the window is
/// `ProjectState.repository`, which the repository loader fills from the same
/// query, so asking Git the same question here makes the two representations
/// coincide by construction rather than by a fixup. The alternatives are both
/// wrong: `--show-toplevel` is the work tree rather than the repository, and
/// `--absolute-git-dir` answers `<root>/.git/worktrees/<name>` inside a linked
/// worktree, so the same project would get one key per worktree the CLI
/// happened to be invoked from. The common directory is the one form that is
/// identical from the work tree root, from a subdirectory, from a linked
/// worktree and from inside the Git directory itself. `repository_git_contract`
/// holds the two derivations to each other, because a key that agrees with the
/// window only by inspection is a key that will drift.
///
/// Public so that contract can be stated against the real `git` rather than
/// against a copy of this logic.
/// The crate's own repository helpers are `ui`-gated - the shipped `cagents`
/// binary has no Git library in its dependency graph at all - and adding one for
/// a single query would be a dependency for a question the `git` on the user's
/// path already answers, with the user's own configuration applied.
pub fn repository_key(directory: &Path) -> Result<PathBuf, String> {
    // Checked before spawning, because `current_dir` on a path that is not
    // there fails as a spawn error and would be reported as a broken `git`
    // install - sending the user to look at the wrong thing entirely.
    if !directory.is_dir() {
        return Err(format!("`{}` is not a directory", directory.display()));
    }
    let output = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(directory)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if !output.status.success() {
        // Named without a hint about which argument to correct: the same
        // derivation serves `channel --repository` and a positional `project`
        // path, and a hint that names the wrong one is worse than none.
        return Err(format!(
            "`{}` is not inside a Git repository",
            directory.display()
        ));
    }
    let key = String::from_utf8(output.stdout)
        .map_err(|_| "git reported a repository path that is not valid UTF-8".to_owned())?;
    let key = key.trim_end();
    if key.is_empty() {
        return Err("git reported an empty repository path".to_owned());
    }
    // Canonicalized here rather than by the caller, so the public derivation is
    // the whole key and nothing is left for a producer to remember to do.
    canonical_repository(Path::new(key))
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

    fn parsed_confirm<const N: usize>(arguments: [&str; N]) -> Option<Option<String>> {
        match Cli::try_parse_from(arguments).unwrap().command {
            TopCommand::Daemon {
                command: DaemonCommand::Stop { confirm, .. },
            } => confirm,
            other => panic!("expected a daemon stop command, got {other:?}"),
        }
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
                    confirm: None
                }
            }
        ));
        assert!(
            Cli::try_parse_from(["cagents", "daemon", "stop", "--require-empty", "--confirm"])
                .is_err()
        );
        // A service unit has no terminal, so `--confirm` must also accept the
        // counted capability that replaces the prompt.
        assert_eq!(
            parsed_confirm(["cagents", "daemon", "stop", "--confirm", "{\"token\":1}"]),
            Some(Some("{\"token\":1}".to_owned()))
        );
        assert_eq!(
            parsed_confirm(["cagents", "daemon", "stop", "--confirm"]),
            Some(None)
        );
        assert_eq!(parsed_confirm(["cagents", "daemon", "stop"]), None);
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

    /// `channel spawn` opens the project before creating the channel in it,
    /// mints the id itself, and reports the created channel under the
    /// `channel.spawn` envelope every automation keys on.
    ///
    /// The fixture is a real repository because the key is whatever `git`
    /// resolves, not the path the user named: a fixture that never ran `git`
    /// could not tell the two apart, which is how a key no window could match
    /// shipped.
    #[test]
    fn channel_spawn_opens_the_project_then_creates_the_channel_it_minted() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let peer_proof = proof("session", "boot", "incarnation");
        // No usable `git` means there is no key to derive and nothing this test
        // can assert. Every lane that runs it has one.
        let Some(checkout) = git_checkout() else {
            return;
        };
        let repository = checkout.path().to_path_buf();
        // The common Git directory, which is what a window stores for a project
        // and therefore what the channel has to be keyed on.
        let expected =
            crate::workspace_model::canonical_repository(&repository.join(".git")).unwrap();
        let server_repository = expected.clone();
        // Derived the same way the CLI derives it, from the checkout the key's
        // Git directory belongs to.
        let expected_name = checkout
            .path()
            .file_name()
            .expect("the fixture checkout is named")
            .to_string_lossy()
            .into_owned();
        let server = thread::spawn(move || {
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &connected_envelope(1, peer_proof.clone(), peer_proof.clone()),
            )
            .unwrap();
            let opened: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(
                opened.request,
                ClientRequest::EditWorkspaceModel {
                    edit: ModelEdit::OpenProject {
                        repository: server_repository.clone(),
                        // The checkout's name, never the key's own last
                        // component, which is `.git` and names nothing.
                        name: expected_name.clone(),
                    },
                },
                "the project a channel needs is opened first"
            );
            write_frame(
                &mut server_stream,
                &model_envelope(
                    opened.request_id,
                    peer_proof.clone(),
                    WorkspaceModel::default(),
                ),
            )
            .unwrap();
            let created: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            let ClientRequest::EditWorkspaceModel {
                edit:
                    ModelEdit::CreateChannel {
                        id,
                        repository: asked,
                        title,
                    },
            } = created.request
            else {
                panic!("expected a channel creation, got {:?}", created.request);
            };
            assert_eq!(asked, server_repository);
            assert_eq!(title, "probe");
            let model = WorkspaceModel {
                identity: uuid::Uuid::new_v4(),
                sequence: 2,
                projects: Vec::new(),
                channels: vec![ModelChannel {
                    id,
                    repository: server_repository,
                    title,
                    ordinal: 4,
                }],
            };
            write_frame(
                &mut server_stream,
                &model_envelope(created.request_id, peer_proof, model),
            )
            .unwrap();
            id
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        // Through the real resolution, so the test proves the key the CLI sends
        // is the one `git` derives rather than the one the test hands it.
        let resolved = ChannelCommand::Spawn {
            repository: Some(repository),
            title: "probe".into(),
        }
        .resolved()
        .unwrap();
        let output = channel_output(&mut client, resolved).unwrap();
        let id = server.join().unwrap();
        assert_eq!(output.command, "channel.spawn");
        assert_eq!(output.data["channel"]["id"], json!(id.to_string()));
        assert_eq!(output.data["channel"]["ordinal"], json!(4));
        assert_eq!(output.data["sequence"], json!(2));
        assert_eq!(
            output.human,
            format!("{id}\t{}\tprobe", expected.display()),
            "one tab-separated line: id, repository, title"
        );
    }

    fn model_envelope(
        request_id: u64,
        envelope_proof: HostSessionProof,
        model: WorkspaceModel,
    ) -> ServerEnvelope {
        ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request_id),
            proof: envelope_proof,
            message: ServerMessage::Response {
                response: ServerResponse::WorkspaceModel { model },
            },
        }
    }

    /// A real repository, because every key here is whatever `git` resolves
    /// rather than the path the test names. `None` when there is no usable
    /// `git`, so the suite skips instead of failing on a machine without one.
    fn git_checkout() -> Option<tempfile::TempDir> {
        let checkout = tempfile::tempdir().unwrap();
        let initialized = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(checkout.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        initialized
            .is_ok_and(|status| status.success())
            .then_some(checkout)
    }

    fn git(checkout: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(checkout)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Cagents")
            .env("GIT_AUTHOR_EMAIL", "cagents@example.invalid")
            .env("GIT_COMMITTER_NAME", "Cagents")
            .env("GIT_COMMITTER_EMAIL", "cagents@example.invalid")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {arguments:?}");
    }

    /// The project a shell opens is the project a window opens, from wherever
    /// the shell happened to be standing.
    ///
    /// A linked worktree is the case that discriminates: its own Git directory
    /// is `<common>/worktrees/<name>`, so keying on that would give one
    /// checkout one project per worktree the user ever ran the verb from, and
    /// the sidebar would fill with duplicates of the same repository.
    #[test]
    fn project_open_keys_one_project_from_every_worktree_of_one_checkout() {
        let Some(checkout) = git_checkout() else {
            return;
        };
        std::fs::write(checkout.path().join("file"), b"one\n").unwrap();
        git(checkout.path(), &["add", "-A"]);
        git(checkout.path(), &["commit", "--quiet", "-m", "initial"]);
        let linked = checkout.path().join("linked");
        git(
            checkout.path(),
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );

        let key = |path: &Path| {
            let ResolvedProject::Open { repository } = ProjectCommand::Open {
                path: Some(path.to_path_buf()),
            }
            .resolved()
            .unwrap_or_else(|error| panic!("{} resolves: {}", path.display(), error.message)) else {
                panic!("`project open` resolves to an open");
            };
            repository
        };

        let expected = canonical_repository(&checkout.path().join(".git")).unwrap();
        assert_eq!(key(checkout.path()), expected);
        assert_eq!(
            key(&linked),
            expected,
            "a linked worktree is the same project as the checkout it belongs to"
        );
    }

    /// Both argument errors are settled by the argument alone, so they answer
    /// without a daemon: starting one for a command that cannot succeed leaves
    /// a process behind that the user never asked for and did not get a result
    /// from. `ResolvedProject` is the proof this ordering cannot drift -
    /// `project_output` takes nothing else, and only `resolved` makes one.
    #[test]
    fn a_project_path_that_names_no_repository_fails_before_any_daemon() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("file");
        std::fs::write(&file, b"not a directory\n").unwrap();

        let Err(error) = (ProjectCommand::Open {
            path: Some(file.clone()),
        })
        .resolved() else {
            panic!("a file is not a repository");
        };
        assert_eq!(error.kind, "repository");
        assert_eq!(
            error.message,
            format!("`{}` is not a directory", file.display())
        );

        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let Err(error) = (ProjectCommand::Close {
            path: Some(directory.path().to_path_buf()),
            yes: true,
        })
        .resolved() else {
            panic!("a directory outside any repository is not a project");
        };
        assert_eq!(
            error.message,
            format!(
                "`{}` is not inside a Git repository",
                directory.path().display()
            )
        );
    }

    /// `project open` is the upsert a window publishes, with the key `git`
    /// resolves and the name the checkout carries, and it reports the project
    /// the registry ended up holding - including the rank, which only the
    /// registry knows.
    #[test]
    fn project_open_publishes_the_upsert_a_window_would_and_reports_the_row() {
        let Some(checkout) = git_checkout() else {
            return;
        };
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let peer_proof = proof("session", "boot", "incarnation");
        let expected = canonical_repository(&checkout.path().join(".git")).unwrap();
        let name = checkout
            .path()
            .file_name()
            .expect("the fixture checkout is named")
            .to_string_lossy()
            .into_owned();
        let served = expected.clone();
        let served_name = name.clone();
        let server = thread::spawn(move || {
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &connected_envelope(1, peer_proof.clone(), peer_proof.clone()),
            )
            .unwrap();
            let opened: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(
                opened.request,
                ClientRequest::EditWorkspaceModel {
                    edit: ModelEdit::OpenProject {
                        repository: served.clone(),
                        // The checkout's name, never the key's own last
                        // component, which is `.git` and names nothing.
                        name: served_name.clone(),
                    },
                },
                "the CLI opens a project exactly as a window publishes one"
            );
            let model = WorkspaceModel {
                identity: uuid::Uuid::new_v4(),
                sequence: 4,
                projects: vec![ModelProject {
                    repository: served,
                    name: served_name,
                    ordinal: 2,
                }],
                channels: Vec::new(),
            };
            write_frame(
                &mut server_stream,
                &model_envelope(opened.request_id, peer_proof, model),
            )
            .unwrap();
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let resolved = ProjectCommand::Open {
            path: Some(checkout.path().to_path_buf()),
        }
        .resolved()
        .unwrap();
        let output = project_output(&mut client, resolved).unwrap();
        server.join().unwrap();
        assert_eq!(output.command, "project.open");
        assert_eq!(output.data["project"]["ordinal"], json!(2));
        assert_eq!(output.data["sequence"], json!(4));
        assert_eq!(
            output.human,
            format!("{}\t{name}", expected.display()),
            "one tab-separated line: repository, name"
        );
    }

    /// A listing is of the registry, which records no source: a project a
    /// window opened and one a shell opened are the same row, listed by the
    /// same rule, newest first.
    #[test]
    fn project_list_answers_newest_first_and_tells_no_source_apart() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let peer_proof = proof("session", "boot", "incarnation");
        let window_opened = ModelProject {
            repository: PathBuf::from("/window/.git"),
            name: "window".into(),
            ordinal: 1,
        };
        let cli_opened = ModelProject {
            repository: PathBuf::from("/shell/.git"),
            name: "shell".into(),
            ordinal: 2,
        };
        let served = vec![window_opened.clone(), cli_opened.clone()];
        let server = thread::spawn(move || {
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &connected_envelope(1, peer_proof.clone(), peer_proof.clone()),
            )
            .unwrap();
            let listed: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(listed.request, ClientRequest::ReadWorkspaceModel);
            let model = WorkspaceModel {
                identity: uuid::Uuid::new_v4(),
                sequence: 7,
                projects: served,
                channels: Vec::new(),
            };
            write_frame(
                &mut server_stream,
                &model_envelope(listed.request_id, peer_proof, model),
            )
            .unwrap();
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let output = project_output(&mut client, ProjectCommand::List.resolved().unwrap()).unwrap();
        server.join().unwrap();
        assert_eq!(output.command, "project.list");
        assert_eq!(
            output.human, "/shell/.git\tshell\n/window/.git\twindow",
            "newest first, one tab-separated line each, whoever opened them"
        );
        assert_eq!(output.data["projects"], json!([cli_opened, window_opened]));
    }

    /// Closing a project deletes every channel in it, in every window, and the
    /// shell it was asked from has no sidebar to see them in. So a project that
    /// still holds channels is refused until `--yes`, the refusal names them,
    /// and no edit is sent - the daemon never hears about a close the user did
    /// not confirm.
    #[test]
    fn project_close_refuses_to_take_channels_with_it_unconfirmed() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let peer_proof = proof("session", "boot", "incarnation");
        let repository = PathBuf::from("/repo/.git");
        let doomed = ChannelId::new();
        let served = model_with_channels(&repository, &[(doomed, "review", 3)]);
        let server = thread::spawn(move || {
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &connected_envelope(1, peer_proof.clone(), peer_proof.clone()),
            )
            .unwrap();
            let read: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(read.request, ClientRequest::ReadWorkspaceModel);
            write_frame(
                &mut server_stream,
                &model_envelope(read.request_id, peer_proof, served),
            )
            .unwrap();
            // Nothing else may arrive: the client drops the connection without
            // ever sending an edit.
            read_frame::<ClientEnvelope>(&mut server_stream).unwrap()
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let Err(error) = project_output(
            &mut client,
            ResolvedProject::Close {
                repository,
                confirmed: false,
            },
        ) else {
            panic!("an unconfirmed close of a project with channels is refused");
        };
        drop(client);
        assert_eq!(
            server.join().unwrap(),
            None,
            "a refused close sends the daemon no edit at all"
        );
        assert_eq!(error.kind, "confirmation");
        assert!(
            error.message.contains(&doomed.to_string()),
            "the refusal names the channels rather than counting them: {}",
            error.message
        );
        assert_eq!(error.details.unwrap()["channels"][0]["title"], "review");
    }

    /// Confirmed, the close reports exactly what it removed: the project and
    /// every channel that went with it, in the spellings `project list` and
    /// `channel list` print. The registry's answer cannot say - the cascade has
    /// already taken them - so the report is read first.
    #[test]
    fn a_confirmed_project_close_reports_the_channels_it_took() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let peer_proof = proof("session", "boot", "incarnation");
        let repository = PathBuf::from("/repo/.git");
        let older = ChannelId::new();
        let newer = ChannelId::new();
        let served = model_with_channels(&repository, &[(older, "older", 1), (newer, "newer", 5)]);
        let closed = repository.clone();
        let server = thread::spawn(move || {
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &connected_envelope(1, peer_proof.clone(), peer_proof.clone()),
            )
            .unwrap();
            let read: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &model_envelope(read.request_id, peer_proof.clone(), served),
            )
            .unwrap();
            let edit: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(
                edit.request,
                ClientRequest::EditWorkspaceModel {
                    edit: ModelEdit::CloseProject {
                        repository: closed.clone(),
                    },
                },
            );
            // What the registry is left with: the project and its channels are
            // gone together, through the store's own cascade.
            write_frame(
                &mut server_stream,
                &model_envelope(
                    edit.request_id,
                    peer_proof,
                    WorkspaceModel {
                        identity: uuid::Uuid::new_v4(),
                        sequence: 9,
                        projects: Vec::new(),
                        channels: Vec::new(),
                    },
                ),
            )
            .unwrap();
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let output = project_output(
            &mut client,
            ResolvedProject::Close {
                repository: repository.clone(),
                confirmed: true,
            },
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(output.command, "project.close");
        assert_eq!(output.data["closed"], json!(true));
        assert_eq!(
            output.human,
            format!("/repo/.git\trepo\n{newer}\t/repo/.git\tnewer\n{older}\t/repo/.git\tolder"),
            "the project's line, then every channel it took, newest first"
        );
        let channels = output.data["channels"].as_array().unwrap();
        assert_eq!(
            channels
                .iter()
                .map(|channel| channel["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>(),
            vec![newer.to_string(), older.to_string()],
            "and names them under the field name `channel list` uses"
        );
    }

    /// A registry holding one project and the channels named in it.
    fn model_with_channels(
        repository: &Path,
        channels: &[(ChannelId, &str, u32)],
    ) -> WorkspaceModel {
        WorkspaceModel {
            identity: uuid::Uuid::new_v4(),
            sequence: 8,
            projects: vec![ModelProject {
                repository: repository.to_path_buf(),
                name: repository_display_name(repository),
                ordinal: 1,
            }],
            channels: channels
                .iter()
                .map(|(id, title, ordinal)| ModelChannel {
                    id: *id,
                    repository: repository.to_path_buf(),
                    title: (*title).to_owned(),
                    ordinal: *ordinal,
                })
                .collect(),
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
                    | (
                        1,
                        ControlError::UnexpectedMessage {
                            actual: "redraw",
                            ..
                        }
                    )
            ));
            server.join().unwrap();
        }
    }

    /// The CLI's spawn contract is the descriptor it returns. An inventory edge
    /// can land between the request and its answer on a subscribed connection,
    /// and neither the answer nor the edge may be lost.
    #[test]
    fn a_spawn_returns_its_descriptor_while_an_interleaved_inventory_edge_survives() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let pinned = proof("session", "boot", "incarnation");
        let recipe = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: PathBuf::from("/tmp"),
            environment: BTreeMap::new(),
        };
        let session = TerminalSession {
            recovery_origin: None,
            id: TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: recipe.clone(),
                last_exit: None,
            },
        };
        let spawned = session.id;
        let elsewhere = TerminalId::new();
        let peer_proof = pinned.clone();
        let peer_recipe = recipe.clone();
        let server = thread::spawn(move || {
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &connected_envelope(1, peer_proof.clone(), peer_proof.clone()),
            )
            .unwrap();
            let watch: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(matches!(watch.request, ClientRequest::WatchTerminals));
            write_frame(
                &mut server_stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(watch.request_id),
                    proof: peer_proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Accepted,
                    },
                },
            )
            .unwrap();
            let spawn: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(matches!(spawn.request, ClientRequest::Spawn { .. }));
            for message in [
                ServerMessage::TerminalCreated {
                    terminal: TerminalDescriptor::relaunchable(
                        elsewhere,
                        peer_recipe.clone(),
                        None,
                    ),
                },
                ServerMessage::Response {
                    response: ServerResponse::Spawned {
                        terminal: TerminalDescriptor::relaunchable(spawned, peer_recipe, None),
                    },
                },
                ServerMessage::Synchronized {
                    terminal_id: spawned,
                    generation: 0,
                    event_sequence: 0,
                },
            ] {
                let request_id =
                    matches!(message, ServerMessage::Response { .. }).then_some(spawn.request_id);
                write_frame(
                    &mut server_stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: peer_proof.clone(),
                        message,
                    },
                )
                .unwrap();
            }
        });

        let mut client = ControlClient::from_stream(client_stream).unwrap();
        client.watch_terminals().unwrap();
        let descriptor = client
            .spawn(session, TerminalSpawnOptions::default())
            .unwrap();
        assert_eq!(descriptor.session.id, spawned);
        assert_eq!(descriptor.session.launch, recipe);
        assert_eq!(
            client.next_created_terminal().unwrap().session.id,
            elsewhere
        );
        server.join().unwrap();
    }
    /// Closing a chat attaches this connection to it, and the interrupt the
    /// close delivers is reported to every attached client before the answer.
    /// Neither the close nor the requests after it may mistake that report for
    /// a stray response; a chat this connection never attached still may not
    /// interleave.
    #[test]
    fn closing_a_chat_survives_its_own_notifications_but_no_one_elses() {
        use crate::agent::ActivityState;

        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let pinned = proof("session", "boot", "incarnation");
        let recipe = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: PathBuf::from("/tmp"),
            environment: BTreeMap::new(),
        };
        let session = |id| crate::chat::Session {
            id,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: recipe.clone(),
                session_id: None,
            },
            proof: Some(pinned.clone()),
            terminal: None,
        };
        let [closed, refused, elsewhere] =
            [TerminalId::new(), TerminalId::new(), TerminalId::new()];
        let peer_proof = pinned.clone();
        let server = thread::spawn(move || {
            let send = |stream: &mut UnixStream, request_id, message| {
                write_frame(
                    stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: peer_proof.clone(),
                        message,
                    },
                )
                .unwrap();
            };
            let respond = |stream: &mut UnixStream, request_id, response| {
                send(
                    stream,
                    Some(request_id),
                    ServerMessage::Response { response },
                );
            };
            let activity = |chat_id, state: ActivityState| ServerMessage::ChatActivity {
                chat_id,
                state,
                generation: 1,
            };
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &connected_envelope(1, peer_proof.clone(), peer_proof.clone()),
            )
            .unwrap();

            // A close the daemon carries out.
            let attach: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            let ClientRequest::ChatAttach { session } = attach.request else {
                panic!("expected an attachment, got {:?}", attach.request);
            };
            assert_eq!(session.id, closed);
            respond(
                &mut server_stream,
                attach.request_id,
                ServerResponse::ChatAttached {
                    session,
                    snapshot: crate::chat::Snapshot::default(),
                },
            );
            send(
                &mut server_stream,
                None,
                ServerMessage::ChatObserved {
                    chat_id: closed,
                    epoch: 1,
                },
            );
            let close: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(close.request, ClientRequest::ChatClose { chat_id: closed });
            send(
                &mut server_stream,
                None,
                activity(closed, ActivityState::Idle),
            );
            send(
                &mut server_stream,
                None,
                ServerMessage::ChatChanged { chat_id: closed },
            );
            respond(
                &mut server_stream,
                close.request_id,
                ServerResponse::ChatClosed {
                    chat_id: closed,
                    snapshot: crate::chat::Snapshot::default(),
                },
            );

            // A close the daemon refuses: the connection detaches instead.
            let attach: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            let ClientRequest::ChatAttach { session } = attach.request else {
                panic!("expected an attachment, got {:?}", attach.request);
            };
            respond(
                &mut server_stream,
                attach.request_id,
                ServerResponse::ChatAttached {
                    session,
                    snapshot: crate::chat::Snapshot::default(),
                },
            );
            let close: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            send(
                &mut server_stream,
                None,
                activity(refused, ActivityState::Working),
            );
            send(
                &mut server_stream,
                Some(close.request_id),
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::TerminalUnavailable,
                    "The thread changed while its previous actor was closing.",
                    false,
                )),
            );
            let detach: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(
                detach.request,
                ClientRequest::ChatDetach { chat_id: refused }
            );
            send(
                &mut server_stream,
                None,
                activity(refused, ActivityState::Idle),
            );
            respond(
                &mut server_stream,
                detach.request_id,
                ServerResponse::Detached,
            );

            // The connection stays in step for the next request, and a chat it
            // never attached cannot answer for it.
            let list: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(list.request, ClientRequest::ChatList);
            respond(
                &mut server_stream,
                list.request_id,
                ServerResponse::ChatListed {
                    sessions: Vec::new(),
                },
            );
            let list: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            send(
                &mut server_stream,
                None,
                activity(elsewhere, ActivityState::Idle),
            );
            respond(
                &mut server_stream,
                list.request_id,
                ServerResponse::ChatListed {
                    sessions: Vec::new(),
                },
            );
        });

        let mut client = ControlClient::from_stream(client_stream).unwrap();
        client.close_chat(session(closed)).unwrap();
        assert!(matches!(
            client.close_chat(session(refused)),
            Err(ControlError::Remote {
                code: ProtocolErrorCode::TerminalUnavailable,
                ..
            })
        ));
        assert!(client.attached_chats.is_empty());
        assert!(client.list_chats().unwrap().is_empty());
        assert!(matches!(
            client.list_chats(),
            Err(ControlError::UnexpectedMessage {
                actual: "chat_activity",
                ..
            })
        ));
        server.join().unwrap();
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
