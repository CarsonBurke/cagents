use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::{OsStr, OsString},
    io::{self, IsTerminal, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{ChildStdout, Command, ExitCode, Stdio},
    thread,
    time::{Duration, Instant},
};

use clap::{Parser, Subcommand, error::ErrorKind as ClapErrorKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

mod accounts;
pub use accounts::AccountCommand;
#[cfg(test)]
mod agent_start_authority_tests;
mod agents;
mod caller_context;
pub(crate) use caller_context::{CallerContext, NativeReadiness};

#[cfg(test)]
mod automation_work_authority_tests;
mod channel_work;
mod channels;
mod chat;
#[cfg(test)]
mod project_channel_authority_tests;
pub use chat::ChatCommand;
mod format;
pub use agents::AgentCommand;
mod automations;
mod completions;
pub use automations::AutomationCommand;
mod sessions;
mod terminal_list;
#[cfg(test)]
mod terminal_start_authority_tests;
pub use sessions::SessionsArgs;
mod messages;
pub use messages::{Arrival, AskArgs, InboxArgs, ReplyArgs, SendArgs};
mod linear;
pub use linear::LinearCommand;
mod work;
pub use work::WorkCommand;
#[cfg(feature = "ui")]
pub(crate) mod invocation;

use crate::{
    agent_execution::Authority,
    instance::{
        self, UI_LAUNCH_ERROR_PREFIX, UI_LAUNCH_HANDSHAKE_ENV, UI_LAUNCH_READY,
        UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX, UI_LAUNCH_RECOVERY_PROGRESS_PREFIX,
    },
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
// Close is acknowledged only after interruption and process teardown. Allow
// those grace periods plus time to schedule the worker and deliver its reply.
const CHAT_CLOSE_TIMEOUT: Duration = crate::chat::INTERRUPT_TIMEOUT
    .saturating_add(crate::chat::TERMINATE_GRACE)
    .saturating_add(Duration::from_secs(8));
// Retained-owner cleanup can retire both the native holder and its paired PTY.
const RETAINED_CHAT_CLOSE_TIMEOUT: Duration = Duration::from_secs(75);
const SNAPSHOT_RETRY_INTERVAL: Duration = Duration::from_millis(25);
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const UI_LAUNCH_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `cagents ui` waits for a window that has closed, and so refuses
/// activations, to finish exiting and release its lease. An exiting UI waits
/// for its language servers (bounded at 5s) and its window's removal (2s),
/// then flushes its session.
const UI_PREDECESSOR_EXIT_TIMEOUT: Duration = Duration::from_secs(10);
const UI_PREDECESSOR_EXIT_POLL: Duration = Duration::from_millis(200);
const MAX_UI_LAUNCH_HANDSHAKE_BYTES: usize = 4096;
const MAX_WAIT_SECONDS: f64 = 24.0 * 60.0 * 60.0;

#[derive(Debug, Parser)]
#[command(
    name = "cagents",
    version = crate::VERSION,
    about = "Manage cagents agents, sessions, automations, projects, channels, terminals, messaging, Linear and accounts"
)]
pub struct Cli {
    /// Backend device from devices.json; local is the default.
    #[arg(long, global = true)]
    pub device: Option<String>,
    /// Emit stable JSON envelopes for automation.
    #[arg(long, global = true)]
    pub json: bool,
    /// Connect to an existing daemon without starting one (also CAGENTS_NO_START=1).
    #[arg(long, global = true)]
    pub no_start: bool,

    #[command(subcommand)]
    pub command: TopCommand,
}

#[derive(Debug, Subcommand)]
pub enum TopCommand {
    /// Generate a shell completion script without contacting services.
    Completions {
        /// Shell whose completion script should be generated.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Read bounded private diagnostics without starting services.
    Diagnostics,
    /// Read and update Linear tickets, or link a ticket to this Git worktree.
    Linear(LinearCommand),
    /// Show this session, its parent and agents, and peers in the same checkout.
    Whoami,
    /// Inspect account plan allowances and the calling session’s context.
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// List live agent sessions in this project, whoever started them.
    #[command(visible_alias = "ls")]
    Sessions(SessionsArgs),
    /// Inspect native resident capacity and explicitly release completed handles.
    Chat {
        #[command(subcommand)]
        command: ChatCommand,
    },
    /// Message another session; a live conversation receives it once idle.
    Send(SendArgs),
    /// Message another session and wait for its reply.
    Ask(AskArgs),
    /// Answer a message this session received.
    Reply(ReplyArgs),
    /// Read the messages left for this session.
    Inbox(InboxArgs),
    /// Show every worktree and branch in progress, newest activity first.
    Work {
        #[command(subcommand)]
        command: WorkCommand,
    },
    /// Manage recurring daemon-owned work and its durable run history.
    #[command(alias = "automations")]
    Automation {
        #[command(subcommand)]
        command: AutomationCommand,
    },
    /// Start, message, inspect, wait for, and close owned provider agents.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Launch the native cagents UI and return immediately.
    Ui {
        /// Open another durable window slot instead of activating a running UI.
        #[arg(long, conflicts_with = "window")]
        new_window: bool,
        /// Reopen or activate a named window slot.
        #[arg(long, value_parser = parse_window_slot)]
        window: Option<String>,
        /// Copy initial navigation from this same-device client window.
        #[arg(long, value_parser = parse_window_slot, requires = "new_window")]
        source: Option<String>,
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
    UpdateWorker {
        #[arg(help = "Detached update job ID.")]
        job_id: String,
    },
    /// Cooperatively fence application windows around an external installer.
    #[command(hide = true)]
    CoordinateInstall {
        #[arg(long)]
        #[arg(help = "Installation destination prefix.")]
        prefix: PathBuf,
        #[arg(long)]
        #[arg(help = "Installer executable to run.")]
        installer: PathBuf,
        /// Publish files while preserving running windows and the daemon.
        #[arg(long)]
        stage_only: bool,
        #[arg(last = true)]
        #[arg(help = "Arguments passed to the installer after --.")]
        arguments: Vec<OsString>,
    },
    /// Validate a launch fence inherited from the update coordinator.
    #[command(hide = true)]
    VerifyInstallGuard {
        #[arg(long)]
        #[arg(help = "Stage an installation while retaining running windows.")]
        stage_only: bool,
    },
    /// Record that staged files must retain windows on the running daemon's UI.
    #[command(hide = true)]
    RecordDeferredInstall {
        #[arg(long, requires_all = ["prefix", "version_root"])]
        #[arg(help = "Stage an installation while retaining running windows.")]
        stage_only: bool,
        #[arg(long, requires = "stage_only")]
        #[arg(help = "Installation destination prefix.")]
        prefix: Option<PathBuf>,
        #[arg(long, requires = "stage_only")]
        #[arg(help = "Directory containing the staged executable version.")]
        version_root: Option<PathBuf>,
    },
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
    /// Forward a framed daemon connection over stdin/stdout (used by SSH).
    #[command(hide = true)]
    Bridge,
    /// Start the daemon if it is not already running.
    Start,
    /// Report whether the daemon is running.
    Status,
    /// Finish a staged installation through the same reviewed update flow as the app.
    Upgrade {
        /// Approve the displayed impact without an interactive prompt.
        #[arg(long)]
        yes: bool,
        /// Permit active native turns and their background subagents to stop.
        #[arg(long)]
        interrupt: bool,
    },
    /// Request daemon termination (use --confirm for a counted confirmation).
    Stop {
        #[arg(long, conflicts_with = "confirm")]
        #[arg(help = "Refuse shutdown while hosted sessions remain.")]
        require_empty: bool,
        #[arg(long, conflicts_with = "confirm")]
        #[arg(help = "Refuse shutdown if the observed session inventory generation changed.")]
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
        /// Restart hosted sessions, stopping their turns and background subagents.
        #[arg(long)]
        yes: bool,
        /// Relaunch identified agents without an exact resume ID into their resume picker.
        /// Without this or --no-relaunch-agents, Settings decides; it defaults to on.
        #[arg(long, conflicts_with = "no_relaunch_agents")]
        relaunch_agents: bool,
        /// Refuse the upgrade while identified agents have no exact resume ID.
        #[arg(long)]
        no_relaunch_agents: bool,
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
    StopForRebuild {
        /// Confirm that every development job, including background subagents, stops.
        #[arg(long)]
        including_subagents: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum TerminalCommand {
    /// List running terminals with their observed state and title.
    List {
        /// Include retained terminals that are not running.
        #[arg(long, conflicts_with = "exited")]
        all: bool,
        /// Show only terminals with a recorded process exit.
        #[arg(long)]
        exited: bool,
    },
    /// Read one terminal's full descriptor, including launch and recovery details.
    Show {
        /// The daemon terminal ID.
        terminal_id: TerminalId,
    },
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
        #[arg(help = "The daemon terminal ID.")]
        terminal_id: TerminalId,
        #[arg(help = "UTF-8 text to write.")]
        text: String,
    },
    /// Explicitly terminate a terminal.
    Close {
        /// The daemon terminal ID.
        terminal_id: TerminalId,
    },
    /// Read a current immutable terminal snapshot.
    Snapshot {
        /// The daemon terminal ID.
        terminal_id: TerminalId,
    },
    /// Wait until a current terminal snapshot contains text.
    Wait {
        #[arg(help = "The daemon terminal ID.")]
        terminal_id: TerminalId,
        #[arg(long)]
        #[arg(help = "Text that must appear in the terminal snapshot.")]
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
        /// A folder or path inside a Git repository (defaults to the current directory).
        path: Option<PathBuf>,
    },
    /// List shared projects, newest first.
    List,
    /// Close a project and every channel in it.
    Close {
        /// A folder or path inside a Git repository (defaults to the current directory).
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
    /// Inspect and declare the work this channel is progressing.
    Work {
        #[command(subcommand)]
        command: channel_work::WorkCommand,
    },
    /// Move a live pane, hosted session or agent into another channel.
    Move {
        /// Full pane/session ID, agent ID or unique prefix, or unique agent name.
        target: String,
        /// Destination channel in the same project.
        #[arg(long)]
        to: ChannelId,
    },
    /// Create a channel in a project, with or without a window running.
    Spawn {
        /// Folder or Git repository the channel belongs to (defaults to the
        /// current directory).
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
    Close {
        /// The shared channel ID.
        channel_id: ChannelId,
    },
}

/// A `--project` value: a path, made absolute, or the name of an open
/// project, which the daemon looks up. A single word that is no directory
/// here is a name.
fn project_argument(value: PathBuf) -> Result<PathBuf, AppError> {
    let mut components = value.components();
    let word = matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    );
    if word && !value.exists() {
        return Ok(value);
    }
    std::path::absolute(value).map_err(|e| AppError::new("target", e))
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
    #[error("the daemon did not answer within {} s", timeout.as_secs_f64())]
    ReplyTimeout {
        timeout: Duration,
        #[source]
        error: FrameError,
    },
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
    #[error("the daemon is dropping the connection ({code:?}): {message}")]
    ConnectionRejected {
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

/// Ends a [`ControlClient`]'s connection from another thread.
pub struct Hangup(UnixStream);

impl Hangup {
    pub fn hang_up(&self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
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
    observed_notifications: VecDeque<crate::notifications::Update>,
    /// The newest Work board pushed while a correlated response was pending.
    /// Each carries the whole board, so only the newest matters.
    observed_work: Option<(u64, Box<crate::work::Board>)>,
    /// Native chats this connection is attached to, whose change and activity
    /// notifications the daemon interleaves with responses.
    ///
    /// Attaching is only how this client earns the right to close a chat; it
    /// reads nothing from the chat's notifications, so they are dropped.
    attached_chats: BTreeSet<TerminalId>,
    /// Terminal streams this connection successfully requested. Their payloads
    /// are not retained, but queued notifications can outlive a Detach reply,
    /// so remember their IDs for this connection's lifetime.
    terminal_streams: BTreeSet<TerminalId>,
}

/// Linux AF_UNIX backlog pressure returns EAGAIN without connecting. Retry
/// fresh nonblocking sockets within the same establishment deadline, rather
/// than treating writability/SO_ERROR as evidence that EAGAIN completed.
pub(crate) fn connect_control_socket(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid daemon Unix socket path",
        ));
    }
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as _;
    }
    let deadline = Instant::now() + timeout;
    let mut delay = Duration::from_millis(1);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon socket connection deadline elapsed",
            ));
        }
        // SAFETY: socket returns a new descriptor transferred to this stream.
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        // SAFETY: address is initialized with its valid length and the
        // descriptor belongs exclusively to this connection attempt.
        let result = unsafe {
            libc::connect(
                fd,
                (&address as *const libc::sockaddr_un).cast(),
                (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as _,
            )
        };
        if result == 0 {
            stream.set_nonblocking(false)?;
            return Ok(stream);
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ) {
            return Err(error);
        }
        // EAGAIN has not queued a connection. Close this attempt before
        // retrying, preserving bounded descriptor use and the selected endpoint.
        drop(stream);
        thread::sleep(delay.min(deadline.saturating_duration_since(Instant::now())));
        delay = (delay * 2).min(Duration::from_millis(20));
    }
}

impl ControlClient {
    /// Connect and complete a proof-pinning handshake with no expected proof.
    pub fn connect(socket_path: &Path) -> Result<Self, ControlError> {
        #[cfg(feature = "ui")]
        let health_request = crate::daemon_health::begin();
        let deadline = Instant::now() + Self::handshake_timeout();
        let result = (|| {
            let stream = connect_control_socket(
                socket_path,
                IO_TIMEOUT.min(deadline.saturating_duration_since(Instant::now())),
            )
            .map_err(ControlError::Connect)?;
            Self::from_stream_inner(stream, deadline, deadline.min(Instant::now() + IO_TIMEOUT))
        })();
        #[cfg(feature = "ui")]
        crate::daemon_health::connection(socket_path, &result, health_request);
        result
    }

    pub(crate) fn from_stream(stream: UnixStream) -> Result<Self, ControlError> {
        let deadline = Instant::now() + Self::handshake_timeout();
        Self::from_stream_inner(stream, deadline, deadline.min(Instant::now() + IO_TIMEOUT))
    }

    fn handshake_timeout() -> Duration {
        if crate::device::is_remote() {
            Duration::from_secs(30)
        } else {
            IO_TIMEOUT
        }
    }

    pub(crate) fn from_stream_with_deadline(
        stream: UnixStream,
        deadline: Instant,
    ) -> Result<Self, ControlError> {
        Self::from_stream_inner(stream, deadline, deadline)
    }

    fn from_stream_inner(
        stream: UnixStream,
        deadline: Instant,
        write_deadline: Instant,
    ) -> Result<Self, ControlError> {
        stream
            .set_read_timeout(Some(Self::handshake_timeout()))
            .map_err(ControlError::Configure)?;
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .map_err(ControlError::Configure)?;
        let mut client = HandshakeClient {
            stream,
            next_request_id: 1,
        };
        let proof = client.handshake(deadline, write_deadline)?;
        client
            .stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .map_err(ControlError::Configure)?;
        client
            .stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .map_err(ControlError::Configure)?;
        Ok(Self {
            stream: client.stream,
            proof,
            next_request_id: client.next_request_id,
            observed: VecDeque::new(),
            observed_model: VecDeque::new(),
            observed_notifications: VecDeque::new(),
            observed_work: None,
            attached_chats: BTreeSet::new(),
            terminal_streams: BTreeSet::new(),
        })
    }

    pub fn proof(&self) -> &HostSessionProof {
        &self.proof
    }

    /// Release a native-to-terminal replay receipt after its replacement is durable.
    #[cfg(any(feature = "ui", all(test, feature = "host")))]
    pub(crate) fn commit_terminal_handoff(
        &mut self,
        terminal_id: TerminalId,
    ) -> Result<(), ControlError> {
        match self.request_strict(ClientRequest::ChatCommitTerminal { terminal_id })? {
            ServerResponse::Accepted => Ok(()),
            response => Err(unexpected_response(
                "a terminal handoff acknowledgement",
                &response,
            )),
        }
    }

