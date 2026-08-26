//! Git repository and worktree services.
//!
//! The service is deliberately synchronous: callers should place potentially slow
//! operations on `spawn_blocking`.  Commands are represented as data before they are
//! executed, which keeps argument handling shell-free and makes every operation
//! straightforward to test with a fake executor.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::model::{OperationId, WorktreeId};

/// How long a terminated process group is given to exit on SIGTERM before it
/// is killed outright.
const TERMINATION_GRACE: Duration = Duration::from_millis(150);
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_STDOUT_LIMIT: usize = 16 * 1024 * 1024;
const DEFAULT_STDERR_LIMIT: usize = 4 * 1024 * 1024;
const FAILURE_EXCERPT_LIMIT: usize = 64 * 1024;
const ZERO_SHA1: &str = "0000000000000000000000000000000000000000";

/// A command invocation. `program` and every argument are passed directly to
/// [`Command`]; this type never accepts a shell command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    pub environment: BTreeMap<OsString, OsString>,
    pub removed_environment: Vec<OsString>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
    pub stdout_limit: usize,
    pub stderr_limit: usize,
}

impl CommandSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            current_dir: None,
            environment: BTreeMap::new(),
            removed_environment: Vec::new(),
            stdin: None,
            timeout: Some(DEFAULT_COMMAND_TIMEOUT),
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        }
    }

    pub fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.arguments.extend(arguments.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    pub fn env(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.insert(name.into(), value.into());
        self
    }

    pub fn env_remove(mut self, name: impl Into<OsString>) -> Self {
        self.removed_environment.push(name.into());
        self
    }

    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn output_limits(mut self, stdout_limit: usize, stderr_limit: usize) -> Self {
        self.stdout_limit = stdout_limit;
        self.stderr_limit = stderr_limit;
        self
    }

    pub fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        command.args(&self.arguments);
        if let Some(current_dir) = &self.current_dir {
            command.current_dir(current_dir);
        }
        for name in &self.removed_environment {
            command.env_remove(name);
        }
        command.envs(&self.environment);
        command.stdin(if self.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessFailureKind {
    Spawn,
    Io,
    Cancelled,
    TimedOut,
    Exit,
    OutputLimit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessFailure {
    pub kind: ProcessFailureKind,
    pub program: PathBuf,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub message: String,
    /// False means the executable never started, so it cannot have produced an
    /// external side effect.
    pub started: bool,
}

impl fmt::Display for ProcessFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.program.display(), self.message)
    }
}

impl std::error::Error for ProcessFailure {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessOutput {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ProcessOutput {
    fn from_parts(status: ExitStatus, stdout: Vec<u8>, stderr: Vec<u8>) -> Self {
        Self {
            exit_code: status.code(),
            signal: exit_signal(&status),
            success: status.success(),
            stdout,
            stderr,
        }
    }

    pub fn require_success(self, program: &Path) -> Result<Self, ProcessFailure> {
        if self.success {
            Ok(self)
        } else {
            Err(ProcessFailure {
                kind: ProcessFailureKind::Exit,
                program: program.to_path_buf(),
                exit_code: self.exit_code,
                signal: self.signal,
                stdout: self.stdout,
                stderr: self.stderr,
                message: format!("process exited with status {:?}", self.exit_code),
                started: true,
            })
        }
    }
}

/// What woke a blocking process wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitEvent {
    Exited,
    Stdout,
    Stderr,
    Stdin,
    Cancelled,
}

/// Cooperative cancellation shared between the UI operation and a blocking
/// process runner.
///
/// Cancellation is *pushed*, not polled. A runner registers a waker for the
/// duration of one blocking wait, so [`CancellationToken::cancel`] interrupts
/// it immediately. That is what allows the runner to block on the child
/// instead of waking on a timer to ask whether anything has happened yet.
/// [`CancellationToken::is_cancelled`] remains available for cheap checks
/// before launch and for executors that cannot register a waker.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<CancellationState>);

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    wakers: Mutex<Vec<(u64, mpsc::Sender<WaitEvent>)>>,
    next_waker: AtomicU64,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        // Publish the flag before waking anyone, so a woken waiter is
        // guaranteed to observe it.
        self.0.cancelled.store(true, Ordering::Release);
        for (_, waker) in lock_wakers(&self.0).iter() {
            let _ = waker.send(WaitEvent::Cancelled);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    /// Registers a waker for the duration of one blocking wait. The returned
    /// guard deregisters it, so a long-lived token does not accumulate senders.
    fn register(&self, waker: mpsc::Sender<WaitEvent>) -> CancellationWaker<'_> {
        let id = self.0.next_waker.fetch_add(1, Ordering::Relaxed);
        lock_wakers(&self.0).push((id, waker));
        CancellationWaker {
            state: &self.0,
            id,
        }
    }
}

struct CancellationWaker<'token> {
    state: &'token CancellationState,
    id: u64,
}

impl Drop for CancellationWaker<'_> {
    fn drop(&mut self) {
        let mut wakers = lock_wakers(self.state);
        if let Some(index) = wakers.iter().position(|(id, _)| *id == self.id) {
            wakers.swap_remove(index);
        }
    }
}

/// A panic elsewhere must never be able to disable cancellation, and the
/// guarded vector has no invariant that a panic could leave broken.
fn lock_wakers(state: &CancellationState) -> MutexGuard<'_, Vec<(u64, mpsc::Sender<WaitEvent>)>> {
    state
        .wakers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub trait ProcessExecutor: Send + Sync {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure>;
}

/// Runs commands as real child processes.
///
/// The wait path blocks rather than polls: a dedicated thread owns the `Child`
/// and blocks in `wait`, the pipe readers announce their own completion, and
/// cancellation arrives through [`CancellationToken`]. A `git` or `gh`
/// invocation therefore pays no polling latency and causes no timer wakeups,
/// which matters because a single sidebar refresh issues dozens of them back to
/// back.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemProcessExecutor;