    /// Acts for the agent session `authority` was issued to, which may then
    /// change only the terminals it controls. Without a token the connection
    /// keeps acting for the user.
    pub fn authenticate(&mut self, authority: Authority) -> Result<(), ControlError> {
        match self.request_strict(ClientRequest::Authenticate { authority })? {
            ServerResponse::Accepted => Ok(()),
            response => Err(unexpected_response("authentication", &response)),
        }
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

    /// A handle that ends this connection from another thread, so a read
    /// blocked in one of the `next_*` calls returns a closed connection.
    pub fn hangup(&self) -> Result<Hangup, ControlError> {
        self.stream
            .try_clone()
            .map(Hangup)
            .map_err(ControlError::Configure)
    }

    /// Subscribes to shared-model edges and returns the model in one round trip.
    ///
    /// Unlike `watch_terminals` this needs no separate read: the daemon answers
    /// the subscription with the model itself, so there is no gap between
    /// subscribing and reading for an edit to fall into.
    pub fn watch_workspace_model(&mut self) -> Result<WorkspaceModel, ControlError> {
        self.expect_model(ClientRequest::WatchWorkspaceModel)
    }

    pub fn watch_notifications(
        &mut self,
        presence: crate::notifications::Presence,
    ) -> Result<crate::notifications::Snapshot, ControlError> {
        self.notification_snapshot_request(ClientRequest::WatchNotifications { presence })
    }

    pub fn read_notifications(&mut self) -> Result<crate::notifications::Snapshot, ControlError> {
        self.notification_snapshot_request(ClientRequest::ReadNotifications)
    }

    fn notification_snapshot_request(
        &mut self,
        request: ClientRequest,
    ) -> Result<crate::notifications::Snapshot, ControlError> {
        self.stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .map_err(ControlError::Configure)?;
        match self.request_strict(request)? {
            ServerResponse::Notifications { snapshot } => Ok(snapshot),
            response => Err(unexpected_response("agent attention", &response)),
        }
    }

    fn accepted(&mut self, request: ClientRequest) -> Result<(), ControlError> {
        match self.request_strict(request)? {
            ServerResponse::Accepted => Ok(()),
            response => Err(unexpected_response("request acceptance", &response)),
        }
    }

    pub fn observe_review_requests(
        &mut self,
        observed: crate::notifications::review::ObservedRequests,
    ) -> Result<(), ControlError> {
        self.accepted(ClientRequest::ObserveReviewRequests { observed })
    }

    pub fn notification_presence(
        &mut self,
        presence: crate::notifications::Presence,
    ) -> Result<(), ControlError> {
        self.accepted(ClientRequest::NotificationPresence { presence })
    }

    pub fn notification_preferences(
        &mut self,
        preferences: Option<crate::notifications::Preferences>,
    ) -> Result<(), ControlError> {
        self.accepted(ClientRequest::NotificationPreferencesChanged { preferences })
    }

    /// Asks the daemon to show a test notification and waits for the
    /// desktop's answer.
    pub fn send_test_notification(
        &mut self,
    ) -> Result<crate::notifications::TestOutcome, ControlError> {
        match self.request_with_deadline(
            ClientRequest::SendTestNotification,
            Instant::now() + crate::notifications::TEST_TIMEOUT + IO_TIMEOUT,
        )? {
            ServerResponse::NotificationTest { outcome } => Ok(outcome),
            response => Err(unexpected_response(
                "a test notification outcome",
                &response,
            )),
        }
    }

    pub fn acknowledge_notification(&mut self, id: uuid::Uuid) -> Result<(), ControlError> {
        self.accepted(ClientRequest::AcknowledgeNotification { id })
    }

    pub fn next_notification(&mut self) -> Result<crate::notifications::Update, ControlError> {
        if let Some(update) = self.observed_notifications.pop_front() {
            return Ok(update);
        }
        self.stream
            .set_read_timeout(None)
            .map_err(ControlError::Configure)?;
        loop {
            let envelope = self.read_envelope()?;
            self.validate_pinned_envelope(&envelope)?;
            match envelope.message {
                ServerMessage::NotificationsChanged => {
                    return Ok(crate::notifications::Update::Changed);
                }
                ServerMessage::NotificationActivated { target } => {
                    return Ok(crate::notifications::Update::Activated(target));
                }
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
    /// Its response deadline covers the provider's interruption and process
    /// termination grace periods; success still requires confirmed teardown.
    ///
    /// Attached, this connection is sent the chat's notifications until the
    /// close removes it - the interrupt the close delivers is itself reported
    /// as activity - so they are expected between the requests' responses.
    pub fn close_chat(&mut self, session: crate::chat::Session) -> Result<(), ControlError> {
        let chat_id = session.id;
        // The daemon subscribes this connection before it answers the
        // attachment, so the chat's notifications can arrive ahead of that
        // answer.
        self.attached_chats.insert(chat_id);
        let closed = match self.request_strict(ClientRequest::ChatAttach { session }) {
            Ok(ServerResponse::ChatAttached { .. }) => self
                .request_with_deadline(
                    ClientRequest::ChatClose { chat_id },
                    Instant::now() + CHAT_CLOSE_TIMEOUT,
                )
                .and_then(|response| match response {
                    ServerResponse::ChatClosed { .. } => Ok(()),
                    response => Err(unexpected_response("a native chat close", &response)),
                }),
            Ok(response) => Err(unexpected_response("a native chat attachment", &response)),
            // The daemon refuses an attachment before it subscribes anyone.
            Err(error @ ControlError::Remote { .. }) => {
                self.attached_chats.remove(&chat_id);
                return Err(error);
            }
            Err(error) => return Err(error),
        };
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

    /// Proves teardown without reopening provider history or creating an actor.
    pub fn close_retained_chat(
        &mut self,
        session: crate::chat::Session,
    ) -> Result<(), ControlError> {
        match self.request_with_deadline(
            ClientRequest::ChatCloseRetained { session },
            Instant::now() + RETAINED_CHAT_CLOSE_TIMEOUT,
        )? {
            ServerResponse::Accepted => Ok(()),
            response => Err(unexpected_response(
                "retained native owner teardown",
                &response,
            )),
        }
    }

    pub fn prepare_chat_resume(
        &mut self,
        source: crate::chat::Session,
    ) -> Result<crate::chat::Session, ControlError> {
        match self.request_strict(ClientRequest::ChatPrepareResume { source })? {
            ServerResponse::ChatResumePrepared { session } => Ok(session),
            response => Err(unexpected_response(
                "a prepared conversation resume",
                &response,
            )),
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
        self.terminal_streams.insert(expected);
        self.read_attachment_replay(expected)?;
        Ok(terminal)
    }

    pub fn complete_pending_terminal(
        &mut self,
        pane: crate::model::CardId,
        terminal_id: TerminalId,
        options: TerminalSpawnOptions,
    ) -> Result<TerminalDescriptor, ControlError> {
        let response = self.request_attached(
            ClientRequest::CompletePendingTerminal {
                pane,
                terminal_id,
                options,
            },
            terminal_id,
        )?;
        let terminal = match response {
            ServerResponse::Spawned { terminal } if terminal.session.id == terminal_id => terminal,
            ServerResponse::Spawned { terminal } => {
                return Err(ControlError::TerminalMismatch {
                    expected: terminal_id,
                    actual: terminal.session.id,
                });
            }
            response => return Err(unexpected_response("a reserved terminal", &response)),
        };
        self.terminal_streams.insert(terminal_id);
        self.read_attachment_replay(terminal_id)?;
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
        match action {
            Ok(()) => self.finish_closed_attachment(terminal_id, ()),
            Err(error) => self.finish_attachment(terminal_id, Err(error)),
        }
    }

    pub fn release_completed_terminal(
        &mut self,
        session: crate::model::TerminalSession,
        allow_active: bool,
    ) -> Result<terminal::ipc::TerminalCloseResult, ControlError> {
        let id = session.id;
        // A completed PTY can still own a native provider actor. Its explicit
        // close must allow the same bounded teardown as closing the chat.
        match self.request_with_deadline(
            ClientRequest::ReleaseCompletedTerminal {
                session,
                allow_active,
            },
            Instant::now() + CHAT_CLOSE_TIMEOUT,
        )? {
            ServerResponse::TerminalCloseChecked {
                terminal_id,
                outcome,
            } if terminal_id == id => Ok(outcome),
            response => Err(unexpected_response("completed terminal release", &response)),
        }
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
        match action {
            Ok(terminal::ipc::TerminalCloseResult::Closed) => self
                .finish_closed_attachment(terminal_id, terminal::ipc::TerminalCloseResult::Closed),
            result => self.finish_attachment(terminal_id, result),
        }
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

    pub fn upgrade_impact(
        &mut self,
        policy: crate::update::impact::Policy,
    ) -> Result<crate::update::impact::DaemonImpact, ControlError> {
        self.upgrade_impact_with_deadline(policy, Instant::now() + Duration::from_secs(30))
    }

    /// Preview may scan a large owned inventory. Its complete operation has a
    /// separate budget; individual reads and unsolicited traffic cannot renew it.
    pub(crate) fn upgrade_impact_with_deadline(
        &mut self,
        policy: crate::update::impact::Policy,
        deadline: Instant,
    ) -> Result<crate::update::impact::DaemonImpact, ControlError> {
        match self.request_with_deadline(ClientRequest::UpgradeImpact { policy }, deadline)? {
            ServerResponse::UpgradeImpact(impact) => Ok(impact),
            response => Err(unexpected_response("update impact", &response)),
        }
    }

    pub fn prepare_confirmed_upgrade(
        &mut self,
        impact: crate::update::impact::DaemonImpact,
        relaunch_agents: bool,
    ) -> Result<(u64, u64), ControlError> {
        match self.request_with_deadline(
            ClientRequest::PrepareConfirmedUpgrade {
                impact,
                relaunch_agents,
            },
            Instant::now() + Duration::from_secs(60),
        )? {
            ServerResponse::UpgradePrepared { terminals, chats } => Ok((terminals, chats)),
            response => Err(unexpected_response(
                "confirmed upgrade checkpoint",
                &response,
            )),
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
        self.terminal_streams.insert(terminal_id);
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

    /// Teardown is already confirmed by the pinned, correlated close reply.
    /// Ordinary exit may finish this connection before the trailing Detach.
    /// Other detach failures remain errors, and unconfirmed closes use the
    /// ordinary attachment path above.
    fn finish_closed_attachment<T>(
        &mut self,
        terminal_id: TerminalId,
        value: T,
    ) -> Result<T, ControlError> {
        match self.detach(terminal_id) {
            Ok(())
            | Err(ControlError::ConnectionClosed)
            | Err(ControlError::UnexpectedMessage {
                actual: "host_shutting_down",
                ..
            }) => Ok(value),
            Err(ControlError::Frame(FrameError::Io(error)))
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::NotConnected
                ) =>
            {
                Ok(value)
            }
            Err(error) => Err(error),
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

    pub fn device_operation(
        &mut self,
        operation: crate::device_service::Operation,
    ) -> Result<crate::device_service::Reply, ControlError> {
        match self.request_with_deadline(
            ClientRequest::DeviceOperation { operation },
            Instant::now() + Duration::from_secs(120),
        )? {
            ServerResponse::DeviceOperation { reply } => Ok(reply),
            response => Err(unexpected_response("device operation", &response)),
        }
    }

    /// Exercises the public control wire in host integration fixtures.
    #[cfg(all(test, feature = "host"))]
    pub(crate) fn fixture_request(
        &mut self,
        request: ClientRequest,
    ) -> Result<ServerResponse, ControlError> {
        self.request_strict(request)
    }

    pub(crate) fn channel_work(
        &mut self,
        authority: Authority,
        request: crate::channel_work_api::Request,
        timeout: Duration,
    ) -> Result<crate::channel_work_api::Response, ControlError> {
        match self.request_with_deadline(
            ClientRequest::ChannelWork { authority, request },
            Instant::now() + timeout,
        )? {
            ServerResponse::ChannelWork(response) => Ok(response),
            response => Err(unexpected_response("channel_work", &response)),
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
        self.request_inner(request, streaming_terminal, None)
    }

    fn request_with_deadline(
        &mut self,
        request: ClientRequest,
        deadline: Instant,
    ) -> Result<ServerResponse, ControlError> {
        let result = self.request_inner(request, None, Some(deadline));
        let read_restored = self.restore_read_timeout();
        let write_restored = self
            .stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .map_err(ControlError::Configure);
        // Restore both directions even when the request or either restoration fails.
        result.and_then(|response| read_restored.and(write_restored).map(|()| response))
    }

    fn request_inner(
        &mut self,
        request: ClientRequest,
        streaming_terminal: Option<TerminalId>,
        deadline: Option<Instant>,
    ) -> Result<ServerResponse, ControlError> {
        #[cfg(feature = "ui")]
        let health_request = crate::daemon_health::begin();
        let result = self.request_inner_unobserved(request, streaming_terminal, deadline);
        #[cfg(feature = "ui")]
        crate::daemon_health::request(&self.stream, &result, health_request);
        result
    }

    fn request_inner_unobserved(
        &mut self,
        request: ClientRequest,
        streaming_terminal: Option<TerminalId>,
        deadline: Option<Instant>,
    ) -> Result<ServerResponse, ControlError> {
        let reply_timeout = deadline
            .map(|end| end.saturating_duration_since(Instant::now()))
            .unwrap_or(IO_TIMEOUT);
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        if self.next_request_id == 0 {
            self.next_request_id = 1;
        }
        let envelope = ClientEnvelope {
            version: PROTOCOL_VERSION,
            request_id,
            proof: Some(self.proof.clone()),
            request,
        };
        if let Some(deadline) = deadline {
            write_frame(
                &mut DeadlineWriter {
                    stream: &mut self.stream,
                    deadline,
                },
                &envelope,
            )?;
        } else {
            write_frame(&mut self.stream, &envelope)?;
        }

        loop {
            let envelope = match deadline {
                Some(deadline) => read_frame(&mut DeadlineReader {
                    stream: &mut self.stream,
                    deadline,
                })
                .map_err(|error| reply_error(error, Some(reply_timeout)))?
                .ok_or(ControlError::ConnectionClosed)?,
                None => self.read_envelope()?,
            };
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
                    // The daemon's notice that it is dropping this connection,
                    // such as an overflowing delivery queue, says why.
                    Err(ServerMessage::Error(error)) => {
                        return Err(ControlError::ConnectionRejected {
                            code: error.code,
                            message: error.message,
                            retryable: error.retryable,
                        });
                    }
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
            ServerMessage::NotificationsChanged => {
                if !self
                    .observed_notifications
                    .contains(&crate::notifications::Update::Changed)
                {
                    self.observed_notifications
                        .push_back(crate::notifications::Update::Changed);
                }
            }
            ServerMessage::NotificationActivated { target } => {
                if self.observed_notifications.len() < 32 {
                    self.observed_notifications
                        .push_back(crate::notifications::Update::Activated(target));
                }
            }
            ServerMessage::WorkBoard { revision, board } => {
                if self
                    .observed_work
                    .as_ref()
                    .is_none_or(|(observed, _)| *observed < revision)
                {
                    self.observed_work = Some((revision, board));
                }
            }
            ServerMessage::ChatChanged { chat_id }
            | ServerMessage::ChatObserved { chat_id, .. }
            | ServerMessage::ChatActivity { chat_id, .. }
                if self.attached_chats.contains(&chat_id) => {}
            ServerMessage::Redraw { terminal_id, .. }
            | ServerMessage::Snapshot { terminal_id, .. }
            | ServerMessage::TerminalObserved { terminal_id, .. }
            | ServerMessage::Event { terminal_id, .. }
            | ServerMessage::Synchronized { terminal_id, .. }
                if self.terminal_streams.contains(&terminal_id) => {}
            message => return Err(message),
        }
        Ok(())
    }

    fn read_envelope(&mut self) -> Result<ServerEnvelope, ControlError> {
        let timeout = self.stream.read_timeout().ok().flatten();
        read_frame(&mut self.stream)
            .map_err(|error| reply_error(error, timeout))?
            .ok_or(ControlError::ConnectionClosed)
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

/// Bound the complete response, including unsolicited notifications and every
/// partial frame read. A socket timeout alone restarts on each successful read.
struct DeadlineReader<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon response deadline expired",
            ));
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buffer)
    }
}

/// Bound every partial write of a request against the same overall deadline.
struct DeadlineWriter<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}

impl Write for DeadlineWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon request deadline expired",
            ));
        }
        self.stream.set_write_timeout(Some(remaining))?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

struct HandshakeClient {
    stream: UnixStream,
    next_request_id: u64,
}

impl HandshakeClient {
    fn handshake(
        &mut self,
        deadline: Instant,
        write_deadline: Instant,
    ) -> Result<HostSessionProof, ControlError> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        let request = ClientEnvelope {
            version: PROTOCOL_VERSION,
            request_id,
            proof: None,
            request: ClientRequest::Connect {
                expected_host: None,
                resume: Vec::new(),
            },
        };
        // Defaults keep the original five-second write budget, including when
        // remote handshakes allow longer reads. Explicit callers retain their
        // own complete operation deadline for both directions.
        write_frame(
            &mut DeadlineWriter {
                stream: &mut self.stream,
                deadline: write_deadline,
            },
            &request,
        )?;
        let timeout = deadline.saturating_duration_since(Instant::now());
        let envelope: ServerEnvelope = read_frame(&mut DeadlineReader {
            stream: &mut self.stream,
            deadline,
        })
        .map_err(|error| reply_error(error, Some(timeout)))?
        .ok_or(ControlError::ConnectionClosed)?;
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
fn reply_error(error: FrameError, timeout: Option<Duration>) -> ControlError {
    if let Some(timeout) = timeout
        && matches!(&error, FrameError::Io(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock))
    {
        ControlError::ReplyTimeout { timeout, error }
    } else {
        ControlError::Frame(error)
    }
}
fn is_timeout_error(error: &ControlError) -> bool {
    matches!(error, ControlError::ReplyTimeout { .. })
        || matches!(
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
        | ServerMessage::NotificationsChanged
        | ServerMessage::NotificationActivated { .. }
        | ServerMessage::WorkBoard { .. }
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
        ServerMessage::NotificationsChanged => "notifications_changed",
        ServerMessage::NotificationActivated { .. } => "notification_activated",
        ServerMessage::WorkBoard { .. } => "work_board",
    }
}

fn response_name(response: &ServerResponse) -> &'static str {
    match response {
        ServerResponse::Automation(_) => "automation",
        ServerResponse::Agent(_) => "agent",
        ServerResponse::Notifications { .. } => "notifications",
        ServerResponse::NotificationTest { .. } => "notification_test",
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
        ServerResponse::UpgradeImpact(_) => "upgrade_impact",
        ServerResponse::ShutdownOutcome(_) => "shutdown_outcome",
        ServerResponse::ChatAttached { .. } => "chat_attached",
        ServerResponse::ChatSnapshot { .. } => "chat_snapshot",
        ServerResponse::ChatListed { .. } => "chat_listed",
        ServerResponse::ChatCapacity { .. } => "chat_capacity",
        ServerResponse::ChatReleaseInspected { .. } => "chat_release_inspected",
        ServerResponse::ChatReleased { .. } => "chat_released",
        ServerResponse::ChatClosed { .. } => "chat_closed",
        ServerResponse::ChatTerminal { .. } => "chat_terminal",
        ServerResponse::ChatPreviewed { .. } => "chat_previewed",
        ServerResponse::ChatCandidates { .. } => "chat_candidates",
        ServerResponse::TerminalCloseChecked { .. } => "terminal_close_checked",
        ServerResponse::AgentActivity { .. } => "agent_activity",
        ServerResponse::UpgradePrepared { .. } => "upgrade_prepared",
        ServerResponse::UpgradeChatResolved { .. } => "upgrade_chat_resolved",
        ServerResponse::WorkspaceModel { .. } => "workspace_model",
        ServerResponse::ChatResumePrepared { .. } => "chat_resume_prepared",
        ServerResponse::ChatUpdate { .. } => "chat_update",
        ServerResponse::ChatFileChanges { .. } => "chat_file_changes",
        ServerResponse::ChatFileChangePatches { .. } => "chat_file_change_patches",
        ServerResponse::ChatTextCopy { .. } => "chat_text_copy",
        ServerResponse::Work(_) => "work",
        ServerResponse::ChannelWork(_) => "channel_work",
        ServerResponse::DeviceOperation { .. } => "device_operation",
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

/// Scriptable outcomes shared by commands that wait for work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WaitOutcome {
    Succeeded,
    Pending,
    TimedOut,
    Failed,
    Waiting,
}

impl WaitOutcome {
    fn exit_code(self) -> u8 {
        match self {
            Self::Succeeded => 0,
            Self::Pending | Self::TimedOut => 3,
            Self::Failed => 4,
            Self::Waiting => 5,
        }
    }
}

impl CommandOutput {
    fn exit_code(&self) -> ExitCode {
        let code = match self.data.get("outcome").and_then(Value::as_str) {
            Some("pending" | "timed_out") => WaitOutcome::TimedOut.exit_code(),
            Some("failed") => WaitOutcome::Failed.exit_code(),
            Some("waiting") => WaitOutcome::Waiting.exit_code(),
            _ => WaitOutcome::Succeeded.exit_code(),
        };
        ExitCode::from(code)
    }
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

static NO_START: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn daemon_autostart_disabled() -> bool {
    NO_START.load(std::sync::atomic::Ordering::Relaxed)
        || std::env::var("CAGENTS_NO_START")
            .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

struct StartPolicy(bool);
impl Drop for StartPolicy {
    fn drop(&mut self) {
        NO_START.store(self.0, std::sync::atomic::Ordering::Relaxed);
    }
}

fn control_daemon_socket() -> Result<PathBuf, AppError> {
    if !daemon_autostart_disabled() {
        return terminal::ensure_daemon_ready().map_err(|e| AppError::new("daemon_start", e));
    }
    let socket = daemon_socket_path().map_err(|e| AppError::new("socket_path", e))?;
    match UnixStream::connect(&socket) {
        Ok(_) => Ok(socket),
        Err(error) if daemon_missing(&error) => {
            confirm_daemon_absent(&socket)?;
            Err(AppError::new(
                "daemon_not_running",
                "The daemon is not running; omit --no-start to start it",
            ))
        }
        Err(error) => Err(AppError::new("daemon_connection", error)),
    }
}

fn daemon_missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

fn confirm_daemon_absent(socket: &Path) -> Result<(), AppError> {
    match crate::daemon::HostLease::is_held_at(socket) {
        Ok(false) => Ok(()),
        Ok(true) => Err(AppError::new(
            "daemon_busy",
            "The daemon still owns its inventory lease but is not accepting connections; retry after startup or shutdown completes",
        )),
        Err(error) => Err(AppError::new("daemon_connection", error)),
    }
}

fn already_stopped() -> CommandOutput {
    CommandOutput {
        command: "daemon.stop",
        data: json!({"state": "already_stopped"}),
        human: "Daemon already stopped".into(),
    }
}

pub fn run_from<I, T>(arguments: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    run_from_with_staged_lookup(arguments, crate::install_guard::staged_owner_bin)
}

fn run_from_with_staged_lookup<I, T>(
    arguments: I,
    mut staged_lookup: impl FnMut(&Path, bool) -> io::Result<Option<PathBuf>>,
) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let arguments = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
    let wants_json = arguments
        .iter()
        .any(|argument| argument.as_os_str() == OsStr::new("--json"));
    let cli = match Cli::try_parse_from(arguments.clone()) {
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

    // Grammar-only output must work without device selection, installer routing,
    // daemon startup, or a reachable backend.
    if let TopCommand::Completions { shell } = cli.command {
        emit_success(cli.json, completions::execute(shell));
        return ExitCode::SUCCESS;
    }
    if matches!(
        &cli.command,
        TopCommand::Daemon {
            command: DaemonCommand::ProtocolVersion
        }
    ) {
        return match execute_daemon(DaemonCommand::ProtocolVersion) {
            Ok(output) => {
                emit_success(cli.json, output);
                ExitCode::SUCCESS
            }
            Err(error) => {
                emit_error(cli.json, &error);
                ExitCode::FAILURE
            }
        };
    }

    // These operations govern the whole daemon, not a hosted session's own
    // actors. Refuse before staged-owner routing, SSH preparation, autostart or
    // local installer effects. Presence is enough: malformed/non-Unicode
    // authority must never silently become a User connection.
    let user_only_message = match &cli.command {
        TopCommand::Daemon {
            command:
                DaemonCommand::Bridge
                | DaemonCommand::Start
                | DaemonCommand::Stop { .. }
                | DaemonCommand::Preflight
                | DaemonCommand::PrepareUpgrade { .. }
                | DaemonCommand::Upgrade { .. }
                | DaemonCommand::AbandonUpgrade { .. }
                | DaemonCommand::StopForRebuild { .. },
        } => Some(
            "Daemon lifecycle commands require your own terminal; hosted sessions cannot start, stop or upgrade the daemon",
        ),
        TopCommand::CoordinateInstall { .. } | TopCommand::RestartWindows { .. } => {
            Some("Installation and window restart commands require your own terminal")
        }
        TopCommand::Account {
            command:
                AccountCommand::Link { .. }
                | AccountCommand::Default { .. }
                | AccountCommand::Remove { .. },
        } => Some("Account changes require your own terminal"),
        _ => None,
    };
    if std::env::var_os(crate::agent_execution::AUTHORITY_ENV).is_some()
        && let Some(message) = user_only_message
    {
        emit_error(cli.json, &AppError::new("permission_denied", message));
        return ExitCode::FAILURE;
    }

    // These implementations operate on this installation's files or pidfds.
    // Refuse a remote target before selecting/preparing it or touching local
    // recovery state; the selected remote must never imply local consent.
    if (cli.device.as_deref().is_some_and(|name| name != "local") || crate::device::is_remote())
        && matches!(
            &cli.command,
            TopCommand::Daemon {
                command: DaemonCommand::Bridge
                    | DaemonCommand::Upgrade { .. }
                    | DaemonCommand::AbandonUpgrade { .. }
                    | DaemonCommand::StopForRebuild { .. },
            } | TopCommand::CoordinateInstall { .. }
                | TopCommand::RestartWindows { .. }
        )
    {
        emit_error(
            cli.json,
            &AppError::new(
                "daemon_local_only",
                "This operation manages the local installation; omit --device or select local",
            ),
        );
        return ExitCode::FAILURE;
    }

    // Physical-device image receipts are user-managed local files. Refuse
    // hosted/remote authority before device preparation, staged-owner routing,
    // daemon recovery, or even opening the image store.
    if let TopCommand::Chat {
        command: ChatCommand::ImageAssets { command },
    } = &cli.command
    {
        let hosted = [
            crate::agent_execution::AUTHORITY_ENV,
            "CAGENTS_AGENT_TOKEN",
            "CAGENTS_SESSION_TOKEN",
        ]
        .iter()
        .any(|name| std::env::var_os(name).is_some());
        return match chat::image_assets::execute(
            command,
            cli.device.as_deref(),
            crate::device::is_remote(),
            hosted,
        ) {
            Ok(output) => {
                emit_success(cli.json, output);
                ExitCode::SUCCESS
            }
            Err(error) => {
                emit_error(cli.json, &error);
                ExitCode::FAILURE
            }
        };
    }

    let stopping_daemon = matches!(
        &cli.command,
        TopCommand::Daemon {
            command: DaemonCommand::Stop { .. }
        }
    );
    let no_start = stopping_daemon
        || cli.no_start
        || std::env::var("CAGENTS_NO_START")
            .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"));
    let _start_policy = StartPolicy(NO_START.swap(no_start, std::sync::atomic::Ordering::Relaxed));

    // Local evidence must remain readable when a staged owner, backend, or
    // daemon cannot start. Do not select/prepare a device or follow a route.
    if matches!(cli.command, TopCommand::Diagnostics) {
        if cli.device.is_some() {
            emit_error(
                cli.json,
                &AppError::new(
                    "diagnostics_local_only",
                    "Diagnostics inspect this device; omit --device",
                ),
            );
            return ExitCode::FAILURE;
        }
        return match execute(TopCommand::Diagnostics) {
            Ok(output) => {
                emit_success(cli.json, output);
                ExitCode::SUCCESS
            }
            Err(error) => {
                emit_error(cli.json, &error);
                ExitCode::FAILURE
            }
        };
    }

    let _device_cleanup = crate::device::Cleanup;
    if let Err(error) = crate::device::select(cli.device.as_deref()) {
        emit_error(cli.json, &AppError::new("device", error));
        return ExitCode::FAILURE;
    }
    if crate::device::is_remote() && matches!(cli.command, TopCommand::Linear(_)) {
        emit_error(
            cli.json,
            &AppError::new(
                "linear_remote_unsupported",
                "Run Linear commands on the device containing the worktree and API key; --device is not supported for Linear",
            ),
        );
        return ExitCode::FAILURE;
    }
    // Retain legacy write scope before installer recovery or an older CLI can
    // handle the command. Legacy reads keep their original routing; a scoped
    // legacy write uses this client's selected host.
    let project_channel_context =
        legacy_model_command_mutates(&cli.command).then(ProjectChannelContext::capture);
    if let Some(context) = &project_channel_context
        && let Err(error) = context.admit_remote()
    {
        emit_error(cli.json, &error);
        return ExitCode::FAILURE;
    }
    // Preserve the original caller before operational recovery or delegation
    // to a staged CLI whose caller-scoped service transport may predate this policy.
    let service_context = (matches!(
        &cli.command,
        TopCommand::Automation { .. }
            | TopCommand::Work { .. }
            | TopCommand::Channel {
                command: ChannelCommand::Work { .. }
            }
    ) || matches!(
        &cli.command,
        TopCommand::Agent { command } if !matches!(command, AgentCommand::Console { .. })
    ))
    .then(ServiceRequestContext::capture);
    if let Some(context) = &service_context
        && let Err(error) = context.admit_remote()
    {
        emit_error(cli.json, &error);
        return ExitCode::FAILURE;
    }
    if !crate::device::is_remote()
        && routes_to_staged_owner_as(&cli.command, project_channel_context.as_ref())
        && !service_context
            .as_ref()
            .is_some_and(|context| context.scoped())
    {
        let recover = !no_start && staged_command_requires_daemon(&cli.command);
        match staged_lookup(&std::env::current_exe().unwrap_or_default(), recover) {
            Ok(Some(bin)) => {
                use std::os::unix::process::CommandExt;
                let mut command =
                    Command::new(bin.join(crate::companion::Product::Cli.file_name()));
                // Carry implicit stop/environment policy into the matched CLI.
                // An older owner that lacks this flag must refuse parsing
                // rather than recover/start services before handling the read.
                if no_start && !cli.no_start {
                    command.arg("--no-start");
                }
                command.args(&arguments[1..]);
                crate::install_guard::sanitize_child_environment(&mut command);
                let error = command.exec();
                emit_error(cli.json, &AppError::new("deferred_install", error));
                return ExitCode::FAILURE;
            }
            Ok(None) => (),
            Err(error) => {
                emit_error(cli.json, &AppError::new("deferred_install", error));
                return ExitCode::FAILURE;
            }
        }
    }
    if matches!(
        cli.command,
        TopCommand::Daemon {
            command: DaemonCommand::Bridge
        }
    ) {
        return match crate::device::serve_stdio() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) if crate::device::daemon_absent(&error) => {
                crate::device::report_absent_bridge();
                ExitCode::from(crate::device::BRIDGE_ABSENT_EXIT)
            }
            Err(error) => {
                eprintln!("cagents daemon bridge: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if crate::device::is_remote() && !matches!(cli.command, TopCommand::Ui { .. }) {
        if let Err(error) = crate::device::prepare() {
            if crate::device::daemon_absent(&error) {
                if stopping_daemon {
                    emit_success(cli.json, already_stopped());
                    return ExitCode::SUCCESS;
                }
                emit_error(cli.json, &AppError::new("daemon_not_running", error));
                return ExitCode::FAILURE;
            }
            emit_error(cli.json, &AppError::new("device", error));
            return ExitCode::FAILURE;
        }
    }
    match execute_with_model_context(cli.command, project_channel_context, service_context) {
        Ok(output) => {
            let code = output.exit_code();
            emit_success(cli.json, output);
            code
        }
        Err(error) => {
            emit_error(cli.json, &error);
            ExitCode::from(if error.kind == "timeout" { 3 } else { 1 })
        }
    }
}

fn staged_command_requires_daemon(command: &TopCommand) -> bool {
    match command {
        TopCommand::Daemon {
            command: DaemonCommand::Start,
        }
        | TopCommand::Ui { .. }
        | TopCommand::Terminal { .. }
        | TopCommand::Project { .. }
        | TopCommand::Channel { .. }
        | TopCommand::Automation { .. }
        | TopCommand::Work { .. }
        | TopCommand::Account { .. }
        | TopCommand::Chat { .. }
        | TopCommand::Whoami
        | TopCommand::Sessions(_)
        | TopCommand::Send(_)
        | TopCommand::Ask(_)
        | TopCommand::Reply(_)
        | TopCommand::Inbox(_) => true,
        TopCommand::Agent { command } => !matches!(command, AgentCommand::Console { .. }),
        // These paths do not call ensure_daemon_ready. Internal helpers and new
        // commands remain non-recovering until their actual contract requires it.
        _ => false,
    }
}

fn routes_to_staged_owner(command: &TopCommand) -> bool {
    !matches!(
        command,
        TopCommand::Completions { .. }
            | TopCommand::Diagnostics
            | TopCommand::CoordinateInstall { .. }
            | TopCommand::VerifyInstallGuard { .. }
            | TopCommand::RecordDeferredInstall { .. }
            | TopCommand::UpdateWorker { .. }
            | TopCommand::RestartWindows { .. }
            | TopCommand::Daemon {
                command: DaemonCommand::ProtocolVersion | DaemonCommand::StopForRebuild { .. }
            }
    )
}

/// Desktop launches need the same matched owner as CLI launches while a staged
/// update is pending. Internal helpers and remote devices do not use this route.
pub fn staged_ui_executable() -> io::Result<Option<PathBuf>> {
    crate::install_guard::staged_owner_bin(&std::env::current_exe()?, true)
        .map(|bin| bin.map(|bin| bin.join(crate::companion::Product::Ui.file_name())))
}

fn execute(command: TopCommand) -> Result<CommandOutput, AppError> {
    execute_with_model_context(command, None, None)
}

fn execute_with_model_context(
    command: TopCommand,
    project_channel_context: Option<ProjectChannelContext>,
    service_context: Option<ServiceRequestContext>,
) -> Result<CommandOutput, AppError> {
    match command {
        TopCommand::Completions { shell } => Ok(completions::execute(shell)),
        TopCommand::Diagnostics => {
            let report = crate::diagnostics::inspect().map_err(|_| {
                AppError::new(
                    "diagnostics_unavailable",
                    "Private diagnostics could not be safely read",
                )
            })?;
            let human = report.human();
            Ok(CommandOutput {
                command: "diagnostics",
                data: serde_json::to_value(report).map_err(|_| {
                    AppError::new(
                        "diagnostics_unavailable",
                        "Private diagnostics could not be encoded",
                    )
                })?,
                human,
            })
        }
        TopCommand::Linear(command) => linear::execute(command),
        TopCommand::Automation { command } => match service_context {
            Some(context) => automations::execute_with_context(command, &context),
            None => automations::execute(command),
        },
        TopCommand::Work { command } => match service_context {
            Some(context) => work::execute_with_context(command, &context),
            None => work::execute(command),
        },
        TopCommand::Agent { command } => match service_context {
            Some(context) => agents::execute_with_context(command, &context),
            None => agents::execute(command),
        },
        TopCommand::Whoami => sessions::whoami(),
        TopCommand::Account { command } => accounts::execute(command),
        TopCommand::Chat { command } => chat::execute(command),
        TopCommand::Sessions(arguments) => sessions::list(arguments),
        TopCommand::Send(arguments) => messages::send(arguments),
        TopCommand::Ask(arguments) => messages::ask(arguments),
        TopCommand::Reply(arguments) => messages::reply(arguments),
        TopCommand::Inbox(arguments) => messages::inbox(arguments),
        TopCommand::CoordinateInstall {
            prefix,
            installer,
            stage_only,
            arguments,
        } => {
            crate::install_guard::run(&prefix, &installer, &arguments, stage_only)
                .map_err(|error| AppError::new("install", error))?;
            Ok(CommandOutput {
                command: "update.coordinate_install",
                data: json!({"reopened": !stage_only, "staged": stage_only}),
                human: if stage_only {
                    "Update staged; running windows and daemon preserved"
                } else {
                    "Installer finished; saved windows reopened"
                }
                .into(),
            })
        }
        TopCommand::VerifyInstallGuard { stage_only } => {
            (if stage_only {
                instance::validate_stage_guard()
            } else {
                instance::validate_install_guard()
            })
            .map_err(|error| AppError::new("install", error))?;
            Ok(CommandOutput {
                command: "update.verify_install_guard",
                data: json!({"valid": true}),
                human: "Installer owns the application update fence".into(),
            })
        }
        TopCommand::RecordDeferredInstall {
            stage_only,
            prefix,
            version_root,
        } => {
            let stage = if stage_only {
                Some((prefix.as_deref().unwrap(), version_root.as_deref().unwrap()))
            } else {
                None
            };
            crate::install_guard::record_deferred(stage)
                .map_err(|error| AppError::new("install", error))?;
            Ok(CommandOutput {
                command: "update.record_deferred_install",
                data: json!({"deferred":true}),
                human: "Daemon replacement deferred".into(),
            })
        }
        TopCommand::Ui {
            new_window,
            window,
            source,
        } => launch_ui(new_window, window.as_deref(), source.as_deref()),
        TopCommand::Daemon { command } => execute_daemon(command),
        TopCommand::Terminal { command } => match project_channel_context {
            Some(context) => execute_terminal_as(command, &context),
            None => execute_terminal(command),
        },
        TopCommand::Project { command } => match project_channel_context {
            Some(context) => execute_project_as(command, &context),
            None => execute_project(command),
        },
        TopCommand::Channel {
            command: ChannelCommand::Work { command },
        } => match service_context {
            Some(context) => channel_work::execute_with_context(command, &context),
            None => channel_work::execute(command),
        },
        TopCommand::Channel { command } => match project_channel_context {
            Some(context) => execute_channel_as(command, &context),
            None => execute_channel(command),
        },
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

fn launch_ui(
    new_window: bool,
    slot: Option<&str>,
    source: Option<&str>,
) -> Result<CommandOutput, AppError> {
    let unique_slot = uuid::Uuid::new_v4().to_string();
    let remote_new_window = new_window && crate::device::is_remote();
    let new_window = new_window && !remote_new_window;
    let slot = if remote_new_window {
        Some(unique_slot.as_str())
    } else {
        slot
    };
    let scoped_slot =
        crate::device::window_slot(slot).map_err(|error| AppError::new("device", error))?;
    let slot = scoped_slot.as_deref();
    let activate = || match slot {
        Some(slot) => instance::activate_window(slot),
        None => instance::activate_running_ui(),
    };
    if !new_window && activate().map_err(|error| AppError::new("launch", error))? {
        return Ok(already_running_ui_output());
    }
    let executable = crate::companion::Product::Ui
        .beside_current()
        .map_err(|error| AppError::new("launch", error))?;
    let predecessor_deadline = Instant::now() + UI_PREDECESSOR_EXIT_TIMEOUT;
    loop {
        let mut command = Command::new(&executable);
        if let Some(device) = crate::device::selected() {
            command.args(["--device", &device.name, "--device-window-scoped"]);
        }
        if new_window {
            command.arg("--new-window");
        }
        if let Some(source) = source {
            command.args(["--from-window", source]);
        }

        if let Some(slot) = slot {
            command.args(["--window", slot]);
        }
        let Err(error) = spawn_ui_process(&mut command) else {
            break;
        };
        if new_window || !handshake_reports_already_running(&error) {
            return Err(AppError::new("launch", error));
        }
        if activate().map_err(|error| AppError::new("launch", error))? {
            return Ok(already_running_ui_output());
        }
        // The window's lease is held, yet nothing takes an activation: its
        // window has closed and the process is finishing its exit. Wait for
        // it to release the lease rather than report a window nobody sees.
        let now = Instant::now();
        if now >= predecessor_deadline {
            return Err(AppError::new("launch", error));
        }
        thread::sleep(UI_PREDECESSOR_EXIT_POLL.min(predecessor_deadline - now));
    }
    Ok(CommandOutput {
        command: "ui",
        data: json!({ "launched": true, "executable": executable }),
        human: "cagents-ui started".to_owned(),
    })
}

pub(crate) fn spawn_ui_process(command: &mut Command) -> io::Result<()> {
    crate::install_guard::sanitize_child_environment(command);
    // Capture the budget before spawning: the child can consume the checkpoint
    // while restoring, but the launcher must retain its full startup allowance.
    // Remote windows attach to an already-running device daemon. Its handoff
    // budget is not stored on the client, whose repository filesystem is gated.
    let timeout = if crate::device::is_remote() {
        UI_LAUNCH_HANDSHAKE_TIMEOUT
    } else {
        crate::upgrade::handoff::startup_timeout().map_err(io::Error::other)?
            + UI_LAUNCH_HANDSHAKE_TIMEOUT
    };
    command
        .env(UI_LAUNCH_HANDSHAKE_ENV, "1")
        .env(instance::UI_LAUNCH_RECOVERY_PROGRESS_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    // SAFETY: the hook performs only the close_range syscall before exec.
    unsafe {
        command.pre_exec(crate::daemon::seal_inherited_descriptors);
    }
    let mut child = command.spawn()?;
    let mut stdout = child.stdout.take();
    let result = stdout
        .as_mut()
        .ok_or_else(|| io::Error::other("cagents-ui handshake pipe was not created"))
        .and_then(|stdout| read_ui_launch_handshake(stdout, timeout));
    finish_ui_launch(child, result, stdout)
}

fn finish_ui_launch(
    mut child: std::process::Child,
    result: io::Result<()>,
    stdout: Option<ChildStdout>,
) -> io::Result<()> {
    if let Err(error) = result {
        if error
            .to_string()
            .starts_with(UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX)
        {
            // A visible window can still offer exact-session recovery. Keep its
            // drafts and controls available while the updater reports failure.
            thread::spawn(move || {
                // A late final acknowledgement must not hit BrokenPipe and
                // turn an otherwise usable recovery window into a failed UI.
                if let Some(mut stdout) = stdout {
                    let mut buffer = [0; 256];
                    loop {
                        match stdout.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(_) => {}
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(100))
                            }
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                            Err(_) => break,
                        }
                    }
                }
                let _ = child.wait();
            });
            return Err(error);
        }
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

    let started = Instant::now();
    let mut deadline = started + timeout;
    let mut recovery_announced = false;
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
                while let Some(newline) = response.iter().position(|byte| *byte == b'\n') {
                    let remaining = response.split_off(newline + 1);
                    response.truncate(newline);
                    if let Some(items) = parse_ui_recovery_progress(&response)? {
                        if recovery_announced {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "cagents-ui announced recovery inventory more than once",
                            ));
                        }
                        recovery_announced = true;
                        // The daemon may have consumed the checkpoint before
                        // this window starts. Its saved-pane inventory supplies
                        // one bounded allowance, never an endless keepalive.
                        deadline = deadline.max(
                            started + UI_LAUNCH_HANDSHAKE_TIMEOUT + Duration::from_secs(30 * items),
                        );
                        response = remaining;
                        continue;
                    }
                    return parse_ui_launch_handshake(&response);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        if recovery_announced {
                            format!(
                                "{UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX}Saved conversations are still recovering. The window was kept open; inspect its recovery notices before retrying the update"
                            )
                        } else {
                            format!(
                                "cagents-ui did not acknowledge initialization within {timeout:?}"
                            )
                        },
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn parse_ui_recovery_progress(response: &[u8]) -> io::Result<Option<u64>> {
    let response = std::str::from_utf8(response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let Some(count) = response.strip_prefix(UI_LAUNCH_RECOVERY_PROGRESS_PREFIX) else {
        return Ok(None);
    };
    // Use the same inventory bound as the durable handoff.
    let count = count
        .parse::<u64>()
        .ok()
        .filter(|count| *count <= crate::upgrade::handoff::MAX_SESSIONS as u64)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "cagents-ui reported an invalid recovery inventory",
            )
        })?;
    Ok(Some(count))
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

/// Retained native conversations that the last daemon upgrade could not
/// reattach and that have not since been reattached or closed, as structured
/// data and a human warning. `None` when nothing needs attention.
fn upgrade_attention() -> Option<(Value, String)> {
    let (evidence, conversations) = match crate::upgrade::handoff::Store::existing() {
        Ok(None) => return None,
        Ok(Some(store)) => (Some(store.unattached_path()), store.unattached()),
        Err(error) => (None, Err(error)),
    };
    let conversations = match conversations {
        Ok(conversations) if conversations.is_empty() => return None,
        Ok(conversations) => conversations,
        Err(error) => {
            return Some((
                json!({ "evidence": evidence, "error": error }),
                format!(
                    "cannot read retained-conversation upgrade evidence: {}",
                    printable(&error)
                ),
            ));
        }
    };
    Some((
        json!({ "evidence": evidence, "unattached_conversations": conversations }),
        describe_unattached(&conversations, evidence.as_deref()),
    ))
}

fn describe_unattached(
    conversations: &[crate::upgrade::handoff::UnattachedSummary],
    evidence: Option<&Path>,
) -> String {
    let mut text = format!(
        "{} retained native conversation{} could not be reattached after the daemon update. Nothing was restarted or resent for them and their providers were left as they were; open each in cagents to see why, then recover or close it.",
        conversations.len(),
        if conversations.len() == 1 { "" } else { "s" },
    );
    for conversation in conversations {
        text.push_str(&format!(
            "\n  {} {} in {}: {}",
            conversation.provider.label(),
            printable(
                conversation
                    .conversation
                    .as_deref()
                    .unwrap_or("(unconfirmed conversation)")
            ),
            printable(&conversation.working_directory.to_string_lossy()),
            printable(&conversation.reason),
        ));
    }
    if let Some(evidence) = evidence {
        text.push_str(&format!(
            "\n  evidence: {}",
            printable(&evidence.to_string_lossy())
        ));
    }
    text
}

/// Escapes control characters, such as terminal escape sequences in a
/// provider's error, and bidirectional formatting characters, which can
/// visually reorder what follows them, before text recorded by the daemon
/// reaches a terminal.
fn printable(text: &str) -> String {
    let mut printable = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control()
            || matches!(
                character,
                '\u{061C}' | '\u{200E}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
            )
        {
            printable.extend(character.escape_default());
        } else {
            printable.push(character);
        }
    }
    printable
}

fn execute_daemon(command: DaemonCommand) -> Result<CommandOutput, AppError> {
    if crate::device::is_remote() && matches!(command, DaemonCommand::Upgrade { .. }) {
        return Err(AppError::new(
            "device",
            "run installation upgrades in a terminal on the selected device",
        ));
    }
    match command {
        DaemonCommand::Bridge => Err(AppError::new(
            "bridge",
            "stdio bridge must run before command output",
        )),
        DaemonCommand::Upgrade { yes, interrupt } => {
            let job = crate::update::upgrade_installed(yes, interrupt)
                .map_err(|error| AppError::new("update", error))?;
            Ok(CommandOutput {
                command: "daemon.upgrade",
                human: job.message.clone(),
                data: serde_json::to_value(job)
                    .map_err(|error| AppError::new("serialization", error))?,
            })
        }
        DaemonCommand::Start => {
            let socket = control_daemon_socket()?;
            if daemon_autostart_disabled() {
                // Readiness still requires the matched protocol; a listener
                // alone does not prove this daemon can serve CLI requests.
                let _client = ControlClient::connect(&socket).map_err(protocol_error)?;
            }
            let mut data = json!({ "instance": instance::NAME, "running": true, "socket": socket });
            // Installers discard a successful start's output, but a daemon that
            // came up without some retained conversations must still say so.
            if let Some((attention, warning)) = upgrade_attention() {
                eprintln!("cagents: {warning}");
                data["upgrade_attention"] = attention;
            }
            Ok(CommandOutput {
                command: "daemon.start",
                data,
                human: format!("{} daemon running ({})", instance::NAME, socket.display()),
            })
        }
        DaemonCommand::Status => {
            let paths = crate::daemon::RuntimePaths::discover()
                .map_err(|error| AppError::new("socket_path", error))?;
            let selected_socket =
                daemon_socket_path().map_err(|error| AppError::new("socket_path", error))?;
            let socket = &selected_socket;
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
            let mut data = json!({
                "instance": instance::NAME,
                "running": running,
                "socket": socket,
                "daemon_version": daemon_version,
                "cli_version": crate::VERSION
            });
            let mut human = if running {
                format!(
                    "{} daemon {} running ({}); cli {}",
                    instance::NAME,
                    daemon_version.as_deref().unwrap_or("starting"),
                    socket.display(),
                    crate::VERSION
                )
            } else {
                format!("{} daemon stopped; cli {}", instance::NAME, crate::VERSION)
            };
            if let Some((attention, warning)) = upgrade_attention() {
                data["upgrade_attention"] = attention;
                human.push_str("\nwarning: ");
                human.push_str(&warning);
            }
            Ok(CommandOutput {
                command: "daemon.status",
                data,
                human,
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
            let mut client = match ControlClient::connect(&socket) {
                Ok(client) => client,
                Err(ControlError::Connect(error)) if daemon_missing(&error) => {
                    confirm_daemon_absent(&socket)?;
                    return Ok(already_stopped());
                }
                Err(error) => return Err(protocol_error(error)),
            };
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
                        "Stop daemon and terminate {} hosted terminal/chat sessions and all their background subagents ({} currently running; includes pending spawns and unconfirmed provider work)? Type yes to confirm: ",
                        preflight.capability.live_terminals,
                        preflight.capability.background_subagents
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
            no_relaunch_agents,
        } => {
            if !yes {
                return Err(AppError::new(
                    "confirmation_required",
                    "full upgrade restarts the daemon: terminals are carried across where supported; other sessions resume in new processes, stopping their active turns and background subagents. Background work will not resume automatically; pass --yes to proceed",
                ));
            }
            let relaunch_agents = if relaunch_agents || no_relaunch_agents {
                relaunch_agents
            } else {
                crate::instance::settings_path()
                    .ok_or_else(|| "no settings directory is available".to_owned())
                    .and_then(|settings| crate::upgrade::relaunch_agents_by_default(&settings))
                    .map_err(|error| {
                        AppError::new(
                            "settings",
                            format!(
                                "{error}; pass --relaunch-agents or --no-relaunch-agents to choose explicitly"
                            ),
                        )
                    })?
            };
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
        DaemonCommand::StopForRebuild {
            including_subagents,
        } => {
            if !including_subagents {
                return Err(AppError::new(
                    "confirmation_required",
                    "Rebuilding stops all development terminal/chat jobs and their background subagents; pass --including-subagents to permit this.",
                ));
            }
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

/// One plain line about a shutdown outcome, for the tray menu and the stop
/// confirmation, which have room for a sentence rather than the CLI's full
/// account of what was and was not requested.
pub fn shutdown_outcome_summary(outcome: &ShutdownOutcome) -> String {
    let sessions = |count: u64| match count {
        1 => "1 hosted session".to_owned(),
        count => format!("{count} hosted sessions"),
    };
    match *outcome {
        ShutdownOutcome::Accepted {
            live_terminals_before: 0,
            ..
        } => "Stopping the daemon…".into(),
        ShutdownOutcome::Accepted {
            live_terminals_before,
            ..
        } => format!("Stopping… {} will close", sessions(live_terminals_before)),
        ShutdownOutcome::EmptyRequired { current_count }
        | ShutdownOutcome::ConfirmationRequired { current_count } => {
            format!("Stop needs confirmation: {} open", sessions(current_count))
        }
        ShutdownOutcome::InventoryChanged { current_count } => format!(
            "Not stopped: hosted sessions or background tasks changed ({} sessions now). Refresh and try again.",
            current_count
        ),
        ShutdownOutcome::Expired => "Not stopped: the confirmation expired. Try again.".into(),
        ShutdownOutcome::DaemonRestarted => "Not stopped: the daemon restarted. Try again.".into(),
        ShutdownOutcome::InvalidCapability => {
            "Not stopped: the confirmation was not accepted. Try again.".into()
        }
        ShutdownOutcome::AlreadyShuttingDown => "The daemon is already stopping…".into(),
    }
}

/// What the stop confirmation reports to the tray that opened it, as one JSON
/// line on its stdout; the tray words it, rather than showing that text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum StopReport {
    Cancelled,
    Finished { outcome: ShutdownOutcome },
    Failed { error: String },
}

fn execute_terminal(command: TerminalCommand) -> Result<CommandOutput, AppError> {
    if terminal_command_mutates(&command) {
        return execute_terminal_as(command, &ProjectChannelContext::capture());
    }
    // Keep the four global read verbs on their existing unauthenticated route.
    let socket = control_daemon_socket()?;
    let client = ControlClient::connect(&socket)
        .map_err(|error| AppError::new("daemon_connection", error))?;
    terminal_output(command, client)
}

fn execute_terminal_as(
    command: TerminalCommand,
    context: &ProjectChannelContext,
) -> Result<CommandOutput, AppError> {
    let client = context.connect(true)?;
    terminal_output(command, client)
}

fn terminal_output(
    command: TerminalCommand,
    mut client: ControlClient,
) -> Result<CommandOutput, AppError> {
    match command {
        TerminalCommand::List { all, exited } => {
            let terminals = client.list().map_err(protocol_error)?;
            Ok(terminal_list::list(&terminals, client.proof(), all, exited))
        }
        TerminalCommand::Show { terminal_id } => {
            let terminals = client.list().map_err(protocol_error)?;
            let terminal = terminals
                .iter()
                .find(|terminal| terminal.session.id == terminal_id)
                .ok_or_else(|| {
                    AppError::new("not_found", format!("Unknown terminal {terminal_id}"))
                })?;
            Ok(terminal_list::show(terminal, client.proof()))
        }
        TerminalCommand::Spawn { cwd, argv } => {
            let cwd = absolute_working_directory(cwd)
                .map_err(|error| AppError::new("working_directory", error))?;
            let recipe = if argv.is_empty() {
                let mut recipe = LaunchRecipe::shell(cwd)
                    .map_err(|error| AppError::new("login_shell", error))?;
                if let Some(shell) = crate::device::shell() {
                    recipe.program = shell.to_owned();
                }
                recipe
            } else {
                LaunchRecipe {
                    unset_environment: Default::default(),
                    program: argv[0].clone(),
                    arguments: argv[1..].to_vec(),
                    working_directory: cwd,
                    environment: BTreeMap::new(),
                }
            };
            let session = TerminalSession {
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
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
        | ControlError::ConnectionClosed
        | ControlError::ConnectionRejected { .. } => "connection",
        _ => "protocol",
    };
    let details = match &error {
        ControlError::Remote {
            code, retryable, ..
        }
        | ControlError::ConnectionRejected {
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

/// Agents, Automation, Workboard and channel work share endpoint selection, while
/// retaining their business-envelope and connection-authentication contracts.
#[derive(Clone)]
struct ServiceRequestContext {
    authority: Authority,
    remote: bool,
}

impl ServiceRequestContext {
    fn capture() -> Self {
        Self {
            authority: Authority::from_environment(),
            remote: crate::device::is_remote(),
        }
    }

    fn scoped(&self) -> bool {
        self.authority.token().is_some()
    }

    fn admit_remote(&self) -> Result<(), AppError> {
        if self.scoped() && self.remote {
            return Err(AppError::new(
                "permission_denied",
                "Agent, automation and work commands from an agent session require your own terminal on the remote device",
            ));
        }
        Ok(())
    }

    fn connect(&self) -> Result<ControlClient, AppError> {
        self.admit_remote()?;
        let socket = if self.scoped() {
            daemon_socket_path().map_err(|error| AppError::new("socket_path", error))?
        } else {
            control_daemon_socket()?
        };
        ControlClient::connect(&socket).map_err(|error| AppError::new("daemon_connection", error))
    }
}

/// Caller scope for legacy terminal and shared-model CLI mutations. ChannelWork carries
/// its own explicit authority and never enters this connection path.
struct ProjectChannelContext {
    authority: Authority,
    remote: bool,
    no_start: bool,
}

impl ProjectChannelContext {
    fn capture() -> Self {
        Self {
            authority: Authority::from_environment(),
            remote: crate::device::is_remote(),
            no_start: daemon_autostart_disabled(),
        }
    }

    fn connect(&self, mutation: bool) -> Result<ControlClient, AppError> {
        self.connect_with(
            mutation,
            || {
                if mutation && self.authority.token().is_some() {
                    daemon_socket_path().map_err(|error| AppError::new("socket_path", error))
                } else {
                    // Preserve --no-start absence and ownership checks for
                    // ordinary Users and intentionally global reads alike.
                    control_daemon_socket()
                }
            },
            control_daemon_socket,
            ControlClient::connect,
        )
    }

    fn admit_remote(&self) -> Result<(), AppError> {
        if self.authority.token().is_some() && self.remote {
            return Err(AppError::new(
                "permission_denied",
                "Project, channel and terminal commands from an agent session require your own terminal on the remote device",
            ));
        }
        Ok(())
    }

    fn connect_with(
        &self,
        mutation: bool,
        existing: impl FnOnce() -> Result<PathBuf, AppError>,
        ready: impl FnOnce() -> Result<PathBuf, AppError>,
        connect: impl FnOnce(&Path) -> Result<ControlClient, ControlError>,
    ) -> Result<ControlClient, AppError> {
        let scoped = self.authority.token().is_some();
        self.admit_remote()?;
        let socket = if (scoped && mutation) || self.no_start {
            existing()?
        } else {
            ready()?
        };
        let mut client =
            connect(&socket).map_err(|error| AppError::new("daemon_connection", error))?;
        // Lists are deliberately global reads, including for an invalid or
        // empty inherited capability. Only legacy mutations bind the stream.
        if scoped && mutation {
            client
                .authenticate(self.authority.clone())
                .map_err(protocol_error)?;
        }
        Ok(client)
    }
}

fn legacy_model_command_mutates(command: &TopCommand) -> bool {
    match command {
        TopCommand::Terminal { command } => terminal_command_mutates(command),
        TopCommand::Project { command } => project_command_mutates(command),
        TopCommand::Channel { command } => channel_command_mutates(command) == Some(true),
        _ => false,
    }
}

fn routes_to_staged_owner_as(
    command: &TopCommand,
    context: Option<&ProjectChannelContext>,
) -> bool {
    routes_to_staged_owner(command)
        && !(legacy_model_command_mutates(command)
            && context.is_some_and(|context| context.authority.token().is_some()))
}

fn terminal_command_mutates(command: &TerminalCommand) -> bool {
    match command {
        TerminalCommand::Spawn { .. }
        | TerminalCommand::Write { .. }
        | TerminalCommand::Close { .. } => true,
        TerminalCommand::List { .. }
        | TerminalCommand::Show { .. }
        | TerminalCommand::Snapshot { .. }
        | TerminalCommand::Wait { .. } => false,
    }
}

fn project_command_mutates(command: &ProjectCommand) -> bool {
    match command {
        ProjectCommand::Open { .. } | ProjectCommand::Close { .. } => true,
        ProjectCommand::List => false,
    }
}

fn channel_command_mutates(command: &ChannelCommand) -> Option<bool> {
    match command {
        ChannelCommand::Spawn { .. }
        | ChannelCommand::Move { .. }
        | ChannelCommand::Close { .. } => Some(true),
        ChannelCommand::List { .. } => Some(false),
        ChannelCommand::Work { .. } => None,
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
    let context = ProjectChannelContext::capture();
    execute_project_as(command, &context)
}

fn execute_project_as(
    command: ProjectCommand,
    context: &ProjectChannelContext,
) -> Result<CommandOutput, AppError> {
    // Remote path discovery can itself resolve a bridge; refuse its captured
    // local-session scope before filesystem or device discovery as well.
    context.admit_remote()?;
    execute_project_with(command, |mutation| context.connect(mutation))
}

fn execute_project_with(
    command: ProjectCommand,
    connect: impl FnOnce(bool) -> Result<ControlClient, AppError>,
) -> Result<CommandOutput, AppError> {
    let mutation = project_command_mutates(&command);
    // Reject invalid filesystem paths before starting a daemon. The shared
    // registry then takes precedence over fresh Git discovery for open folders.
    let selected = match &command {
        ProjectCommand::Open { path } | ProjectCommand::Close { path, .. } => {
            Some(project_directory(path.clone())?)
        }
        ProjectCommand::List => None,
    };
    let mut client = connect(mutation)?;
    let model = selected
        .map(|_| client.read_workspace_model())
        .transpose()
        .map_err(protocol_error)?;
    let command = command.resolved_with_model(model.as_ref())?;
    project_output(&mut client, command)
}

/// A project verb whose repository is already the shared key.
enum ResolvedProject {
    Open {
        repository: PathBuf,
        kind: crate::model::ProjectKind,
    },
    List,
    Close {
        repository: PathBuf,
        confirmed: bool,
    },
}

impl ProjectCommand {
    #[cfg(test)]
    fn resolved(self) -> Result<ResolvedProject, AppError> {
        self.resolved_with_model(None)
    }

    fn resolved_with_model(
        self,
        model: Option<&WorkspaceModel>,
    ) -> Result<ResolvedProject, AppError> {
        Ok(match self {
            Self::Open { path } => {
                let location = resolve_project_location(path, model)?;
                ResolvedProject::Open {
                    repository: location.key,
                    kind: location.kind,
                }
            }
            Self::List => ResolvedProject::List,
            Self::Close { path, yes } => ResolvedProject::Close {
                repository: resolve_repository(path, model)?,
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
        ResolvedProject::Open { repository, kind } => {
            // The same upsert a window publishes when it opens a repository,
            // carrying the name derived from the same key, so a project opened
            // from a shell and one opened in a window are one row that no
            // sidebar can tell apart.
            let model = client
                .edit_workspace_model(ModelEdit::ReopenProject {
                    kind,
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
    let command = match command {
        ChannelCommand::Work { command } => return channel_work::execute(command),
        command => command,
    };
    let context = ProjectChannelContext::capture();
    execute_channel_as(command, &context)
}

fn execute_channel_as(
    command: ChannelCommand,
    context: &ProjectChannelContext,
) -> Result<CommandOutput, AppError> {
    context.admit_remote()?;
    execute_channel_with(command, |mutation| context.connect(mutation))
}

fn execute_channel_with(
    command: ChannelCommand,
    connect: impl FnOnce(bool) -> Result<ControlClient, AppError>,
) -> Result<CommandOutput, AppError> {
    let mutation = channel_command_mutates(&command)
        .expect("ChannelWork is dispatched before legacy channel execution");
    // Reject invalid filesystem paths before starting a daemon. The shared
    // registry then takes precedence over fresh Git discovery for open folders.
    let selected = match &command {
        ChannelCommand::Spawn { repository, .. } => Some(project_directory(repository.clone())?),
        ChannelCommand::List {
            repository: Some(path),
        } => Some(project_directory(Some(path.clone()))?),
        _ => None,
    };
    let mut client = connect(mutation)?;
    let model = selected
        .map(|_| client.read_workspace_model())
        .transpose()
        .map_err(protocol_error)?;
    let command = command.resolved_with_model(model.as_ref())?;
    channel_output(&mut client, command)
}

/// A channel verb whose repository is already the shared key.
///
/// The output path consumes resolved keys; it never rediscovers a project or
/// changes an open folder's identity.
enum ResolvedChannel {
    Move {
        target: String,
        to: ChannelId,
    },
    Spawn {
        repository: PathBuf,
        title: String,
        kind: crate::model::ProjectKind,
    },
    List {
        repository: Option<PathBuf>,
    },
    Close {
        channel_id: ChannelId,
    },
}

impl ChannelCommand {
    #[cfg(test)]
    fn resolved(self) -> Result<ResolvedChannel, AppError> {
        self.resolved_with_model(None)
    }

    fn resolved_with_model(
        self,
        model: Option<&WorkspaceModel>,
    ) -> Result<ResolvedChannel, AppError> {
        Ok(match self {
            Self::Work { .. } => {
                unreachable!("channel work is dispatched before legacy channel resolution")
            }
            Self::Move { target, to } => ResolvedChannel::Move { target, to },
            Self::Spawn { repository, title } => {
                let location = resolve_project_location(repository, model)?;
                ResolvedChannel::Spawn {
                    repository: location.key,
                    kind: location.kind,
                    title,
                }
            }
            // Only an explicit filter is resolved: no `--repository` means every
            // repository, so defaulting to the one containing the current
            // directory would silently hide channels the user asked to see.
            Self::List { repository } => ResolvedChannel::List {
                repository: repository
                    .map(|path| resolve_repository(Some(path), model))
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
        ResolvedChannel::Move { target, to } => channels::move_pane(client, &target, to),
        ResolvedChannel::Spawn {
            repository,
            title,
            kind,
        } => {
            // The project comes first: a channel in a repository no project owns
            // is reachable from no window's sidebar. `OpenProject` is an upsert,
            // so this is a no-op when a window already has the project open.
            client
                .edit_workspace_model(ModelEdit::OpenProject {
                    kind,
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
                    title_source: Default::default(),
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
            let human = channel_list_human(&channels);
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

fn channel_list_human(channels: &[&ModelChannel]) -> String {
    format::table(
        ["CHANNEL", "REPOSITORY", "TITLE"],
        channels
            .iter()
            .map(|channel| {
                [
                    channel.id.to_string(),
                    channel.repository.display().to_string(),
                    channel.title.clone(),
                ]
            })
            .collect(),
        "No channels",
    )
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
/// Git projects use their common metadata directory; folder projects use their
/// canonical directory. An already-open folder keeps its identity until closed.
fn project_directory(path: Option<PathBuf>) -> Result<PathBuf, AppError> {
    let from =
        absolute_working_directory(path).map_err(|error| AppError::new("repository", error))?;
    if crate::device::is_remote() {
        return match crate::device_service::request(crate::device_service::Operation::Discover {
            path: from,
        }) {
            Ok(crate::device_service::Reply::Project { path, .. }) => Ok(path),
            Ok(_) => Err(AppError::new(
                "repository",
                "unexpected remote project response",
            )),
            Err(error) => Err(AppError::new("repository", error)),
        };
    }
    let canonical = from
        .canonicalize()
        .map_err(|error| AppError::new("repository", error))?;
    if !canonical.is_dir() {
        return Err(AppError::new(
            "repository",
            format!("`{}` is not a directory", canonical.display()),
        ));
    }
    Ok(canonical)
}

fn resolve_repository(
    repository: Option<PathBuf>,
    model: Option<&WorkspaceModel>,
) -> Result<PathBuf, AppError> {
    resolve_project_location(repository, model).map(|location| location.key)
}

fn resolve_project_location(
    repository: Option<PathBuf>,
    model: Option<&WorkspaceModel>,
) -> Result<crate::project_location::ProjectLocation, AppError> {
    let from = project_directory(repository)?;
    if crate::device::is_remote() {
        if let Some(project) = model.and_then(|model| model.project(&from)) {
            return Ok(crate::project_location::ProjectLocation {
                key: from,
                kind: project.kind,
            });
        }
        return match crate::device_service::request(crate::device_service::Operation::Discover {
            path: from,
        }) {
            Ok(crate::device_service::Reply::Project { key, kind, .. }) => {
                Ok(crate::project_location::ProjectLocation { key, kind })
            }
            Ok(_) => Err(AppError::new(
                "repository",
                "unexpected remote project response",
            )),
            Err(error) => Err(AppError::new("repository", error)),
        };
    }
    crate::project_location::resolve(&from, model)
        .map_err(|error| AppError::new("repository", error))
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
    if crate::device::is_remote() {
        return match cwd {
            Some(path) if path.is_absolute() => Ok(path),
            Some(_) => Err(io::Error::other(
                "remote working directory must be absolute",
            )),
            None => {
                crate::device::prepare()?;
                crate::device::home()
                    .cloned()
                    .ok_or_else(|| io::Error::other("remote HOME is unavailable"))
            }
        };
    }
    let current = std::env::current_dir()?;
    Ok(match cwd {
        Some(path) if path.is_absolute() => path,
        Some(path) => current.join(path),
        None => current,
    })
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
    #[test]
    fn hosted_daemon_lifecycle_refuses_before_routing_and_user_framing_stays_usable() {
        use std::os::unix::{
            ffi::OsStringExt,
            fs::{MetadataExt, PermissionsExt},
            net::UnixListener,
        };
        const CHILD: &str = "CAGENTS_TEST_DAEMON_LIFECYCLE_ORIGIN";
        let name = thread::current().name().unwrap().to_owned();
        let Some(case) = std::env::var_os(CHILD) else {
            for case in ["hosted", "empty", "non-unicode", "user"] {
                let directory = tempfile::Builder::new().prefix("da-").tempdir().unwrap();
                for leaf in ["home", "runtime", "config", "data", "cache"] {
                    let path = directory.path().join(leaf);
                    std::fs::create_dir(&path).unwrap();
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                        .unwrap();
                }
                let out = directory.path().join("stdout");
                let err = directory.path().join("stderr");
                let mut command = Command::new(std::env::current_exe().unwrap());
                // Predetermined roles against a fake peer only. Never inspect
                // or copy the invoking session's real opaque capability.
                command
                    .env_clear()
                    .args(["--exact", &name, "--nocapture", "--test-threads=1"])
                    .current_dir(directory.path())
                    .env(CHILD, case)
                    .env("PATH", "/usr/bin:/bin")
                    .env("HOME", directory.path().join("home"))
                    .env("XDG_RUNTIME_DIR", directory.path().join("runtime"))
                    .env("XDG_CONFIG_HOME", directory.path().join("config"))
                    .env("XDG_DATA_HOME", directory.path().join("data"))
                    .env("XDG_CACHE_HOME", directory.path().join("cache"))
                    .stdin(Stdio::null())
                    .stdout(std::fs::File::create(&out).unwrap())
                    .stderr(std::fs::File::create(&err).unwrap());
                match case {
                    "hosted" => {
                        command.env(crate::agent_execution::AUTHORITY_ENV, "inert-fixture-role");
                    }
                    "empty" => {
                        command.env(crate::agent_execution::AUTHORITY_ENV, "");
                    }
                    "non-unicode" => {
                        command.env(
                            crate::agent_execution::AUTHORITY_ENV,
                            OsString::from_vec(vec![0xff]),
                        );
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
                        panic!("isolated daemon-origin fixture timed out: {case}");
                    }
                    thread::sleep(Duration::from_millis(5));
                };
                let stdout = std::fs::read_to_string(out).unwrap();
                let stderr = std::fs::read_to_string(err).unwrap();
                assert!(status.success(), "{case}: {stdout}\n{stderr}");
                assert!(stdout.contains(&format!("test {name} ...")));
                assert!(stdout.contains("1 passed; 0 failed; 0 ignored;"));
                assert!(!stdout.contains("inert-fixture-role"));
                assert!(!stderr.contains("inert-fixture-role"));
                let errors = stderr
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .collect::<Vec<_>>();
                assert_eq!(errors.len(), if case == "user" { 6 } else { 26 });
                assert!(errors.iter().all(|error| {
                    error["ok"] == false
                        && error["error"]["kind"]
                            == if case == "user" {
                                "daemon_local_only"
                            } else {
                                "permission_denied"
                            }
                }));
            }
            return;
        };
        let case = case.to_str().unwrap();
        let socket = daemon_socket_path().unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let before = std::fs::symlink_metadata(&socket).unwrap();
        let pinned = proof("fixture-host", "fixture-boot", "fixture-incarnation");
        for no_start in [false, true] {
            let mut argv = vec![
                "cagents",
                "--json",
                "--device",
                "unknown-must-not-be-selected",
            ];
            if no_start {
                argv.push("--no-start");
            }
            argv.extend(["daemon", "protocol-version"]);
            assert_eq!(
                run_from_with_staged_lookup(argv, |_, _| panic!("local protocol query routed")),
                ExitCode::SUCCESS
            );
        }

        fn accept(listener: &UnixListener) -> UnixStream {
            let mut event = libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut event, 1, 5000) }, 1);
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert_eq!(request.version, PROTOCOL_VERSION);
            assert_eq!(request.request_id, 1);
            assert_eq!(request.proof, None);
            assert!(matches!(request.request, ClientRequest::Connect { .. }));
            stream
        }
        fn response(
            stream: &mut UnixStream,
            pinned: &HostSessionProof,
            request_id: u64,
            response: ServerResponse,
        ) {
            write_frame(
                stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request_id),
                    proof: pinned.clone(),
                    message: ServerMessage::Response { response },
                },
            )
            .unwrap();
        }

        if case != "user" {
            let commands: [&[&str]; 13] = [
                &["daemon", "start"],
                &["daemon", "stop", "--require-empty"],
                &["daemon", "preflight"],
                &["daemon", "prepare-upgrade", "--yes", "--no-relaunch-agents"],
                &["daemon", "upgrade", "--yes"],
                &["daemon", "abandon-upgrade", "--yes"],
                &["daemon", "stop-for-rebuild", "--including-subagents"],
                &["daemon", "bridge"],
                &[
                    "coordinate-install",
                    "--prefix",
                    "/missing-prefix",
                    "--installer",
                    "/missing-installer",
                ],
                &["restart-windows", "--instance", "missing-instance"],
                &["account", "default", "claude", "automatic"],
                &["account", "link", "codex", "/missing-home"],
                &["account", "remove", "claude/system", "--yes"],
            ];
            for device in ["local", "fixture-device-must-not-be-selected"] {
                for command in commands {
                    let mut argv = vec!["cagents", "--json", "--device", device];
                    argv.extend_from_slice(command);
                    assert_eq!(
                        run_from_with_staged_lookup(argv, |_, _| panic!(
                            "scoped initiator reached staged routing"
                        )),
                        ExitCode::FAILURE
                    );
                }
            }
            // Test the bridge's own admission as well as public CLI dispatch.
            // The private EOF peer keeps a missing guard a bounded failure;
            // no-start prevents even a broken entry from spawning a daemon.
            {
                let _policy =
                    StartPolicy(NO_START.swap(true, std::sync::atomic::Ordering::Relaxed));
                let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let stop = stopped.clone();
                let observed = listener.try_clone().unwrap();
                let peer = thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        match observed.accept() {
                            Ok((stream, _)) => {
                                drop(stream);
                                return true;
                            }
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
                            Err(error) => panic!("private bridge peer: {error}"),
                        }
                        assert!(Instant::now() < deadline);
                        thread::sleep(Duration::from_millis(1));
                    }
                    false
                });
                let result = crate::device::serve_stdio();
                stopped.store(true, std::sync::atomic::Ordering::Relaxed);
                assert!(!peer.join().unwrap(), "scoped bridge connected");
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            }
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
            );
            let after = std::fs::symlink_metadata(&socket).unwrap();
            assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
            for variable in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"] {
                assert_eq!(
                    std::fs::read_dir(std::env::var_os(variable).unwrap())
                        .unwrap()
                        .count(),
                    0
                );
            }
            assert_eq!(
                run_from(["cagents", "--json", "daemon", "protocol-version"]),
                ExitCode::SUCCESS
            );
            // Reads remain available to hosted callers through the same real
            // CLI and framed connection; this fake peer never owns a process.
            let server = thread::spawn(move || {
                let mut stream = accept(&listener);
                write_frame(
                    &mut stream,
                    &connected_envelope(1, pinned.clone(), pinned.clone()),
                )
                .unwrap();
                let request: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
                assert!(matches!(request.request, ClientRequest::BuildInfo));
                assert_eq!(request.proof, Some(pinned.clone()));
                response(
                    &mut stream,
                    &pinned,
                    request.request_id,
                    ServerResponse::BuildInfo {
                        version: "fixture".into(),
                    },
                );
                assert!(read_frame::<ClientEnvelope>(&mut stream).unwrap().is_none());
            });
            assert_eq!(
                run_from(["cagents", "--json", "--no-start", "daemon", "status"]),
                ExitCode::SUCCESS
            );
            server.join().unwrap();
            return;
        }

        let server = thread::spawn(move || {
            for operation in ["preflight", "prepare", "stop"] {
                let mut stream = accept(&listener);
                write_frame(
                    &mut stream,
                    &connected_envelope(1, pinned.clone(), pinned.clone()),
                )
                .unwrap();
                let capability = ShutdownCapability {
                    proof: pinned.clone(),
                    token: "fake-counted-consent".into(),
                    inventory_generation: 7,
                    live_terminals: 0,
                    background_subagents: 0,
                    background_identity: "fixture-empty".into(),
                    expires_at_unix_ms: i64::MAX,
                };
                let request: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
                assert_eq!(request.proof, Some(pinned.clone()));
                if operation == "stop" {
                    assert!(matches!(
                        request.request,
                        ClientRequest::ShutdownV1 {
                            version: SHUTDOWN_VERSION,
                            operation: ShutdownOperation::Stop {
                                require_empty: true,
                                expected_generation: None
                            }
                        }
                    ));
                    response(
                        &mut stream,
                        &pinned,
                        request.request_id,
                        ServerResponse::ShutdownOutcome(ShutdownOutcome::Accepted {
                            live_terminals_before: 0,
                            termination_requested: true,
                        }),
                    );
                } else {
                    assert!(matches!(
                        request.request,
                        ClientRequest::ShutdownV1 {
                            version: SHUTDOWN_VERSION,
                            operation: ShutdownOperation::Preflight
                        }
                    ));
                    response(
                        &mut stream,
                        &pinned,
                        request.request_id,
                        ServerResponse::ShutdownPreflight(ShutdownPreflight {
                            capability: capability.clone(),
                        }),
                    );
                    if operation == "prepare" {
                        let request: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
                        assert_eq!(request.proof, Some(pinned.clone()));
                        assert!(
                            matches!(request.request, ClientRequest::PrepareUpgrade { capability: actual, relaunch_agents: false } if actual == capability)
                        );
                        response(
                            &mut stream,
                            &pinned,
                            request.request_id,
                            ServerResponse::UpgradePrepared {
                                terminals: 0,
                                chats: 0,
                            },
                        );
                    }
                }
                assert!(read_frame::<ClientEnvelope>(&mut stream).unwrap().is_none());
            }
        });
        for command in [
            vec!["preflight"],
            vec!["prepare-upgrade", "--yes", "--no-relaunch-agents"],
            vec!["stop", "--require-empty"],
        ] {
            let mut argv = vec!["cagents", "--json", "--no-start", "daemon"];
            argv.extend(command);
            assert_eq!(run_from(argv), ExitCode::SUCCESS);
        }
        server.join().unwrap();
        for command in [
            vec!["daemon", "upgrade", "--yes"],
            vec!["daemon", "abandon-upgrade", "--yes"],
            vec!["daemon", "stop-for-rebuild", "--including-subagents"],
            vec!["daemon", "bridge"],
            vec![
                "coordinate-install",
                "--prefix",
                "/missing-prefix",
                "--installer",
                "/missing-installer",
            ],
            vec!["restart-windows", "--instance", "missing-instance"],
        ] {
            let mut argv = vec![
                "cagents",
                "--json",
                "--device",
                "unknown-must-not-be-selected",
            ];
            argv.extend(command);
            assert_eq!(
                run_from_with_staged_lookup(argv, |_, _| panic!("local-only operation routed")),
                ExitCode::FAILURE
            );
        }
    }

    #[test]
    fn readable_channel_list_has_headers_and_a_plain_empty_state() {
        assert_eq!(channel_list_human(&[]), "No channels");
        let channel = ModelChannel {
            id: ChannelId::new(),
            repository: "/repo/.git".into(),
            title: "Build\nlogs".into(),
            title_source: Default::default(),
            ordinal: 1,
        };
        let human = channel_list_human(&[&channel]);
        assert_eq!(human.lines().count(), 2);
        assert_eq!(
            human
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["CHANNEL", "REPOSITORY", "TITLE"]
        );
        assert!(human.ends_with("Build logs"));
        assert!(human.contains(&channel.id.to_string()));
        // Channel repository identity is retained; it is not asserted to be a checkout path.
        assert!(human.contains("/repo/.git"));
    }

    #[test]
    fn rebuild_stop_requires_explicit_permission_before_touching_a_daemon() {
        let error = execute_daemon(DaemonCommand::StopForRebuild {
            including_subagents: false,
        })
        .err()
        .expect("unconfirmed rebuild must refuse before process lookup");
        assert_eq!(error.kind, "confirmation_required");
        assert!(error.message.contains("--including-subagents"));
        let cli = Cli::try_parse_from([
            "cagents",
            "daemon",
            "stop-for-rebuild",
            "--including-subagents",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            TopCommand::Daemon {
                command: DaemonCommand::StopForRebuild {
                    including_subagents: true
                }
            }
        ));
    }

    #[cfg(feature = "ui")]
    #[test]
    fn remote_ui_launch_does_not_read_a_local_daemon_handoff() {
        const CHILD: &str = "CAGENTS_TEST_REMOTE_UI_LAUNCH";
        const TEST: &str = "control::tests::remote_ui_launch_does_not_read_a_local_daemon_handoff";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "1")
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        crate::device::save(crate::device::Device {
            name: "launch-test".into(),
            host: "build-host".into(),
            executable: "cagents".into(),
        })
        .unwrap();
        crate::device::select(Some("launch-test".into())).unwrap();
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "printf '%s\\n' \"$1\"",
            "launch-test",
            UI_LAUNCH_READY,
        ]);
        spawn_ui_process(&mut command).unwrap();
    }
    use std::{os::unix::net::UnixStream, thread};

    use clap::CommandFactory;

    use crate::{
        model::{BootId, HostIncarnation, HostSessionId},
        terminal::ipc::ProtocolError,
    };

    use super::*;

    #[test]
    fn printable_escapes_terminal_and_bidirectional_controls() {
        assert_eq!(
            printable("a\u{1b}[2Jb\u{202E}c\u{2066}d\u{061C}e\u{200F}f"),
            "a\\u{1b}[2Jb\\u{202e}c\\u{2066}d\\u{61c}e\\u{200f}f"
        );
        assert_eq!(printable("plain é 日本"), "plain é 日本");
    }

    #[test]
    fn unattached_upgrade_conversations_are_named_with_their_reasons() {
        let conversation = |reason: &str| crate::upgrade::handoff::UnattachedSummary {
            id: TerminalId::new(),
            provider: crate::agent::Provider::Codex,
            conversation: Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into()),
            working_directory: "/work/project".into(),
            reason: reason.into(),
        };
        let one = describe_unattached(
            &[conversation("no rollout found for thread id")],
            Some(Path::new("/data/daemon-upgrade.unattached.json")),
        );
        assert!(
            one.starts_with("1 retained native conversation could not be reattached"),
            "{one}"
        );
        assert!(one.contains("Nothing was restarted or resent"), "{one}");
        assert!(
            one.contains(
                "\n  Codex aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa in /work/project: no rollout found for thread id"
            ),
            "{one}"
        );
        assert!(
            one.ends_with("\n  evidence: /data/daemon-upgrade.unattached.json"),
            "{one}"
        );
        let mut unconfirmed = conversation("The durable native holder is unavailable");
        unconfirmed.conversation = None;
        let two = describe_unattached(&[conversation("unavailable"), unconfirmed], None);
        assert!(two.starts_with("2 retained native conversations"), "{two}");
        assert!(two.contains("(unconfirmed conversation)"), "{two}");
        assert!(!two.contains("evidence:"), "{two}");
        let hostile =
            describe_unattached(&[conversation("refused\u{1b}]52;c;x\u{7}\nforged")], None);
        assert!(
            hostile.contains("refused\\u{1b}]52;c;x\\u{7}\\nforged"),
            "{hostile}"
        );
        assert!(
            !hostile.chars().any(|c| c.is_control() && c != '\n'),
            "{hostile}"
        );
        assert_eq!(hostile.lines().count(), 2, "{hostile}");
    }

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
    fn upgrade_relaunch_choice_is_explicit_either_way_or_left_to_settings() {
        let parsed = |arguments: &[&str]| {
            let arguments = ["cagents", "daemon", "prepare-upgrade", "--yes"]
                .into_iter()
                .chain(arguments.iter().copied());
            match Cli::try_parse_from(arguments).map(|cli| cli.command) {
                Ok(TopCommand::Daemon {
                    command:
                        DaemonCommand::PrepareUpgrade {
                            relaunch_agents,
                            no_relaunch_agents,
                            ..
                        },
                }) => Some((relaunch_agents, no_relaunch_agents)),
                Ok(other) => panic!("expected an upgrade command, got {other:?}"),
                Err(_) => None,
            }
        };
        assert_eq!(parsed(&[]), Some((false, false)));
        assert_eq!(parsed(&["--relaunch-agents"]), Some((true, false)));
        assert_eq!(parsed(&["--no-relaunch-agents"]), Some((false, true)));
        assert_eq!(parsed(&["--relaunch-agents", "--no-relaunch-agents"]), None);
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

    mod handshake_deadline_tests {
        include!("control/handshake_deadline_tests.rs");
    }

    fn saturated_control_listener(
        path: &Path,
    ) -> (std::os::unix::net::UnixListener, Vec<UnixStream>) {
        let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
        let pending = (0..2)
            .map(|_| connect_control_socket(path, Duration::from_millis(100)).unwrap())
            .collect();
        (listener, pending)
    }

    #[test]
    fn control_socket_backlog_pressure_times_out_without_replacing_the_endpoint() {
        use std::os::unix::fs::MetadataExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.sock");
        let (_listener, _pending) = saturated_control_listener(&path);
        let before = std::fs::symlink_metadata(&path).unwrap();
        let start = Instant::now();
        let error = connect_control_socket(&path, Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() >= Duration::from_millis(50));
        assert!(start.elapsed() < Duration::from_secs(2));
        let after = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
    }

    #[test]
    fn control_socket_retry_connects_and_handshakes_when_the_backlog_drains() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.sock");
        let (listener, pending) = saturated_control_listener(&path);
        let expected = proof("backlog-session", "backlog-boot", "backlog-incarnation");
        let response_proof = expected.clone();
        let server = thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            let accept = || {
                let mut poll = libc::pollfd {
                    fd: listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                assert_eq!(
                    unsafe { libc::poll(&mut poll, 1, 2000) },
                    1,
                    "owned fixture connection must arrive"
                );
                listener.accept().unwrap()
            };
            for _ in 0..pending.len() {
                drop(accept());
            }
            let (mut stream, _) = accept();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let request: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(request.request, ClientRequest::Connect { .. }));
            write_frame(
                &mut stream,
                &connected_envelope(request.request_id, response_proof.clone(), response_proof),
            )
            .unwrap();
        });
        let client = ControlClient::connect(&path).unwrap();
        assert_eq!(client.proof(), &expected);
        assert_eq!(client.stream.read_timeout().unwrap(), Some(IO_TIMEOUT));
        assert_eq!(client.stream.write_timeout().unwrap(), Some(IO_TIMEOUT));
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn control_socket_connection_preserves_missing_and_refused_error_classification() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.sock");
        assert_eq!(
            connect_control_socket(&path, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(listener);
        assert_eq!(
            connect_control_socket(&path, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::ConnectionRefused
        );
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
                        kind: Default::default(),
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
                        ..
                    },
            } = created.request
            else {
                panic!("expected a channel creation, got {:?}", created.request);
            };
            assert_eq!(asked, server_repository);
            assert_eq!(title, "probe");
            let model = WorkspaceModel {
                navigation_orders: Vec::new(),
                identity: uuid::Uuid::new_v4(),
                sequence: 2,
                projects: Vec::new(),
                channels: vec![ModelChannel {
                    title_source: Default::default(),
                    id,
                    repository: server_repository,
                    title,
                    ordinal: 4,
                }],
                closed_projects: Vec::new(),
                views: Vec::new(),
                agents: Vec::new(),
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
            let ResolvedProject::Open { repository, .. } = ProjectCommand::Open {
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
    fn open_folder_identity_survives_git_initialization_for_projects_and_channels() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut model = WorkspaceModel::default();
        model.projects.push(ModelProject {
            kind: crate::model::ProjectKind::Folder,
            repository: root.clone(),
            name: "Folder".into(),
            ordinal: 0,
        });
        // Broken metadata deliberately proves the existing key is consulted
        // before attempting Git rediscovery, not only corrected afterwards.
        std::fs::create_dir(root.join(".git")).unwrap();
        let ResolvedProject::Open { repository, .. } = (ProjectCommand::Open {
            path: Some(root.clone()),
        })
        .resolved_with_model(Some(&model))
        .unwrap() else {
            panic!("expected open");
        };
        assert_eq!(repository, root);
        assert_eq!(
            resolve_project_location(Some(root.clone()), Some(&model))
                .unwrap()
                .kind,
            crate::model::ProjectKind::Folder
        );
        let ResolvedChannel::Spawn { repository, .. } = (ChannelCommand::Spawn {
            repository: Some(root.clone()),
            title: "Work".into(),
        })
        .resolved_with_model(Some(&model))
        .unwrap() else {
            panic!("expected channel");
        };
        assert_eq!(repository, root);
        model.projects.clear();
        assert!(resolve_repository(Some(root), Some(&model)).is_err());
    }

    #[test]
    fn project_resolution_rejects_files_and_accepts_plain_folders() {
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
        let ResolvedProject::Close { repository, .. } = (ProjectCommand::Close {
            path: Some(directory.path().to_path_buf()),
            yes: true,
        })
        .resolved()
        .unwrap() else {
            panic!("expected a resolved close");
        };
        assert_eq!(repository, directory.path().canonicalize().unwrap());
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
                    edit: ModelEdit::ReopenProject {
                        kind: Default::default(),
                        repository: served.clone(),
                        // The checkout's name, never the key's own last
                        // component, which is `.git` and names nothing.
                        name: served_name.clone(),
                    },
                },
                "the CLI opens a project exactly as a window publishes one"
            );
            let model = WorkspaceModel {
                navigation_orders: Vec::new(),
                identity: uuid::Uuid::new_v4(),
                sequence: 4,
                projects: vec![ModelProject {
                    kind: Default::default(),
                    repository: served,
                    name: served_name,
                    ordinal: 2,
                }],
                channels: Vec::new(),
                closed_projects: Vec::new(),
                views: Vec::new(),
                agents: Vec::new(),
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
            kind: Default::default(),
            repository: PathBuf::from("/window/.git"),
            name: "window".into(),
            ordinal: 1,
        };
        let cli_opened = ModelProject {
            kind: Default::default(),
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
                navigation_orders: Vec::new(),
                identity: uuid::Uuid::new_v4(),
                sequence: 7,
                projects: served,
                channels: Vec::new(),
                closed_projects: Vec::new(),
                views: Vec::new(),
                agents: Vec::new(),
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
                        navigation_orders: Vec::new(),
                        identity: uuid::Uuid::new_v4(),
                        sequence: 9,
                        projects: Vec::new(),
                        channels: Vec::new(),
                        closed_projects: Vec::new(),
                        views: Vec::new(),
                        agents: Vec::new(),
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
            navigation_orders: Vec::new(),
            identity: uuid::Uuid::new_v4(),
            sequence: 8,
            projects: vec![ModelProject {
                kind: Default::default(),
                repository: repository.to_path_buf(),
                name: repository_display_name(repository),
                ordinal: 1,
            }],
            channels: channels
                .iter()
                .map(|(id, title, ordinal)| ModelChannel {
                    title_source: Default::default(),
                    id: *id,
                    repository: repository.to_path_buf(),
                    title: (*title).to_owned(),
                    ordinal: *ordinal,
                })
                .collect(),
            closed_projects: Vec::new(),
            views: Vec::new(),
            agents: Vec::new(),
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
    fn pre_image_reference_daemon_revision_is_rejected_before_snapshot_or_prompt() {
        assert!(
            PROTOCOL_VERSION > 104,
            "image reference fields require matched positional peers"
        );
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let server = thread::spawn(move || {
            let hello: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(matches!(hello.request, ClientRequest::Connect { .. }));
            let mut response = connected_envelope(
                1,
                proof("session", "boot", "incarnation"),
                proof("session", "boot", "incarnation"),
            );
            response.version = 104;
            write_frame(&mut server_stream, &response).unwrap();
        });
        assert!(matches!(
            ControlClient::from_stream(client_stream),
            Err(ControlError::UnsupportedVersion { actual: 104 })
        ));
        server.join().unwrap();
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
            unset_environment: Default::default(),
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: PathBuf::from("/tmp"),
            environment: BTreeMap::new(),
        };
        let session = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
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
    #[test]
    fn requested_terminal_streams_do_not_break_later_rpc_or_accept_foreign_frames() {
        for invalid in ["unknown", "proof", "request-id"] {
            let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
            server_stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            server_stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let pinned = proof("terminal-stream", "boot", "incarnation");
            let recipe = LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: Vec::new(),
                working_directory: "/".into(),
                environment: BTreeMap::new(),
                unset_environment: Default::default(),
            };
            let terminal = TerminalDescriptor::relaunchable(TerminalId::new(), recipe, None);
            let session = terminal.session.clone();
            let expected = session.id;
            let server = thread::spawn(move || {
                let connect: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                write_frame(
                    &mut server_stream,
                    &connected_envelope(connect.request_id, pinned.clone(), pinned.clone()),
                )
                .unwrap();
                let spawn: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                assert!(
                    matches!(spawn.request, ClientRequest::Spawn { session, .. } if session.id == expected)
                );
                for (request_id, message) in [
                    (
                        Some(spawn.request_id),
                        ServerMessage::Response {
                            response: ServerResponse::Spawned {
                                terminal: terminal.clone(),
                            },
                        },
                    ),
                    (
                        None,
                        ServerMessage::Synchronized {
                            terminal_id: expected,
                            generation: 0,
                            event_sequence: 0,
                        },
                    ),
                ] {
                    write_frame(
                        &mut server_stream,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id,
                            proof: pinned.clone(),
                            message,
                        },
                    )
                    .unwrap();
                }
                let list: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                assert!(matches!(list.request, ClientRequest::List));
                // These genuine terminal-stream message kinds remain legal
                // after the first attachment replay and during a later RPC.
                for message in [
                    ServerMessage::Redraw {
                        terminal_id: expected,
                        generation: 1,
                    },
                    ServerMessage::Snapshot {
                        terminal_id: expected,
                        snapshot: TerminalSnapshot::full(&crate::terminal::RenderSnapshot::empty()),
                    },
                    ServerMessage::TerminalObserved {
                        terminal_id: expected,
                        epoch: 1,
                    },
                    ServerMessage::Event {
                        terminal_id: expected,
                        sequence: 1,
                        event: crate::terminal::ipc::WireTerminalEvent::Bell,
                    },
                    ServerMessage::Synchronized {
                        terminal_id: expected,
                        generation: 1,
                        event_sequence: 1,
                    },
                    ServerMessage::Response {
                        response: ServerResponse::Listed {
                            terminals: vec![terminal],
                        },
                    },
                ] {
                    let request_id = matches!(message, ServerMessage::Response { .. })
                        .then_some(list.request_id);
                    write_frame(
                        &mut server_stream,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id,
                            proof: pinned.clone(),
                            message,
                        },
                    )
                    .unwrap();
                }
                let list: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                assert!(matches!(list.request, ClientRequest::List));
                let mut envelope = ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: None,
                    proof: pinned,
                    message: ServerMessage::Snapshot {
                        terminal_id: if invalid == "unknown" {
                            TerminalId::new()
                        } else {
                            expected
                        },
                        snapshot: TerminalSnapshot::full(&crate::terminal::RenderSnapshot::empty()),
                    },
                };
                if invalid == "proof" {
                    envelope.proof = proof("different", "boot", "incarnation");
                }
                if invalid == "request-id" {
                    envelope.request_id = Some(list.request_id + 1);
                }
                write_frame(&mut server_stream, &envelope).unwrap();
            });
            let mut client = ControlClient::from_stream(client_stream).unwrap();
            assert_eq!(
                client
                    .spawn(session, Default::default())
                    .unwrap()
                    .session
                    .id,
                expected
            );
            assert_eq!(client.list().unwrap()[0].session.id, expected);
            let result = client.list();
            assert!(match invalid {
                "unknown" => matches!(
                    result,
                    Err(ControlError::UnexpectedMessage {
                        actual: "snapshot",
                        ..
                    })
                ),
                "proof" => matches!(result, Err(ControlError::ProofMismatch)),
                "request-id" => matches!(result, Err(ControlError::RequestIdMismatch { .. })),
                _ => unreachable!(),
            });
            drop(client);
            server.join().unwrap();
        }
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
            unset_environment: Default::default(),
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: PathBuf::from("/tmp"),
            environment: BTreeMap::new(),
        };
        let session = |id| crate::chat::Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
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
            // Subscribed before the answer is queued, a working chat can
            // report ahead of it.
            send(
                &mut server_stream,
                None,
                activity(closed, ActivityState::Working),
            );
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

            // A refused attachment subscribes nothing, so nothing is detached.
            let attach: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            send(
                &mut server_stream,
                Some(attach.request_id),
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::TerminalUnavailable,
                    "Exact native chat not found; no replacement process was spawned",
                    false,
                )),
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
        assert_eq!(client.stream.read_timeout().unwrap(), Some(IO_TIMEOUT));
        assert!(matches!(
            client.close_chat(session(refused)),
            Err(ControlError::Remote {
                code: ProtocolErrorCode::TerminalUnavailable,
                ..
            })
        ));
        assert_eq!(client.stream.read_timeout().unwrap(), Some(IO_TIMEOUT));
        assert!(matches!(
            client.close_chat(session(elsewhere)),
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
    fn retained_chat_close_sends_exact_source_without_attaching_and_requires_receipt() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let pinned = proof("session", "boot", "incarnation");
        let session = crate::chat::Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: LaunchRecipe {
                    program: "codex".into(),
                    arguments: Vec::new(),
                    working_directory: "/fixture".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into()),
            },
            proof: Some(pinned.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: crate::chat::NativeContinuity::Durable {
                provider_version: "0.157.1".into(),
            },
        };
        let expected = session.clone();
        let server = thread::spawn(move || {
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &connected_envelope(1, pinned.clone(), pinned.clone()),
            )
            .unwrap();
            for accepted in [false, true] {
                let request: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                assert_eq!(
                    request.request,
                    ClientRequest::ChatCloseRetained {
                        session: expected.clone()
                    }
                );
                thread::sleep(Duration::from_millis(50));
                let message = if accepted {
                    ServerMessage::Response {
                        response: ServerResponse::Accepted,
                    }
                } else {
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::TerminalUnavailable,
                        "Owner teardown is unconfirmed",
                        true,
                    ))
                };
                write_frame(
                    &mut server_stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request.request_id),
                        proof: pinned.clone(),
                        message,
                    },
                )
                .unwrap();
            }
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        client
            .stream
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        assert!(matches!(
            client.close_retained_chat(session.clone()),
            Err(ControlError::Remote { .. })
        ));
        assert_eq!(client.stream.read_timeout().unwrap(), Some(IO_TIMEOUT));
        client.close_retained_chat(session).unwrap();
        assert!(client.attached_chats.is_empty());
        assert_eq!(client.stream.read_timeout().unwrap(), Some(IO_TIMEOUT));
        server.join().unwrap();
    }

    #[test]
    fn terminal_close_keeps_confirmed_completion_when_detach_connection_ends() {
        for checked in [false, true] {
            for ending in ["detached", "shutdown", "eof", "closed-write"] {
                let (result, reusable) =
                    terminal_close_completion_fixture(checked, "closed", ending);
                assert!(result.is_ok(), "{checked}/{ending}: {result:?}");
                assert_eq!(reusable, ending == "detached");
            }
        }
    }

    #[test]
    fn terminal_close_preserves_unconfirmed_outcomes_and_other_detach_errors() {
        for outcome in ["active", "unknown", "refused"] {
            let (result, _) = terminal_close_completion_fixture(true, outcome, "eof");
            if outcome == "refused" {
                assert!(matches!(
                    result,
                    Err(ControlError::Remote {
                        code: ProtocolErrorCode::TerminalUnavailable,
                        ..
                    })
                ));
            } else {
                assert!(result.is_err(), "{outcome} must not become confirmed Close");
            }
        }
        for ending in [
            "proof",
            "version",
            "request-id",
            "unexpected",
            "refused",
            "timeout",
            "malformed",
        ] {
            let (result, _) = terminal_close_completion_fixture(true, "closed", ending);
            match ending {
                "proof" => assert!(matches!(result, Err(ControlError::ProofMismatch))),
                "version" => assert!(matches!(
                    result,
                    Err(ControlError::UnsupportedVersion { .. })
                )),
                "request-id" => assert!(matches!(
                    result,
                    Err(ControlError::RequestIdMismatch { .. })
                )),
                "unexpected" => assert!(matches!(
                    result,
                    Err(ControlError::UnexpectedResponse { .. })
                )),
                "refused" => assert!(matches!(
                    result,
                    Err(ControlError::Remote {
                        code: ProtocolErrorCode::QueueFull,
                        ..
                    })
                )),
                "timeout" => assert!(matches!(result, Err(ControlError::ReplyTimeout { .. }))),
                "malformed" => assert!(matches!(
                    result,
                    Err(ControlError::Frame(FrameError::InvalidPayload(_)))
                )),
                _ => unreachable!(),
            }
        }
        let (result, _) = terminal_close_completion_fixture(false, "refused", "eof");
        assert!(matches!(
            result,
            Err(ControlError::Remote {
                code: ProtocolErrorCode::TerminalUnavailable,
                ..
            })
        ));
        let (result, _) = terminal_close_completion_fixture(false, "write", "eof");
        assert!(
            result.is_err(),
            "ordinary writes still require successful Detach"
        );
        for outcome in ["active", "unknown"] {
            let (result, _) = terminal_close_completion_fixture(true, outcome, "detached");
            assert!(result.is_ok(), "unchanged non-Closed result: {result:?}");
        }
    }

    /// A private framed peer exercises the public client, including attachment
    /// replay and correlation. It starts no daemon, terminal or provider.
    fn terminal_close_completion_fixture(
        checked: bool,
        outcome: &'static str,
        ending: &'static str,
    ) -> (Result<(), ControlError>, bool) {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        server_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        server_stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let pinned = proof("close-completion", "boot", "incarnation");
        let terminal = TerminalDescriptor::relaunchable(
            TerminalId::new(),
            LaunchRecipe::shell("/fixture".into()).unwrap(),
            None,
        );
        let id = terminal.session.id;
        let (release, wait) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let connect: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &connected_envelope(connect.request_id, pinned.clone(), pinned.clone()),
            )
            .unwrap();
            let attach: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(
                matches!(attach.request, ClientRequest::Attach { terminal_id, .. } if terminal_id == id)
            );
            let send = |stream: &mut UnixStream, request_id, message| {
                write_frame(
                    stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: pinned.clone(),
                        message,
                    },
                )
                .unwrap();
            };
            send(
                &mut server_stream,
                Some(attach.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Attached { terminal },
                },
            );
            send(
                &mut server_stream,
                None,
                ServerMessage::Synchronized {
                    terminal_id: id,
                    generation: 0,
                    event_sequence: 0,
                },
            );
            let close: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(if outcome == "write" {
                matches!(close.request, ClientRequest::Input { terminal_id, ref bytes } if terminal_id == id && bytes == b"fixture")
            } else if checked {
                matches!(close.request, ClientRequest::CloseTerminalChecked { terminal_id, allow_active: true } if terminal_id == id)
            } else {
                matches!(close.request, ClientRequest::ShutdownTerminal { terminal_id } if terminal_id == id)
            });
            let message = if outcome == "refused" {
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::TerminalUnavailable,
                    "original close refused",
                    true,
                ))
            } else if checked {
                ServerMessage::Response {
                    response: ServerResponse::TerminalCloseChecked {
                        terminal_id: id,
                        outcome: match outcome {
                            "closed" => terminal::ipc::TerminalCloseResult::Closed,
                            "active" => terminal::ipc::TerminalCloseResult::Active,
                            "unknown" => terminal::ipc::TerminalCloseResult::Unknown(
                                "original custody remains".into(),
                            ),
                            _ => unreachable!(),
                        },
                    },
                }
            } else {
                ServerMessage::Response {
                    response: ServerResponse::Accepted,
                }
            };
            if ending == "closed-write" {
                // Retire the peer's read direction before the client can
                // receive success and attempt Detach; keep writing that reply.
                server_stream.shutdown(std::net::Shutdown::Read).unwrap();
            }
            send(&mut server_stream, Some(close.request_id), message);
            if ending == "closed-write" {
                return;
            }
            let detach: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(detach.request, ClientRequest::Detach { terminal_id: id });
            if ending == "eof" {
                return;
            }
            if ending == "timeout" {
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
                return;
            }
            if ending == "malformed" {
                server_stream.write_all(&[0, 0, 0, 1, 255]).unwrap();
                return;
            }
            let mut reply = ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(detach.request_id),
                proof: pinned.clone(),
                message: ServerMessage::Response {
                    response: ServerResponse::Detached,
                },
            };
            match ending {
                "shutdown" => {
                    reply.request_id = None;
                    reply.message = ServerMessage::HostShuttingDown;
                }
                "proof" => reply.proof = proof("replacement", "boot", "incarnation"),
                "version" => reply.version += 1,
                "request-id" => reply.request_id = Some(detach.request_id + 1),
                "unexpected" => {
                    reply.message = ServerMessage::Response {
                        response: ServerResponse::Accepted,
                    }
                }
                "refused" => {
                    reply.message = ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::QueueFull,
                        "detach refused",
                        true,
                    ))
                }
                "detached" => {}
                _ => unreachable!(),
            }
            write_frame(&mut server_stream, &reply).unwrap();
            if ending == "detached" {
                let list: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                assert_eq!(list.request, ClientRequest::List);
                send(
                    &mut server_stream,
                    Some(list.request_id),
                    ServerMessage::Response {
                        response: ServerResponse::Listed {
                            terminals: Vec::new(),
                        },
                    },
                );
            }
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        if ending == "timeout" {
            client
                .stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
        }
        let result = if checked {
            client.close_terminal_checked(id, true).map(|actual| {
                assert_eq!(
                    actual,
                    match outcome {
                        "closed" => terminal::ipc::TerminalCloseResult::Closed,
                        "active" => terminal::ipc::TerminalCloseResult::Active,
                        "unknown" => terminal::ipc::TerminalCloseResult::Unknown(
                            "original custody remains".into()
                        ),
                        _ => unreachable!(),
                    }
                );
            })
        } else if outcome == "write" {
            client.write(id, b"fixture".to_vec())
        } else {
            client.close_terminal(id)
        };
        if ending == "timeout" {
            release.send(()).unwrap();
        }
        let reusable = ending == "detached" && client.list().unwrap().is_empty();
        server.join().unwrap();
        (result, reusable)
    }

    #[test]
    fn completed_terminal_release_allows_paired_actor_teardown() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let pinned = proof("session", "boot", "incarnation");
        let session = TerminalSession {
            id: TerminalId::new(),
            launch: LaunchRecipe::shell("/".into()).unwrap(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::Exited {
                last_exit: crate::model::TerminalExit {
                    code: Some(0),
                    signal: None,
                    occurred_at_unix_ms: 1,
                },
            },
        };
        let expected = session.clone();
        let server = thread::spawn(move || {
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &connected_envelope(1, pinned.clone(), pinned.clone()),
            )
            .unwrap();
            let close: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(
                close.request,
                ClientRequest::ReleaseCompletedTerminal {
                    session: expected.clone(),
                    allow_active: true,
                }
            );
            thread::sleep(Duration::from_millis(50));
            write_frame(
                &mut server_stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(close.request_id),
                    proof: pinned,
                    message: ServerMessage::Response {
                        response: ServerResponse::TerminalCloseChecked {
                            terminal_id: expected.id,
                            outcome: terminal::ipc::TerminalCloseResult::Closed,
                        },
                    },
                },
            )
            .unwrap();
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        client
            .stream
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        assert_eq!(
            client.release_completed_terminal(session, true).unwrap(),
            terminal::ipc::TerminalCloseResult::Closed,
        );
        assert_eq!(client.stream.read_timeout().unwrap(), Some(IO_TIMEOUT));
        server.join().unwrap();
    }

    #[test]
    fn chat_close_deadline_bounds_notifications_and_restores_read_timeout() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let pinned = proof("session", "boot", "incarnation");
        let chat_id = TerminalId::new();
        let server = thread::spawn(move || {
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &connected_envelope(1, pinned.clone(), pinned.clone()),
            )
            .unwrap();
            let close: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(close.request, ClientRequest::ChatClose { chat_id });
            // Traffic continues longer than the complete request's budget.
            for _ in 0..100 {
                if write_frame(
                    &mut server_stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: None,
                        proof: pinned.clone(),
                        message: ServerMessage::ChatChanged { chat_id },
                    },
                )
                .is_err()
                {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        client.attached_chats.insert(chat_id);
        let started = Instant::now();
        let error = client
            .request_with_deadline(
                ClientRequest::ChatClose { chat_id },
                started + Duration::from_millis(100),
            )
            .unwrap_err();
        assert!(
            matches!(error, ControlError::ReplyTimeout { timeout, .. } if timeout <= Duration::from_millis(100))
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(client.stream.read_timeout().unwrap(), Some(IO_TIMEOUT));
        assert_eq!(client.stream.write_timeout().unwrap(), Some(IO_TIMEOUT));
        drop(client);
        server.join().unwrap();
    }

    fn preview_impact(pinned: HostSessionProof) -> crate::update::impact::DaemonImpact {
        crate::update::impact::DaemonImpact {
            capability: crate::terminal_ipc::ShutdownCapability {
                proof: pinned,
                token: "preview-only".into(),
                inventory_generation: 1,
                live_terminals: 0,
                background_subagents: 0,
                background_identity: String::new(),
                expires_at_unix_ms: 1,
            },
            policy: crate::update::impact::Policy::WhenIdle,
            sessions: Vec::new(),
            blockers: Vec::new(),
        }
    }

    #[test]
    fn upgrade_impact_accepts_slow_preview_without_changing_normal_rpc_timeout() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let pinned = proof("session", "boot", "incarnation");
        let expected = preview_impact(pinned.clone());
        let response = expected.clone();
        let server = thread::spawn(move || {
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &connected_envelope(1, pinned.clone(), pinned.clone()),
            )
            .unwrap();
            let request: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert_eq!(
                request.request,
                ClientRequest::UpgradeImpact {
                    policy: crate::update::impact::Policy::WhenIdle
                }
            );
            assert_eq!(request.proof, Some(pinned.clone()));
            // This is deliberately beyond the generic RPC's five-second budget.
            thread::sleep(IO_TIMEOUT + Duration::from_millis(100));
            write_frame(
                &mut server_stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: pinned,
                    message: ServerMessage::Response {
                        response: ServerResponse::UpgradeImpact(response),
                    },
                },
            )
            .unwrap();
            // A preview never causes a checkpoint, replay or other operation.
            assert!(
                read_frame::<ClientEnvelope>(&mut server_stream)
                    .unwrap()
                    .is_none()
            );
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        assert_eq!(
            client
                .upgrade_impact(crate::update::impact::Policy::WhenIdle)
                .unwrap(),
            expected
        );
        assert_eq!(client.stream.read_timeout().unwrap(), Some(IO_TIMEOUT));
        assert_eq!(client.stream.write_timeout().unwrap(), Some(IO_TIMEOUT));
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn upgrade_preview_total_deadline_and_exact_reply_fences_are_preserved() {
        for case in 0..5 {
            let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
            let pinned = proof("session", "boot", "incarnation");
            let server = thread::spawn(move || {
                let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                write_frame(
                    &mut server_stream,
                    &connected_envelope(1, pinned.clone(), pinned.clone()),
                )
                .unwrap();
                let request: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                let mut envelope = ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: pinned.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::UpgradeImpact(preview_impact(pinned)),
                    },
                };
                match case {
                    0 => {
                        // Repeated valid unsolicited traffic cannot renew the operation.
                        envelope.request_id = None;
                        envelope.message = ServerMessage::NotificationsChanged;
                        for _ in 0..50 {
                            if write_frame(&mut server_stream, &envelope).is_err() {
                                break;
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                    }
                    1 => {
                        let bytes = postcard::to_stdvec(&envelope).unwrap();
                        server_stream
                            .write_all(&(bytes.len() as u32).to_be_bytes())
                            .unwrap();
                        for byte in bytes {
                            if server_stream.write_all(&[byte]).is_err() {
                                break;
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                    }
                    2 => {
                        envelope.request_id = Some(request.request_id + 1);
                        let _ = write_frame(&mut server_stream, &envelope);
                    }
                    3 => {
                        envelope.proof.host_incarnation = HostIncarnation::new("replacement");
                        let _ = write_frame(&mut server_stream, &envelope);
                    }
                    _ => return,
                }
                // Timing out with unread response bytes can reset a Unix
                // socket when the client closes it. Inspect the first byte
                // directly: even an incomplete replay must fail this guard,
                // rather than being hidden by a frame reader's later reset.
                let mut replay = [0];
                match server_stream.read(&mut replay) {
                    Ok(0) => {}
                    Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
                    result => panic!("no automatic request replay (case {case}): {result:?}"),
                }
            });
            let mut client = ControlClient::from_stream(client_stream).unwrap();
            let started = Instant::now();
            let error = client
                .upgrade_impact_with_deadline(
                    crate::update::impact::Policy::WhenIdle,
                    started + Duration::from_millis(80),
                )
                .unwrap_err();
            match case {
                0 | 1 => assert!(matches!(error, ControlError::ReplyTimeout { .. })),
                2 => assert!(matches!(error, ControlError::RequestIdMismatch { .. })),
                3 => assert!(matches!(error, ControlError::ProofMismatch)),
                _ => assert!(matches!(error, ControlError::ConnectionClosed)),
            }
            assert!(started.elapsed() < Duration::from_secs(1));
            assert_eq!(client.stream.read_timeout().unwrap(), Some(IO_TIMEOUT));
            assert_eq!(client.stream.write_timeout().unwrap(), Some(IO_TIMEOUT));
            drop(client);
            server.join().unwrap();
        }
    }

    #[test]
    fn a_long_reply_deadline_reports_the_actual_budget() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let pinned = proof("session", "boot", "incarnation");
        let server = thread::spawn(move || {
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &connected_envelope(1, pinned.clone(), pinned),
            )
            .unwrap();
            let _: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            // No response: the client must enforce its complete deadline.
            assert!(
                read_frame::<ClientEnvelope>(&mut server_stream)
                    .unwrap()
                    .is_none()
            );
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let started = Instant::now();
        let error = client
            .request_with_deadline(ClientRequest::List, started + Duration::from_millis(6100))
            .unwrap_err();
        assert!(
            matches!(&error, ControlError::ReplyTimeout { timeout, .. } if *timeout > IO_TIMEOUT && *timeout <= Duration::from_millis(6100)),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains("the daemon did not answer within 6."),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(8));
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn request_deadline_bounds_a_full_socket_write_buffer() {
        let (mut client_stream, _held_server) = UnixStream::pair().unwrap();
        client_stream.set_nonblocking(true).unwrap();
        loop {
            match client_stream.write(&[0; 8192]) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("fill socket: {error}"),
            }
        }
        client_stream.set_nonblocking(false).unwrap();
        let started = Instant::now();
        let error = write_frame(
            &mut DeadlineWriter {
                stream: &mut client_stream,
                deadline: started + Duration::from_millis(20),
            },
            &ClientRequest::Agent {
                authority: crate::agent_execution::Authority::default(),
                operation: crate::agent_execution::Operation::PeekInbox(
                    crate::agent_execution::mailbox::Inbox {
                        all: false,
                        replies_to: None,
                    },
                ),
            },
        )
        .unwrap_err();
        assert!(matches!(error, FrameError::Io(error)
            if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn response_deadline_bounds_partial_frame_reads() {
        let (mut client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let server = thread::spawn(move || {
            for byte in 100_u32.to_be_bytes().into_iter().chain([0; 100]) {
                if server_stream.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let started = Instant::now();
        let error = read_frame::<ServerEnvelope>(&mut DeadlineReader {
            stream: &mut client_stream,
            deadline: started + Duration::from_millis(100),
        })
        .unwrap_err();
        assert!(matches!(error, FrameError::Io(error)
            if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(client_stream);
        server.join().unwrap();
    }

    /// The daemon says why it is dropping a connection in an untagged error;
    /// the request that error interrupts reports it rather than a stray frame.
    #[test]
    fn an_untagged_daemon_error_fails_the_pending_request_with_its_reason() {
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
            write_frame(
                &mut server_stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: None,
                    proof: peer_proof,
                    message: ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::QueueFull,
                        "client delivery queue overflowed",
                        true,
                    )),
                },
            )
            .unwrap();
        });

        let mut client = ControlClient::from_stream(client_stream).unwrap();
        assert!(matches!(
            client.list_chats(),
            Err(ControlError::ConnectionRejected {
                code: ProtocolErrorCode::QueueFull,
                retryable: true,
                ..
            })
        ));
        server.join().unwrap();
    }

    #[test]
    fn ui_launcher_advertises_recovery_progress_support() {
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "test \"$CAGENTS_UI_LAUNCH_RECOVERY_PROGRESS\" = 1 || exit 94; printf 'ready\\n'",
            ])
            .env_remove("CAGENTS_UI_LAUNCH_RECOVERY_PROGRESS");
        spawn_ui_process(&mut command).unwrap();
    }

    #[test]
    fn ui_launch_handshake_waits_for_saved_pane_recovery() {
        let mut child = Command::new("sh")
            .args([
                "-c",
                "printf 'recovering: 1\\n'; sleep 0.1; printf 'ready\\n'",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let result = read_ui_launch_handshake(&mut stdout, Duration::from_millis(20));
        child.wait().unwrap();
        result.unwrap();
    }

    #[test]
    fn ui_launch_recovery_inventory_is_bounded_and_announced_once() {
        assert_eq!(
            parse_ui_recovery_progress(b"recovering: 5120").unwrap(),
            Some(5120)
        );
        assert_eq!(parse_ui_recovery_progress(b"ready").unwrap(), None);
        for invalid in ["recovering: 5121", "recovering: -1", "recovering: unknown"] {
            assert_eq!(
                parse_ui_recovery_progress(invalid.as_bytes())
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        for (response, valid) in [
            ("recovering: 0\\nready\\n", true),
            ("recovering: 0\\nrecovering: 0\\nready\\n", false),
        ] {
            let mut child = Command::new("printf")
                .arg(response)
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdout = child.stdout.take().unwrap();
            let result = read_ui_launch_handshake(&mut stdout, Duration::from_secs(1));
            child.wait().unwrap();
            assert_eq!(result.is_ok(), valid);
        }
    }

    #[test]
    fn ui_launch_recovery_keeps_the_pipe_for_a_late_acknowledgement() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("late-ready-written");
        let mut child = Command::new("sh")
            .args(["-c", "printf 'recovering: 1\\nerror: recovery-incomplete: still reconnecting\\n'; sleep 0.1; printf 'ready\\n' || exit 44; printf usable > \"$1\"", "fixture"])
            .arg(&marker)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let result = read_ui_launch_handshake(&mut stdout, Duration::from_secs(1));
        assert!(
            result
                .as_ref()
                .unwrap_err()
                .to_string()
                .starts_with(UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX)
        );
        finish_ui_launch(child, result, Some(stdout)).unwrap_err();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.is_file() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            marker.is_file(),
            "the late final receipt must retain a reader"
        );
    }

    #[test]
    fn incomplete_pane_recovery_keeps_the_window_alive() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("window-still-usable");
        let child = Command::new("sh")
            .args(["-c", "sleep 0.1; printf usable > \"$1\"", "fixture"])
            .arg(&marker)
            .spawn()
            .unwrap();
        let error = finish_ui_launch(
            child,
            Err(io::Error::other(format!(
                "{UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX}one pane unavailable"
            ))),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("one pane unavailable"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.is_file() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            marker.is_file(),
            "the recovery window must survive the launcher's error"
        );

        let failed_marker = directory.path().join("initialization-failed");
        let child = Command::new("sh")
            .args(["-c", "sleep 0.1; printf invalid > \"$1\"", "fixture"])
            .arg(&failed_marker)
            .spawn()
            .unwrap();
        finish_ui_launch(child, Err(io::Error::other("initialization failed")), None).unwrap_err();
        thread::sleep(Duration::from_millis(200));
        assert!(
            !failed_marker.exists(),
            "ordinary failed initialization still terminates its child"
        );
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