impl ProcessExecutor for SystemProcessExecutor {
    fn execute(
        &self,
        spec: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        if cancellation.is_cancelled() {
            return Err(process_failure(
                spec,
                ProcessFailureKind::Cancelled,
                "operation was cancelled before launch",
                false,
            ));
        }

        let mut child = spec.to_command().spawn().map_err(|error| ProcessFailure {
            kind: ProcessFailureKind::Spawn,
            program: spec.program.clone(),
            exit_code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            message: error.to_string(),
            started: false,
        })?;

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let status = terminate_process_tree(child);
                return Err(ProcessFailure {
                    kind: ProcessFailureKind::Io,
                    program: spec.program.clone(),
                    exit_code: status.as_ref().and_then(ExitStatus::code),
                    signal: status.as_ref().and_then(exit_signal),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    message: "stdout pipe was unavailable".into(),
                    started: true,
                });
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                let status = terminate_process_tree(child);
                return Err(ProcessFailure {
                    kind: ProcessFailureKind::Io,
                    program: spec.program.clone(),
                    exit_code: status.as_ref().and_then(ExitStatus::code),
                    signal: status.as_ref().and_then(exit_signal),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    message: "stderr pipe was unavailable".into(),
                    started: true,
                });
            }
        };

        // Every thread below reports on `events`, and so does cancellation, so
        // the main path can block on one receive instead of waking on a timer.
        let (events, event_source) = mpsc::channel();
        // Registered before the loop's first cancellation check, so a `cancel`
        // racing this setup either sets the flag that check reads or wakes the
        // blocking receive.
        let _cancellation_waker = cancellation.register(events.clone());

        let stdout_exceeded = Arc::new(AtomicBool::new(false));
        let stdout_reader = {
            let exceeded = Arc::clone(&stdout_exceeded);
            let events = events.clone();
            let limit = spec.stdout_limit;
            thread::spawn(move || {
                let result = read_pipe(stdout, limit, &exceeded);
                let _ = events.send(WaitEvent::Stdout);
                result
            })
        };
        let stderr_exceeded = Arc::new(AtomicBool::new(false));
        let stderr_reader = {
            let exceeded = Arc::clone(&stderr_exceeded);
            let events = events.clone();
            let limit = spec.stderr_limit;
            thread::spawn(move || {
                let result = read_pipe(stderr, limit, &exceeded);
                let _ = events.send(WaitEvent::Stderr);
                result
            })
        };

        let mut stdin_done = spec.stdin.is_none();
        let stdin_writer = match (spec.stdin.clone(), child.stdin.take()) {
            (Some(bytes), Some(mut stdin)) => {
                let events = events.clone();
                Some(thread::spawn(move || {
                    let result = stdin.write_all(&bytes).and_then(|()| stdin.flush());
                    let _ = events.send(WaitEvent::Stdin);
                    result
                }))
            }
            _ => None,
        };

        // The child moves into a thread that blocks in `wait`, which is what
        // removes the polling latency. Termination below signals the process
        // *group* by pid and therefore needs no access to the `Child`.
        let process_group = child.id();
        let (exit, exit_source) = mpsc::channel();
        let waiter = thread::spawn(move || {
            let status = child.wait();
            // The status is queued before the wakeup, so whoever observes
            // `Exited` can take it without blocking.
            let _ = exit.send(status);
            let _ = events.send(WaitEvent::Exited);
        });

        let deadline = spec.timeout.map(|timeout| Instant::now() + timeout);
        let mut status = None;
        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut terminal_kind = None;
        let mut wait_error = None;
        loop {
            if cancellation.is_cancelled() {
                terminal_kind = Some(ProcessFailureKind::Cancelled);
            } else if stdout_exceeded.load(Ordering::Acquire)
                || stderr_exceeded.load(Ordering::Acquire)
            {
                terminal_kind = Some(ProcessFailureKind::OutputLimit);
            } else if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                terminal_kind = Some(ProcessFailureKind::TimedOut);
            }

            if terminal_kind.is_some() {
                status = terminate_process_group(process_group, status, &exit_source);
                break;
            }

            if status.is_some() && stdout_done && stderr_done && stdin_done {
                break;
            }

            let event = match deadline {
                Some(deadline) => {
                    event_source.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                }
                None => event_source
                    .recv()
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            };
            match event {
                Ok(WaitEvent::Exited) => match exit_source.recv() {
                    Ok(Ok(exit)) => status = Some(exit),
                    Ok(Err(error)) => {
                        wait_error = Some(error.to_string());
                        terminal_kind = Some(ProcessFailureKind::Io);
                        status = terminate_process_group(process_group, None, &exit_source);
                        break;
                    }
                    Err(_) => {
                        wait_error = Some("process waiter reported no exit status".into());
                        terminal_kind = Some(ProcessFailureKind::Io);
                        status = terminate_process_group(process_group, None, &exit_source);
                        break;
                    }
                },
                Ok(WaitEvent::Stdout) => stdout_done = true,
                Ok(WaitEvent::Stderr) => stderr_done = true,
                Ok(WaitEvent::Stdin) => stdin_done = true,
                // Cancellation and the deadline are both re-read at the top of
                // the loop; these wakeups carry nothing else.
                Ok(WaitEvent::Cancelled) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                // Unreachable while the cancellation waker holds a sender, but
                // breaking keeps a lost wakeup from turning into a hang; the
                // missing exit status is reported below.
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        let stdin_error = stdin_writer.and_then(|writer| match writer.join() {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!("stdin: {error}")),
            Err(_) => Some("stdin writer panicked".into()),
        });
        let (stdout_read, stderr_read, pipe_error) = join_readers(stdout_reader, stderr_reader);
        let output_limit_exceeded = stdout_read.limit_exceeded || stderr_read.limit_exceeded;
        if output_limit_exceeded && terminal_kind.is_none() {
            status = terminate_process_group(process_group, status, &exit_source);
            terminal_kind = Some(ProcessFailureKind::OutputLimit);
        }
        let _ = waiter.join();

        let stdout = stdout_read.output;
        let stderr = stderr_read.output;
        if let Some(kind) = terminal_kind {
            let message = match kind {
                ProcessFailureKind::Cancelled => "operation was cancelled".into(),
                ProcessFailureKind::TimedOut => match spec.timeout {
                    Some(timeout) => format!("operation timed out after {timeout:?}"),
                    None => "operation timed out".into(),
                },
                ProcessFailureKind::OutputLimit => output_limit_message(
                    stdout_read.limit_exceeded,
                    spec.stdout_limit,
                    stderr_read.limit_exceeded,
                    spec.stderr_limit,
                ),
                ProcessFailureKind::Io => match (wait_error, pipe_error) {
                    (Some(wait), Some(pipe)) => format!("{wait}; {pipe}"),
                    (Some(error), None) | (None, Some(error)) => error,
                    (None, None) => "process wait failed".into(),
                },
                _ => unreachable!(),
            };
            return Err(ProcessFailure {
                kind,
                program: spec.program.clone(),
                exit_code: status.as_ref().and_then(ExitStatus::code),
                signal: status.as_ref().and_then(exit_signal),
                stdout: failure_excerpt(stdout),
                stderr: failure_excerpt(stderr),
                message,
                started: true,
            });
        }

        let status = status.ok_or_else(|| ProcessFailure {
            kind: ProcessFailureKind::Io,
            program: spec.program.clone(),
            exit_code: None,
            signal: None,
            stdout: failure_excerpt(stdout.clone()),
            stderr: failure_excerpt(stderr.clone()),
            message: "process ended without an exit status".into(),
            started: true,
        })?;
        if let Some(message) = stdin_error.or(pipe_error) {
            return Err(ProcessFailure {
                kind: ProcessFailureKind::Io,
                program: spec.program.clone(),
                exit_code: status.code(),
                signal: exit_signal(&status),
                stdout: failure_excerpt(stdout),
                stderr: failure_excerpt(stderr),
                message,
                started: true,
            });
        }
        Ok(ProcessOutput::from_parts(status, stdout, stderr))
    }
}

fn process_failure(
    spec: &CommandSpec,
    kind: ProcessFailureKind,
    message: &str,
    started: bool,
) -> ProcessFailure {
    ProcessFailure {
        kind,
        program: spec.program.clone(),
        exit_code: None,
        signal: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        message: message.into(),
        started,
    }
}

#[cfg(unix)]
fn exit_signal(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &ExitStatus) -> Option<i32> {
    None
}

/// Signals the process group led by `process_group`.
///
/// Killing by pid rather than through `&mut Child` is what lets the `Child`
/// live in the thread that blocks on `wait`.
#[cfg(unix)]
fn signal_process_group(process_group: u32, signal: i32) {
    unsafe extern "C" {
        #[link_name = "kill"]
        fn send_signal(pid: i32, signal: i32) -> i32;
    }

    // SAFETY: the child was created as the leader of a new process group, so a
    // negative PID targets exactly that group, and both signal numbers used
    // here are valid on the Linux target this application supports.
    unsafe {
        let _ = send_signal(process_group.cast_signed().wrapping_neg(), signal);
    }
}

/// This application only builds on Linux — `persistence` and `session` both
/// depend on Unix-only APIs — so there is no non-Unix process group to signal.
#[cfg(not(unix))]
fn signal_process_group(_process_group: u32, _signal: i32) {}

/// Terminates a process group whose `Child` is owned by a waiter thread.
///
/// `reaped` is the exit status already observed, if any; otherwise the status
/// arrives on `exit_source`. The SIGTERM grace period is a blocking receive
/// rather than a polling loop, so a process that exits promptly is not made to
/// wait for the next poll tick.
fn terminate_process_group(
    process_group: u32,
    reaped: Option<ExitStatus>,
    exit_source: &mpsc::Receiver<io::Result<ExitStatus>>,
) -> Option<ExitStatus> {
    signal_process_group(process_group, SIGTERM);
    let status = reaped.or_else(|| {
        exit_source
            .recv_timeout(TERMINATION_GRACE)
            .ok()
            .and_then(Result::ok)
    });
    // Send SIGKILL even if the group leader exited; descendants may still be
    // alive in the process group.
    signal_process_group(process_group, SIGKILL);
    status.or_else(|| exit_source.recv().ok().and_then(Result::ok))
}

/// Terminates a process group whose `Child` the caller still owns, used only on
/// the cold paths that fail before the main waiter thread exists.
///
/// The child is handed to a short-lived waiter so that this reuses exactly the
/// blocking termination the hot path uses; there is no `try_wait` polling loop
/// anywhere in this executor.
fn terminate_process_tree(mut child: std::process::Child) -> Option<ExitStatus> {
    let process_group = child.id();
    let (exit, exit_source) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let _ = exit.send(child.wait());
    });
    let status = terminate_process_group(process_group, None, &exit_source);
    let _ = waiter.join();
    status
}

struct PipeRead {
    output: Vec<u8>,
    limit_exceeded: bool,
}

fn read_pipe(
    mut pipe: impl Read,
    limit: usize,
    limit_exceeded: &AtomicBool,
) -> io::Result<PipeRead> {
    let mut output = Vec::with_capacity(limit.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = match pipe.read(&mut buffer) {
            Ok(0) => {
                return Ok(PipeRead {
                    output,
                    limit_exceeded: false,
                });
            }
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
        if read > remaining {
            limit_exceeded.store(true, Ordering::Release);
            return Ok(PipeRead {
                output,
                limit_exceeded: true,
            });
        }
    }
}

fn join_readers(
    stdout: thread::JoinHandle<io::Result<PipeRead>>,
    stderr: thread::JoinHandle<io::Result<PipeRead>>,
) -> (PipeRead, PipeRead, Option<String>) {
    fn join_one(
        name: &'static str,
        reader: thread::JoinHandle<io::Result<PipeRead>>,
    ) -> (PipeRead, Option<String>) {
        match reader.join() {
            Ok(Ok(read)) => (read, None),
            Ok(Err(error)) => (
                PipeRead {
                    output: Vec::new(),
                    limit_exceeded: false,
                },
                Some(format!("{name}: {error}")),
            ),
            Err(_) => (
                PipeRead {
                    output: Vec::new(),
                    limit_exceeded: false,
                },
                Some(format!("{name} reader panicked")),
            ),
        }
    }
    let (stdout, stdout_error) = join_one("stdout", stdout);
    let (stderr, stderr_error) = join_one("stderr", stderr);
    let error = match (stdout_error, stderr_error) {
        (Some(stdout), Some(stderr)) => Some(format!("{stdout}; {stderr}")),
        (Some(error), None) | (None, Some(error)) => Some(error),
        (None, None) => None,
    };
    (stdout, stderr, error)
}

fn output_limit_message(
    stdout_exceeded: bool,
    stdout_limit: usize,
    stderr_exceeded: bool,
    stderr_limit: usize,
) -> String {
    match (stdout_exceeded, stderr_exceeded) {
        (true, true) => format!(
            "stdout exceeded its {stdout_limit}-byte limit and stderr exceeded its \
             {stderr_limit}-byte limit"
        ),
        (true, false) => format!("stdout exceeded its {stdout_limit}-byte limit"),
        (false, true) => format!("stderr exceeded its {stderr_limit}-byte limit"),
        (false, false) => "captured output exceeded its limit".into(),
    }
}

fn failure_excerpt(mut output: Vec<u8>) -> Vec<u8> {
    output.truncate(FAILURE_EXCERPT_LIMIT);
    output
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepositoryId(pub Uuid);

impl RepositoryId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RepositoryId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryInfo {
    pub id: RepositoryId,
    pub selected_path: PathBuf,
    pub common_dir: PathBuf,
    pub git_dir: PathBuf,
    pub worktree_root: Option<PathBuf>,
    pub is_bare: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorktreeLock {
    pub locked: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub id: WorktreeId,
    pub path: PathBuf,
    pub head_oid: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub lock: WorktreeLock,
    pub prunable_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DirtyState {
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
    pub conflicted: u32,
    pub submodule: u32,
}

impl DirtyState {
    pub fn is_dirty(&self) -> bool {
        self.staged != 0
            || self.unstaged != 0
            || self.untracked != 0
            || self.conflicted != 0
            || self.submodule != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HeadState {
    Unborn { branch: Option<String> },
    Branch { name: String, oid: String },
    Detached { oid: String },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrackingState {
    NoUpstream,
    Compared {
        upstream: String,
        ahead: u64,
        behind: u64,
    },
    Unknown {
        upstream: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub head: HeadState,
    pub tracking: TrackingState,
    pub dirty: DirtyState,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GitParseError {
    #[error("truncated NUL-delimited {format} output")]
    Truncated { format: &'static str },
    #[error("malformed {format} record {record}: {reason}")]
    Malformed {
        format: &'static str,
        record: usize,
        reason: String,
    },
    #[error("counter overflow while parsing {format}")]
    CounterOverflow { format: &'static str },
}

pub fn parse_worktree_list(output: &[u8]) -> Result<Vec<WorktreeRecord>, GitParseError> {
    let fields = nul_fields(output, "git worktree list")?;
    let mut records = Vec::new();
    let mut current: Option<WorktreeRecord> = None;

    for (index, field) in fields.iter().enumerate() {
        if field.is_empty() {
            if let Some(record) = current.take() {
                records.push(record);
            }
            continue;
        }
        let (key, value) = split_once_space(field);
        match key {
            b"worktree" => {
                if current.is_some() {
                    return Err(malformed(
                        "git worktree list",
                        index,
                        "missing record separator",
                    ));
                }
                let value = value.filter(|value| !value.is_empty()).ok_or_else(|| {
                    malformed("git worktree list", index, "worktree path is missing")
                })?;
                let path = bytes_to_path(value);
                current = Some(WorktreeRecord {
                    id: stable_worktree_id(&path),
                    path,
                    head_oid: None,
                    branch: None,
                    detached: false,
                    bare: false,
                    lock: WorktreeLock::default(),
                    prunable_reason: None,
                });
            }
            b"HEAD" => {
                let record = current.as_mut().ok_or_else(|| {
                    malformed("git worktree list", index, "HEAD appeared before worktree")
                })?;
                let oid = utf8_value(value, "git worktree list", index, "HEAD")?;
                if !is_hex_oid(oid) {
                    return Err(malformed(
                        "git worktree list",
                        index,
                        "invalid HEAD object id",
                    ));
                }
                if oid != ZERO_SHA1 && !oid.bytes().all(|byte| byte == b'0') {
                    record.head_oid = Some(oid.into());
                }
            }
            b"branch" => {
                let record = current.as_mut().ok_or_else(|| {
                    malformed(
                        "git worktree list",
                        index,
                        "branch appeared before worktree",
                    )
                })?;
                record.branch =
                    Some(utf8_value(value, "git worktree list", index, "branch")?.to_owned());
            }
            b"detached" => current_record(&mut current, index)?.detached = true,
            b"bare" => current_record(&mut current, index)?.bare = true,
            b"locked" => {
                let record = current_record(&mut current, index)?;
                record.lock.locked = true;
                record.lock.reason = optional_utf8(value, "git worktree list", index)?;
            }
            b"prunable" => {
                current_record(&mut current, index)?.prunable_reason =
                    optional_utf8(value, "git worktree list", index)?
                        .or_else(|| Some(String::new()));
            }
            _ => {
                if current.is_none() {
                    return Err(malformed(
                        "git worktree list",
                        index,
                        "field appeared before worktree",
                    ));
                }
                // Porcelain output may gain fields. Unknown fields are scoped to the
                // current record and can be ignored safely.
            }
        }
    }
    if current.is_some() {
        return Err(GitParseError::Truncated {
            format: "git worktree list",
        });
    }
    Ok(records)
}

pub fn parse_status(output: &[u8]) -> Result<StatusSnapshot, GitParseError> {
    let fields = nul_fields(output, "git status")?;
    let mut oid: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut saw_oid = false;
    let mut saw_head = false;
    let mut upstream: Option<String> = None;
    let mut ahead_behind: Option<(u64, u64)> = None;
    let mut dirty = DirtyState::default();
    let mut index = 0;

    while index < fields.len() {
        let field = fields[index];
        if field.is_empty() {
            index += 1;
            continue;
        }
        if let Some(header) = field.strip_prefix(b"# ") {
            if let Some(value) = header.strip_prefix(b"branch.oid ") {
                saw_oid = true;
                let value = utf8(value, "git status", index, "branch.oid")?;
                if value == "(initial)" {
                    oid = None;
                } else if is_hex_oid(value) {
                    oid = Some(value.into());
                } else {
                    return Err(malformed("git status", index, "invalid branch.oid"));
                }
            } else if let Some(value) = header.strip_prefix(b"branch.head ") {
                saw_head = true;
                branch = Some(utf8(value, "git status", index, "branch.head")?.into());
            } else if let Some(value) = header.strip_prefix(b"branch.upstream ") {
                upstream = Some(utf8(value, "git status", index, "branch.upstream")?.into());
            } else if let Some(value) = header.strip_prefix(b"branch.ab ") {
                ahead_behind = Some(parse_ahead_behind(value, index)?);
            }
            index += 1;
            continue;
        }

        match field[0] {
            b'1' => parse_changed_entry(field, index, &mut dirty, 9)?,
            b'2' => {
                parse_changed_entry(field, index, &mut dirty, 10)?;
                index += 1;
                let original = fields.get(index).ok_or_else(|| {
                    malformed("git status", index, "rename source path is missing")
                })?;
                if original.is_empty() {
                    return Err(malformed(
                        "git status",
                        index,
                        "rename source path is empty",
                    ));
                }
            }
            b'u' => {
                let fields = fixed_fields(field, index, 11)?;
                increment(&mut dirty.conflicted)?;
                let submodule = fields[2];
                if submodule.first() == Some(&b'S') {
                    increment(&mut dirty.submodule)?;
                }
            }
            b'?' => {
                require_path_record(field, index)?;
                increment(&mut dirty.untracked)?;
            }
            b'!' => require_path_record(field, index)?,
            _ => return Err(malformed("git status", index, "unknown record type")),
        }
        index += 1;
    }

    if !saw_oid || !saw_head {
        return Err(malformed(
            "git status",
            fields.len(),
            "branch.oid and branch.head headers are required",
        ));
    }

    let head = if let Some(oid) = oid {
        match branch.as_deref() {
            Some("(detached)") | None => HeadState::Detached { oid },
            Some(name) => HeadState::Branch {
                name: name.into(),
                oid,
            },
        }
    } else {
        HeadState::Unborn {
            branch: branch.filter(|name| name != "(detached)"),
        }
    };
    let tracking = match (upstream, ahead_behind) {
        (None, None) => TrackingState::NoUpstream,
        (Some(upstream), Some((ahead, behind))) => TrackingState::Compared {
            upstream,
            ahead,
            behind,
        },
        (upstream, _) => TrackingState::Unknown { upstream },
    };
    Ok(StatusSnapshot {
        head,
        tracking,
        dirty,
    })
}

fn parse_changed_entry(
    field: &[u8],
    index: usize,
    dirty: &mut DirtyState,
    minimum_fields: usize,
) -> Result<(), GitParseError> {
    let fields = fixed_fields(field, index, minimum_fields)?;
    let xy = fields[1];
    if xy.len() != 2 {
        return Err(malformed(
            "git status",
            index,
            "XY status must contain two bytes",
        ));
    }
    if xy[0] != b'.' {
        increment(&mut dirty.staged)?;
    }
    if xy[1] != b'.' {
        increment(&mut dirty.unstaged)?;
    }
    let submodule = fields[2];
    if submodule.first() == Some(&b'S') {
        increment(&mut dirty.submodule)?;
    }
    Ok(())
}

fn parse_ahead_behind(value: &[u8], index: usize) -> Result<(u64, u64), GitParseError> {
    let value = utf8(value, "git status", index, "branch.ab")?;
    let mut parts = value.split_ascii_whitespace();
    let ahead = parts
        .next()
        .and_then(|part| part.strip_prefix('+'))
        .and_then(|part| part.parse().ok());
    let behind = parts
        .next()
        .and_then(|part| part.strip_prefix('-'))
        .and_then(|part| part.parse().ok());
    if parts.next().is_some() || ahead.is_none() || behind.is_none() {
        return Err(malformed("git status", index, "invalid branch.ab"));
    }
    Ok((ahead.unwrap(), behind.unwrap()))
}

fn fixed_fields(field: &[u8], index: usize, count: usize) -> Result<Vec<&[u8]>, GitParseError> {
    let fields: Vec<_> = field.splitn(count, |byte| *byte == b' ').collect();
    if fields.len() != count || fields.iter().any(|field| field.is_empty()) {
        Err(malformed(
            "git status",
            index,
            "record has missing or empty fields",
        ))
    } else {
        Ok(fields)
    }
}

fn require_path_record(field: &[u8], index: usize) -> Result<(), GitParseError> {
    if field.len() < 3 || field[1] != b' ' {
        Err(malformed("git status", index, "path is missing"))
    } else {
        Ok(())
    }
}

fn increment(value: &mut u32) -> Result<(), GitParseError> {
    *value = value.checked_add(1).ok_or(GitParseError::CounterOverflow {
        format: "git status",
    })?;
    Ok(())
}

fn nul_fields<'a>(output: &'a [u8], format: &'static str) -> Result<Vec<&'a [u8]>, GitParseError> {
    if output.is_empty() {
        return Ok(Vec::new());
    }
    if output.last() != Some(&0) {
        return Err(GitParseError::Truncated { format });
    }
    Ok(output[..output.len() - 1]
        .split(|byte| *byte == 0)
        .collect())
}

fn split_once_space(field: &[u8]) -> (&[u8], Option<&[u8]>) {
    match field.iter().position(|byte| *byte == b' ') {
        Some(index) => (&field[..index], Some(&field[index + 1..])),
        None => (field, None),
    }
}

fn current_record(
    current: &mut Option<WorktreeRecord>,
    index: usize,
) -> Result<&mut WorktreeRecord, GitParseError> {
    current
        .as_mut()
        .ok_or_else(|| malformed("git worktree list", index, "field appeared before worktree"))
}

fn utf8_value<'a>(
    value: Option<&'a [u8]>,
    format: &'static str,
    index: usize,
    name: &str,
) -> Result<&'a str, GitParseError> {
    let value = value.ok_or_else(|| malformed(format, index, format!("{name} is missing")))?;
    utf8(value, format, index, name)
}

fn utf8<'a>(
    value: &'a [u8],
    format: &'static str,
    index: usize,
    name: &str,
) -> Result<&'a str, GitParseError> {
    std::str::from_utf8(value).map_err(|_| malformed(format, index, format!("{name} is not UTF-8")))
}

fn optional_utf8(
    value: Option<&[u8]>,
    format: &'static str,
    index: usize,
) -> Result<Option<String>, GitParseError> {
    value
        .filter(|value| !value.is_empty())
        .map(|value| utf8(value, format, index, "value").map(str::to_owned))
        .transpose()
}

fn malformed(format: &'static str, record: usize, reason: impl Into<String>) -> GitParseError {
    GitParseError::Malformed {
        format,
        record,
        reason: reason.into(),
    }
}

fn is_hex_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(unix)]
fn bytes_to_path(value: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(value.to_vec()))
}

fn stable_worktree_id(path: &Path) -> WorktreeId {
    WorktreeId::from_uuid(stable_path_uuid(path))
}

fn stable_path_uuid(path: &Path) -> Uuid {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let path_text = path.to_string_lossy();
    #[cfg(not(unix))]
    let bytes = path_text.as_bytes();

    fn fnv(bytes: &[u8], seed: u64) -> u64 {
        bytes.iter().fold(seed, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
    }
    let first = fnv(bytes, 0xcbf29ce484222325);
    let second = fnv(bytes, 0x84222325cbf29ce4);
    let mut uuid = [0_u8; 16];
    uuid[..8].copy_from_slice(&first.to_be_bytes());
    uuid[8..].copy_from_slice(&second.to_be_bytes());
    uuid[6] = (uuid[6] & 0x0f) | 0x80;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    Uuid::from_bytes(uuid)
}

#[cfg(not(unix))]
fn bytes_to_path(value: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(value).into_owned())
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    #[error("branch name is invalid: {0}")]
    InvalidBranch(String),
    #[error("base revision is invalid: {0}")]
    InvalidBase(String),
    #[error("tracking requires a local or remote branch base")]
    TrackingRequiresBranch,
    #[error("destination root is not a directory")]
    InvalidDestinationRoot,
    #[error("destination must have an existing parent beneath the managed root")]
    DestinationOutsideRoot,
    #[error("destination already exists")]
    DestinationOccupied,
    #[error("destination leaf may not be a symlink")]
    DestinationSymlink,
    #[error("path validation failed: {0}")]
    Io(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ValidatedBranchName(String);

impl ValidatedBranchName {
    pub fn parse(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        let invalid_component = value.split('/').any(|component| {
            component.is_empty()
                || component == "."
                || component == ".."
                || component.ends_with(".lock")
                || component.ends_with('.')
        });
        let forbidden = value.bytes().any(|byte| {
            byte.is_ascii_control()
                || byte == b' '
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        });
        if value.is_empty()
            || value.starts_with('-')
            || value.ends_with('/')
            || value.contains("..")
            || value.contains("@{")
            || value == "@"
            || invalid_component
            || forbidden
        {
            Err(ValidationError::InvalidBranch(value))
        } else {
            Ok(Self(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn full_ref(&self) -> String {
        format!("refs/heads/{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum BaseRevision {
    ObjectId(String),
    LocalBranch(ValidatedBranchName),
    RemoteRef(String),
}

impl BaseRevision {
    pub fn object_id(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        if is_hex_oid(&value) {
            Ok(Self::ObjectId(value))
        } else {
            Err(ValidationError::InvalidBase(value))
        }
    }

    pub fn remote_ref(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        let suffix = value.strip_prefix("refs/remotes/");
        if suffix.is_some_and(|suffix| ValidatedBranchName::parse(suffix).is_ok()) {
            Ok(Self::RemoteRef(value))
        } else {
            Err(ValidationError::InvalidBase(value))
        }
    }

    pub fn revision_argument(&self) -> String {
        match self {
            Self::ObjectId(oid) => oid.clone(),
            Self::LocalBranch(branch) => branch.full_ref(),
            Self::RemoteRef(reference) => reference.clone(),
        }
    }

    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ObjectId(oid) => Self::object_id(oid.clone()).map(|_| ()),
            Self::LocalBranch(branch) => ValidatedBranchName::parse(branch.as_str()).map(|_| ()),
            Self::RemoteRef(reference) => Self::remote_ref(reference.clone()).map(|_| ()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateWorktreeRequest {
    pub repository: PathBuf,
    pub managed_root: PathBuf,
    pub destination: PathBuf,
    pub branch: ValidatedBranchName,
    pub base: BaseRevision,
    pub track: bool,
    pub operation_id: OperationId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCreationPlan {
    pub validate_branch: CommandSpec,
    pub resolve_base: CommandSpec,
    pub check_branch_absent: CommandSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeResidualState {
    pub branch_exists: bool,
    pub registered_worktree: Option<WorktreeRecord>,
    pub destination: DestinationResidualState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationResidualState {
    Absent,
    EmptyDirectory,
    NonEmptyDirectory,
    File,
    Symlink,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WorktreeCreationOutcome {
    Created {
        worktree: WorktreeRecord,
        warnings: Vec<ProcessFailure>,
    },
    NotCreated {
        failure: ProcessFailure,
    },
    OutcomeUnknown {
        failure: ProcessFailure,
        residual: WorktreeResidualState,
    },
}

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Parse(#[from] GitParseError),
    #[error("invalid Git output: {error}")]
    InvalidGitOutput {
        error: GitParseError,
        process: ProcessOutput,
    },
    #[error("invalid Git command response: {source}")]
    InvalidResponse {
        #[source]
        source: Box<RepositoryError>,
        process: ProcessOutput,
    },
    #[error(transparent)]
    Process(#[from] ProcessFailure),
    #[error("invalid git output for {field}: {reason}")]
    InvalidOutput { field: &'static str, reason: String },
}

#[derive(Clone)]
pub struct RepositoryService<E = SystemProcessExecutor> {
    git: PathBuf,
    executor: Arc<E>,
}

impl RepositoryService<SystemProcessExecutor> {
    pub fn system(git: impl Into<PathBuf>) -> Self {
        Self::new(git, Arc::new(SystemProcessExecutor::default()))
    }
}

impl<E: ProcessExecutor> RepositoryService<E> {
    pub fn new(git: impl Into<PathBuf>, executor: Arc<E>) -> Self {
        Self {
            git: git.into(),
            executor,
        }
    }

    pub fn discovery_commands(&self, selected_path: &Path) -> Vec<CommandSpec> {
        [
            "--is-bare-repository",
            "--git-common-dir",
            "--git-dir",
            "--show-toplevel",
        ]
        .into_iter()
        .map(|query| {
            self.git_read(
                selected_path,
                ["rev-parse", "--path-format=absolute", query],
            )
        })
        .collect()
    }

    pub fn worktree_list_command(&self, repository: &Path) -> CommandSpec {
        self.git_read(repository, ["worktree", "list", "--porcelain", "-z"])
    }

    pub fn status_command(&self, worktree: &Path) -> CommandSpec {
        self.git_read(
            worktree,
            [
                "status",
                "--porcelain=v2",
                "--branch",
                "--ahead-behind",
                "--untracked-files=normal",
                "-z",
            ],
        )
    }

    pub fn discover(
        &self,
        selected_path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<RepositoryInfo, RepositoryError> {
        let commands = self.discovery_commands(selected_path);
        let bare_output = self.run_checked(&commands[0], cancellation)?;
        let bare =
            attach_repository_process(parse_bool_line(&bare_output.stdout, "bare"), &bare_output)?;
        let common_output = self.run_checked(&commands[1], cancellation)?;
        let common_dir = attach_repository_process(
            parse_path_line(&common_output.stdout, "common_dir"),
            &common_output,
        )?;
        let git_dir_output = self.run_checked(&commands[2], cancellation)?;
        let git_dir = attach_repository_process(
            parse_path_line(&git_dir_output.stdout, "git_dir"),
            &git_dir_output,
        )?;
        let worktree_root = if bare {
            None
        } else {
            let root_output = self.run_checked(&commands[3], cancellation)?;
            Some(attach_repository_process(
                parse_path_line(&root_output.stdout, "worktree_root"),
                &root_output,
            )?)
        };
        Ok(RepositoryInfo {
            id: RepositoryId(stable_path_uuid(&common_dir)),
            selected_path: selected_path.to_path_buf(),
            common_dir,
            git_dir,
            worktree_root,
            is_bare: bare,
        })
    }

    pub fn list_worktrees(
        &self,
        repository: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Vec<WorktreeRecord>, RepositoryError> {
        let command = self.worktree_list_command(repository);
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_worktree_list(&output.stdout);
        match parsed {
            Ok(worktrees) => Ok(worktrees),
            Err(error) => Err(RepositoryError::InvalidGitOutput {
                error,
                process: output,
            }),
        }
    }

    pub fn status(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<StatusSnapshot, RepositoryError> {
        let command = self.status_command(worktree);
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_status(&output.stdout);
        match parsed {
            Ok(status) => Ok(status),
            Err(error) => Err(RepositoryError::InvalidGitOutput {
                error,
                process: output,
            }),
        }
    }

    pub fn plan_worktree_creation(
        &self,
        request: &CreateWorktreeRequest,
    ) -> Result<WorktreeCreationPlan, ValidationError> {
        ValidatedBranchName::parse(request.branch.as_str())?;
        request.base.validate()?;
        if request.track && matches!(&request.base, BaseRevision::ObjectId(_)) {
            return Err(ValidationError::TrackingRequiresBranch);
        }
        validate_destination(&request.managed_root, &request.destination)?;
        let branch = request.branch.as_str();
        let base = format!("{}^{{commit}}", request.base.revision_argument());
        Ok(WorktreeCreationPlan {
            validate_branch: self.git_mutation(
                &request.repository,
                [
                    OsString::from("check-ref-format"),
                    OsString::from("--branch"),
                    branch.into(),
                ],
            ),
            resolve_base: self.git_read(
                &request.repository,
                [
                    OsString::from("rev-parse"),
                    OsString::from("--verify"),
                    OsString::from("--end-of-options"),
                    base.into(),
                ],
            ),
            check_branch_absent: self.git_read(
                &request.repository,
                [
                    OsString::from("show-ref"),
                    OsString::from("--verify"),
                    OsString::from("--quiet"),
                    request.branch.full_ref().into(),
                ],
            ),
        })
    }

    /// Performs a reconciled worktree creation. Callers must serialize this
    /// operation per Git common directory; the service itself never uses a
    /// global lock that would unnecessarily block unrelated repositories.
    pub fn create_worktree(
        &self,
        request: &CreateWorktreeRequest,
        cancellation: &CancellationToken,
    ) -> Result<WorktreeCreationOutcome, RepositoryError> {
        let plan = self.plan_worktree_creation(request)?;
        self.run_checked(&plan.validate_branch, cancellation)?;
        let base_output = self.run_checked(&plan.resolve_base, cancellation)?;
        let base_oid = attach_repository_process(
            parse_utf8_line(&base_output.stdout, "base_oid"),
            &base_output,
        )?;
        if !is_hex_oid(&base_oid) {
            return Err(RepositoryError::InvalidResponse {
                source: Box::new(RepositoryError::InvalidOutput {
                    field: "base_oid",
                    reason: "resolved revision was not a full object id".into(),
                }),
                process: base_output,
            });
        }
        let branch_check = self
            .executor
            .execute(&plan.check_branch_absent, cancellation)?;
        match branch_check.exit_code {
            Some(1) => {}
            _ if branch_check.success => {
                return Err(ValidationError::InvalidBranch(format!(
                    "{} already exists",
                    request.branch.as_str()
                ))
                .into());
            }
            _ => return Err(branch_check.require_success(&self.git).unwrap_err().into()),
        }

        let create = self.create_worktree_command(request, &base_oid)?;
        let creation_result = self.executor.execute(&create, cancellation);
        let creation_failure = match creation_result {
            Ok(output) if output.success => None,
            Ok(output) => Some(output.require_success(&self.git).unwrap_err()),
            Err(failure) => Some(failure),
        };
        if let Some(failure) = creation_failure {
            let residual = self.reconcile_worktree(request, &CancellationToken::new());
            return Ok(match residual {
                Ok(residual)
                    if residual.branch_exists
                        || residual.registered_worktree.is_some()
                        || residual.destination != DestinationResidualState::Absent =>
                {
                    WorktreeCreationOutcome::OutcomeUnknown { failure, residual }
                }
                Ok(_) if failure.started => WorktreeCreationOutcome::NotCreated { failure },
                Ok(_) => WorktreeCreationOutcome::NotCreated { failure },
                Err(_) => WorktreeCreationOutcome::OutcomeUnknown {
                    failure,
                    residual: WorktreeResidualState {
                        branch_exists: false,
                        registered_worktree: None,
                        destination: inspect_destination(&request.destination),
                    },
                },
            });
        }

        let reconciliation_token = CancellationToken::new();
        let mut warnings = Vec::new();
        if request.track {
            let set_upstream = self.set_upstream_command(request)?;
            match self.executor.execute(&set_upstream, &reconciliation_token) {
                Ok(output) if output.success => {}
                Ok(output) => warnings.push(output.require_success(&self.git).unwrap_err()),
                Err(failure) => warnings.push(failure),
            }
        }
        let unlock = self.unlock_worktree_command(request);
        match self.executor.execute(&unlock, &reconciliation_token) {
            Ok(output) if output.success => {}
            Ok(output) => warnings.push(output.require_success(&self.git).unwrap_err()),
            Err(failure) => warnings.push(failure),
        }
        let fallback = WorktreeRecord {
            id: stable_worktree_id(&request.destination),
            path: request.destination.clone(),
            head_oid: Some(base_oid.clone()),
            branch: Some(request.branch.full_ref()),
            detached: false,
            bare: false,
            lock: WorktreeLock {
                locked: false,
                reason: None,
            },
            prunable_reason: None,
        };
        let worktree = match self.list_worktrees(&request.repository, &reconciliation_token) {
            Ok(worktrees) => match worktrees
                .into_iter()
                .find(|worktree| worktree.path == request.destination)
            {
                Some(worktree) => worktree,
                None => {
                    warnings.push(ProcessFailure {
                        kind: ProcessFailureKind::Io,
                        program: self.git.clone(),
                        exit_code: Some(0),
                        signal: None,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                        message: "Git reported success but did not list the new worktree".into(),
                        started: true,
                    });
                    fallback
                }
            },
            Err(error) => {
                warnings.push(repository_warning(&self.git, error));
                fallback
            }
        };
        Ok(WorktreeCreationOutcome::Created { worktree, warnings })
    }

    pub fn create_worktree_command(
        &self,
        request: &CreateWorktreeRequest,
        base_oid: &str,
    ) -> Result<CommandSpec, ValidationError> {
        ValidatedBranchName::parse(request.branch.as_str())?;
        request.base.validate()?;
        validate_destination(&request.managed_root, &request.destination)?;
        if !is_hex_oid(base_oid) {
            return Err(ValidationError::InvalidBase(base_oid.into()));
        }
        Ok(self.git_mutation(
            &request.repository,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--lock"),
                OsString::from("--reason"),
                format!("cagents:{}", request.operation_id).into(),
                OsString::from("--no-track"),
                OsString::from("-b"),
                request.branch.as_str().into(),
                request.destination.as_os_str().to_owned(),
                base_oid.into(),
            ],
        ))
    }

    pub fn unlock_worktree_command(&self, request: &CreateWorktreeRequest) -> CommandSpec {
        self.git_mutation(
            &request.repository,
            [
                OsString::from("worktree"),
                OsString::from("unlock"),
                request.destination.as_os_str().to_owned(),
            ],
        )
    }

    pub fn set_upstream_command(
        &self,
        request: &CreateWorktreeRequest,
    ) -> Result<CommandSpec, ValidationError> {
        request.base.validate()?;
        let upstream = match &request.base {
            BaseRevision::LocalBranch(branch) => branch.full_ref(),
            BaseRevision::RemoteRef(reference) => reference.clone(),
            BaseRevision::ObjectId(_) => return Err(ValidationError::TrackingRequiresBranch),
        };
        Ok(self.git_mutation(
            &request.repository,
            [
                OsString::from("branch"),
                format!("--set-upstream-to={upstream}").into(),
                request.branch.as_str().into(),
            ],
        ))
    }

    fn reconcile_worktree(
        &self,
        request: &CreateWorktreeRequest,
        cancellation: &CancellationToken,
    ) -> Result<WorktreeResidualState, RepositoryError> {
        let worktrees = self.list_worktrees(&request.repository, cancellation)?;
        let branch_output = self.executor.execute(
            &self.git_read(
                &request.repository,
                [
                    OsString::from("show-ref"),
                    OsString::from("--verify"),
                    OsString::from("--quiet"),
                    request.branch.full_ref().into(),
                ],
            ),
            cancellation,
        )?;
        let branch_exists = match branch_output.exit_code {
            _ if branch_output.success => true,
            Some(1) => false,
            _ => return Err(branch_output.require_success(&self.git).unwrap_err().into()),
        };
        Ok(WorktreeResidualState {
            branch_exists,
            registered_worktree: worktrees
                .into_iter()
                .find(|worktree| worktree.path == request.destination),
            destination: inspect_destination(&request.destination),
        })
    }

    fn run_checked(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        self.executor
            .execute(command, cancellation)?
            .require_success(&command.program)
    }

    fn git_read<I, S>(&self, repository: &Path, arguments: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.git_base(repository)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(["-c", "core.fsmonitor=false"])
            .args(arguments)
    }

    fn git_mutation<I, S>(&self, repository: &Path, arguments: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.git_base(repository).args(arguments)
    }

    fn git_base(&self, repository: &Path) -> CommandSpec {
        CommandSpec::new(&self.git)
            .args([OsString::from("-C"), repository.as_os_str().to_owned()])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
    }
}

fn validate_destination(root: &Path, destination: &Path) -> Result<(), ValidationError> {
    let root = fs::canonicalize(root).map_err(|error| ValidationError::Io(error.to_string()))?;
    if !root.is_dir() {
        return Err(ValidationError::InvalidDestinationRoot);
    }
    if !destination.is_absolute() {
        return Err(ValidationError::DestinationOutsideRoot);
    }
    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(ValidationError::DestinationSymlink);
            }
            return Err(ValidationError::DestinationOccupied);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(ValidationError::Io(error.to_string())),
    }
    let parent = destination
        .parent()
        .ok_or(ValidationError::DestinationOutsideRoot)?;
    let parent = fs::canonicalize(parent).map_err(|_| ValidationError::DestinationOutsideRoot)?;
    if parent != root && !parent.starts_with(&root) {
        return Err(ValidationError::DestinationOutsideRoot);
    }
    if destination.file_name().is_none() {
        return Err(ValidationError::DestinationOutsideRoot);
    }
    Ok(())
}

fn inspect_destination(destination: &Path) -> DestinationResidualState {
    match fs::symlink_metadata(destination) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => DestinationResidualState::Absent,
        Err(_) => DestinationResidualState::Unknown,
        Ok(metadata) if metadata.file_type().is_symlink() => DestinationResidualState::Symlink,
        Ok(metadata) if metadata.is_file() => DestinationResidualState::File,
        Ok(metadata) if metadata.is_dir() => match fs::read_dir(destination) {
            Ok(mut entries) => {
                if entries.next().is_none() {
                    DestinationResidualState::EmptyDirectory
                } else {
                    DestinationResidualState::NonEmptyDirectory
                }
            }
            Err(_) => DestinationResidualState::Unknown,
        },
        Ok(_) => DestinationResidualState::Unknown,
    }
}

fn parse_bool_line(output: &[u8], field: &'static str) -> Result<bool, RepositoryError> {
    match parse_utf8_line(output, field)?.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        value => Err(RepositoryError::InvalidOutput {
            field,
            reason: format!("expected true or false, got {value:?}"),
        }),
    }
}

fn parse_path_line(output: &[u8], field: &'static str) -> Result<PathBuf, RepositoryError> {
    let value = trim_one_line_ending(output);
    if value.is_empty() {
        return Err(RepositoryError::InvalidOutput {
            field,
            reason: "path was empty".into(),
        });
    }
    Ok(bytes_to_path(value))
}

fn parse_utf8_line(output: &[u8], field: &'static str) -> Result<String, RepositoryError> {
    let output = trim_one_line_ending(output);
    std::str::from_utf8(output)
        .map(str::to_owned)
        .map_err(|error| RepositoryError::InvalidOutput {
            field,
            reason: error.to_string(),
        })
}

fn trim_one_line_ending(output: &[u8]) -> &[u8] {
    output
        .strip_suffix(b"\r\n")
        .or_else(|| output.strip_suffix(b"\n"))
        .unwrap_or(output)
}

fn attach_repository_process<T>(
    result: Result<T, RepositoryError>,
    process: &ProcessOutput,
) -> Result<T, RepositoryError> {
    result.map_err(|source| RepositoryError::InvalidResponse {
        source: Box::new(source),
        process: process.clone(),
    })
}

fn repository_warning(program: &Path, error: RepositoryError) -> ProcessFailure {
    match error {
        RepositoryError::Process(failure) => failure,
        RepositoryError::InvalidGitOutput { error, process } => ProcessFailure {
            kind: ProcessFailureKind::Io,
            program: program.to_path_buf(),
            exit_code: process.exit_code,
            signal: process.signal,
            stdout: process.stdout,
            stderr: process.stderr,
            message: error.to_string(),
            started: true,
        },
        RepositoryError::InvalidResponse { source, process } => ProcessFailure {
            kind: ProcessFailureKind::Io,
            program: program.to_path_buf(),
            exit_code: process.exit_code,
            signal: process.signal,
            stdout: process.stdout,
            stderr: process.stderr,
            message: source.to_string(),
            started: true,
        },
        error => ProcessFailure {
            kind: ProcessFailureKind::Io,
            program: program.to_path_buf(),
            exit_code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            message: error.to_string(),
            started: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsStr, sync::Mutex};

    #[derive(Default)]
    struct FakeExecutor {
        commands: Mutex<Vec<CommandSpec>>,
        outputs: Mutex<Vec<Result<ProcessOutput, ProcessFailure>>>,
    }

    impl FakeExecutor {
        fn with_outputs(outputs: Vec<Result<ProcessOutput, ProcessFailure>>) -> Self {
            Self {
                commands: Mutex::new(Vec::new()),
                outputs: Mutex::new(outputs.into_iter().rev().collect()),
            }
        }
    }

    impl ProcessExecutor for FakeExecutor {
        fn execute(
            &self,
            command: &CommandSpec,
            _cancellation: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            self.commands.lock().unwrap().push(command.clone());
            self.outputs.lock().unwrap().pop().unwrap()
        }
    }

    fn success(stdout: &[u8]) -> Result<ProcessOutput, ProcessFailure> {
        Ok(ProcessOutput {
            exit_code: Some(0),
            signal: None,
            success: true,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        })
    }

    #[test]
    fn command_specs_have_finite_resource_defaults() {
        let command = CommandSpec::new("git");
        assert_eq!(command.timeout, Some(DEFAULT_COMMAND_TIMEOUT));
        assert_eq!(command.stdout_limit, DEFAULT_STDOUT_LIMIT);
        assert_eq!(command.stderr_limit, DEFAULT_STDERR_LIMIT);
    }

    #[cfg(unix)]
    #[test]
    fn executor_preserves_bounded_stdout_stderr_and_stdin() {
        let command = CommandSpec::new("/bin/sh")
            .args(["-c", "cat; printf warning >&2"])
            .stdin(b"hello".to_vec())
            .output_limits(5, 7)
            .timeout(Duration::from_secs(2));
        let output = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap();

        assert!(output.success);
        assert_eq!(output.stdout, b"hello");
        assert_eq!(output.stderr, b"warning");
    }

    #[cfg(unix)]
    #[test]
    fn stdout_limit_kills_process_group_and_returns_bounded_excerpt() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("descendant-survived");
        let ready = directory.path().join("descendant-started");
        let command = CommandSpec::new("/bin/sh")
            .args([
                OsString::from("-c"),
                OsString::from(
                    "(trap '' TERM; sleep 0.5; printf survived > \"$MARKER\") & \
                     printf ready > \"$READY\"; while :; do printf 0123456789; done",
                ),
            ])
            .env("MARKER", marker.as_os_str())
            .env("READY", ready.as_os_str())
            .output_limits(FAILURE_EXCERPT_LIMIT + 1024, 1024)
            .timeout(Duration::from_secs(2));

        let failure = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap_err();

        assert_eq!(failure.kind, ProcessFailureKind::OutputLimit);
        assert_eq!(failure.stdout.len(), FAILURE_EXCERPT_LIMIT);
        assert!(failure.stderr.len() <= 1024);
        assert!(failure.message.contains("stdout exceeded"));
        assert!(ready.exists(), "fixture did not launch its descendant");
        thread::sleep(Duration::from_millis(600));
        assert!(!marker.exists(), "descendant survived output-limit kill");
    }

    #[cfg(unix)]
    #[test]
    fn stderr_has_an_independent_capture_limit() {
        let command = CommandSpec::new("/bin/sh")
            .args(["-c", "while :; do printf 0123456789 >&2; done"])
            .output_limits(1024, 137)
            .timeout(Duration::from_secs(2));

        let failure = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap_err();

        assert_eq!(failure.kind, ProcessFailureKind::OutputLimit);
        assert!(failure.stdout.len() <= 1024);
        assert_eq!(failure.stderr.len(), 137);
        assert!(failure.message.contains("stderr exceeded"));
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_process_group_and_reaps_the_child() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("descendant-survived");
        let ready = directory.path().join("descendant-started");
        let command = CommandSpec::new("/bin/sh")
            .args([
                OsString::from("-c"),
                OsString::from(
                    "(trap '' TERM; sleep 1; printf survived > \"$MARKER\") & \
                     printf ready > \"$READY\"; \
                     trap '' TERM; while :; do sleep 1; done",
                ),
            ])
            .env("MARKER", marker.as_os_str())
            .env("READY", ready.as_os_str())
            .timeout(Duration::from_millis(500));

        let failure = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap_err();

        assert_eq!(failure.kind, ProcessFailureKind::TimedOut);
        assert!(failure.message.contains("500ms"));
        assert!(ready.exists(), "fixture did not launch its descendant");
        thread::sleep(Duration::from_millis(700));
        assert!(!marker.exists(), "descendant survived timeout kill");
    }

    #[cfg(unix)]
    #[test]
    fn a_quick_command_returns_without_any_polling_latency() {
        // The old 15 ms poll interval cost at least one sleep per invocation, so
        // twenty of these could not have come in under ~300 ms. The wait now
        // blocks on the child, so the only cost is fork/exec.
        let command = CommandSpec::new("/bin/sh")
            .args(["-c", "exit 0"])
            .timeout(Duration::from_secs(5));
        let executor = SystemProcessExecutor;
        let cancellation = CancellationToken::new();

        let started = Instant::now();
        for _ in 0..20 {
            assert!(executor.execute(&command, &cancellation).unwrap().success);
        }
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(250),
            "20 trivial commands took {elapsed:?}, which suggests a polling wait"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_interrupts_a_blocked_wait_immediately() {
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("started");
        let command = CommandSpec::new("/bin/sh")
            .args([
                OsString::from("-c"),
                OsString::from("printf ready > \"$READY\"; while :; do sleep 1; done"),
            ])
            .env("READY", ready.as_os_str())
            // Long enough that a timeout cannot be what ends this test.
            .timeout(Duration::from_secs(30));

        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        let waiter = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ready.exists() {
                assert!(Instant::now() < deadline, "fixture never started");
                thread::sleep(Duration::from_millis(5));
            }
            canceller.cancel();
        });

        let started = Instant::now();
        let failure = SystemProcessExecutor
            .execute(&command, &cancellation)
            .unwrap_err();
        let elapsed = started.elapsed();
        waiter.join().unwrap();

        assert_eq!(failure.kind, ProcessFailureKind::Cancelled);
        assert!(failure.started);
        assert!(
            elapsed < Duration::from_secs(2),
            "cancellation took {elapsed:?}; the wait did not wake on it"
        );
    }

    #[test]
    fn cancellation_wakers_do_not_accumulate_on_a_reused_token() {
        let cancellation = CancellationToken::new();
        let command = CommandSpec::new("/bin/sh")
            .args(["-c", "exit 0"])
            .timeout(Duration::from_secs(5));
        for _ in 0..3 {
            SystemProcessExecutor
                .execute(&command, &cancellation)
                .unwrap();
        }
        assert!(lock_wakers(&cancellation.0).is_empty());
    }

    #[test]
    fn parses_worktree_porcelain_and_ignores_future_fields() {
        let input = b"worktree /repo\0HEAD 0123456789012345678901234567890123456789\0branch refs/heads/main\0locked reason\0future value\0\0worktree /repo/wt\0HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0detached\0prunable missing\0\0";
        let records = parse_worktree_list(input).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].path, PathBuf::from("/repo"));
        assert_eq!(records[0].branch.as_deref(), Some("refs/heads/main"));
        assert_eq!(records[0].lock.reason.as_deref(), Some("reason"));
        assert!(records[1].detached);
        assert_eq!(records[1].prunable_reason.as_deref(), Some("missing"));
    }

    #[test]
    fn rejects_truncated_worktree_porcelain() {
        assert!(matches!(
            parse_worktree_list(b"worktree /repo"),
            Err(GitParseError::Truncated { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_worktree_paths() {
        use std::os::unix::ffi::OsStrExt;

        let mut input = b"worktree /repo/".to_vec();
        input.push(0xff);
        input.extend_from_slice(b"\0HEAD 0123456789012345678901234567890123456789\0\0");
        let records = parse_worktree_list(&input).unwrap();
        assert_eq!(records[0].path.as_os_str().as_bytes().last(), Some(&0xff));
    }

    #[test]
    fn parses_status_headers_changes_conflict_and_rename_source() {
        let input = b"# branch.oid 0123456789012345678901234567890123456789\0# branch.head feature\0# branch.upstream origin/feature\0# branch.ab +2 -3\x001 M. N... 100644 100644 100644 a b file\x002 .M S... 100644 100644 100644 a b R100 new name\0old name\0u UU N... 100644 100644 100644 100644 a b c conflict\0? untracked\0";
        let status = parse_status(input).unwrap();
        assert!(matches!(status.head, HeadState::Branch { ref name, .. } if name == "feature"));
        assert_eq!(status.dirty.staged, 1);
        assert_eq!(status.dirty.unstaged, 1);
        assert_eq!(status.dirty.conflicted, 1);
        assert_eq!(status.dirty.untracked, 1);
        assert_eq!(status.dirty.submodule, 1);
        assert!(matches!(
            status.tracking,
            TrackingState::Compared {
                ahead: 2,
                behind: 3,
                ..
            }
        ));
    }

    #[test]
    fn rejects_missing_rename_source_and_malformed_ab() {
        assert!(parse_status(b"2 R. N... 100644 100644 100644 a b R100 path\0").is_err());
        assert!(parse_status(b"# branch.ab +x -2\0").is_err());
        assert!(matches!(
            parse_status(b"? path"),
            Err(GitParseError::Truncated { .. })
        ));
        assert!(parse_status(&[]).is_err());
    }

    #[test]
    fn status_command_is_shell_free_and_explicit() {
        let service = RepositoryService::new("/usr/bin/git", Arc::new(FakeExecutor::default()));
        let command = service.status_command(Path::new("/tmp/repo; touch /tmp/pwned"));
        assert_eq!(command.program, PathBuf::from("/usr/bin/git"));
        assert_eq!(command.arguments[0], OsString::from("-C"));
        assert_eq!(
            command.arguments[1],
            OsString::from("/tmp/repo; touch /tmp/pwned")
        );
        assert!(
            !command
                .arguments
                .iter()
                .any(|argument| argument == OsStr::new("sh"))
        );
    }

    #[test]
    fn discovery_uses_individual_path_queries() {
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            success(b"false\n"),
            success(b"/repo/.git\n"),
            success(b"/repo/.git\n"),
            success(b"/repo\n"),
        ]));
        let service = RepositoryService::new("git", fake.clone());
        let repository = service
            .discover(Path::new("/repo/subdir"), &CancellationToken::new())
            .unwrap();
        assert_eq!(repository.common_dir, PathBuf::from("/repo/.git"));
        assert_eq!(fake.commands.lock().unwrap().len(), 4);
    }

    #[test]
    fn validates_branch_names_without_normalizing_them() {
        assert_eq!(
            ValidatedBranchName::parse("feature/safe").unwrap().as_str(),
            "feature/safe"
        );
        for invalid in ["-force", "bad name", "a..b", "a.lock", "a@{b", "a//b"] {
            assert!(ValidatedBranchName::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn create_command_keeps_untrusted_values_in_single_arguments() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("feature; echo nope");
        let request = CreateWorktreeRequest {
            repository: PathBuf::from("/repo"),
            managed_root: root.path().into(),
            destination: destination.clone(),
            branch: ValidatedBranchName::parse("feature/safe").unwrap(),
            base: BaseRevision::object_id("a".repeat(40)).unwrap(),
            track: false,
            operation_id: OperationId::from_uuid(Uuid::nil()),
        };
        let service = RepositoryService::new("git", Arc::new(FakeExecutor::default()));
        let plan = service.plan_worktree_creation(&request).unwrap();
        assert_eq!(
            plan.resolve_base.arguments.last().unwrap(),
            &OsString::from(format!("{}^{{commit}}", "a".repeat(40)))
        );
        let command = service
            .create_worktree_command(&request, &"a".repeat(40))
            .unwrap();
        assert!(command.arguments.contains(&destination.into_os_string()));
        assert!(
            !command
                .arguments
                .iter()
                .any(|argument| argument == OsStr::new("sh"))
        );
    }

    #[test]
    fn tracking_is_configured_after_creation_from_the_validated_ref() {
        let root = tempfile::tempdir().unwrap();
        let request = CreateWorktreeRequest {
            repository: PathBuf::from("/repo"),
            managed_root: root.path().into(),
            destination: root.path().join("feature"),
            branch: ValidatedBranchName::parse("feature").unwrap(),
            base: BaseRevision::remote_ref("refs/remotes/origin/main").unwrap(),
            track: true,
            operation_id: OperationId::from_uuid(Uuid::nil()),
        };
        let service = RepositoryService::new("git", Arc::new(FakeExecutor::default()));
        let create = service
            .create_worktree_command(&request, &"a".repeat(40))
            .unwrap();
        assert!(create.arguments.contains(&OsString::from("--no-track")));

        let tracking = service.set_upstream_command(&request).unwrap();
        assert!(tracking.arguments.contains(&OsString::from(
            "--set-upstream-to=refs/remotes/origin/main"
        )));
        assert_eq!(tracking.arguments.last(), Some(&OsString::from("feature")));
    }

    #[test]
    fn cancelled_worktree_creation_reconciles_to_unknown_when_branch_exists() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("feature");
        let cancelled = ProcessFailure {
            kind: ProcessFailureKind::Cancelled,
            program: "git".into(),
            exit_code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: b"cancelled".into(),
            message: "cancelled".into(),
            started: true,
        };
        let absent = Ok(ProcessOutput {
            exit_code: Some(1),
            signal: None,
            success: false,
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
        let branch_exists = success(&[]);
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            success(&[]),
            success(format!("{}\n", "a".repeat(40)).as_bytes()),
            absent,
            Err(cancelled.clone()),
            success(&[]),
            branch_exists,
        ]));
        let service = RepositoryService::new("git", fake);
        let request = CreateWorktreeRequest {
            repository: PathBuf::from("/repo"),
            managed_root: root.path().into(),
            destination,
            branch: ValidatedBranchName::parse("feature").unwrap(),
            base: BaseRevision::object_id("a".repeat(40)).unwrap(),
            track: false,
            operation_id: OperationId::from_uuid(Uuid::nil()),
        };
        let outcome = service
            .create_worktree(&request, &CancellationToken::new())
            .unwrap();
        assert!(matches!(
            outcome,
            WorktreeCreationOutcome::OutcomeUnknown {
                residual: WorktreeResidualState {
                    branch_exists: true,
                    ..
                },
                ..
            }
        ));
    }
}
