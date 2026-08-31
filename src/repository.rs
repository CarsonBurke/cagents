//! Git repository and worktree services.
//!
//! The service is deliberately synchronous: callers should place potentially slow
//! operations on `spawn_blocking`.  Commands are represented as data before they are
//! executed, which keeps argument handling shell-free and makes every operation
//! straightforward to test with a fake executor.

use std::{
    borrow::Cow,
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{self, Read, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::fs_safety;
use crate::model::{OperationId, WorktreeId};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// How long a terminated process group is given to exit on SIGTERM before it
/// is killed outright.
///
/// This and the two signal numbers below are crate-internal rather than
/// public: `crate::lsp` terminates language-server process groups through the
/// same primitives so that the crate keeps exactly one implementation of
/// process-group signalling, and therefore exactly one `unsafe` block.
pub(crate) const TERMINATION_GRACE: Duration = Duration::from_millis(150);
pub(crate) const SIGTERM: i32 = 15;
pub(crate) const SIGKILL: i32 = 9;
/// How long the executor waits, after a child has been terminated, for the
/// pipes to drain and for the kernel to report the exit status, before it
/// interrupts its reader threads and returns without one.
///
/// A bound is required rather than merely nice: a hook can `setsid` a
/// background process that escapes the killed process group while still
/// holding the child's stdout or stderr, and the caller of a mutation holds
/// that repository's [`MutationLocks`] guard for the whole call.
const CLEANUP_GRACE: Duration = Duration::from_millis(500);
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_STDOUT_LIMIT: usize = 16 * 1024 * 1024;
const DEFAULT_STDERR_LIMIT: usize = 4 * 1024 * 1024;
const FAILURE_EXCERPT_LIMIT: usize = 64 * 1024;
const ZERO_SHA1: &str = "0000000000000000000000000000000000000000";

/// Commits returned by one [`RepositoryService::history`] page.
pub const HISTORY_PAGE_SIZE: u32 = 200;

/// Largest single side of a comparison that is read into memory as text.
///
/// It matches `diff_view::MAX_TEXT_DIFF_BYTES_PER_SIDE`, so a side this service
/// hands over as [`DiffSide::Text`] is always a side the diff engine will
/// actually align instead of rejecting as oversize.
pub const MAX_DIFF_SIDE_BYTES: u64 = 16 * 1024 * 1024;

/// Bytes inspected when deciding whether content is binary. Git uses the same
/// leading-window heuristic, so a file this service calls binary is a file Git
/// also refuses to diff as text.
const BINARY_SNIFF_BYTES: usize = 8000;

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

/// How much of one stderr line a notice is given, in characters.
///
/// A notice is one line in a surface of fixed height, and a process relays
/// whatever the server or the remote hook said, which can run to a paragraph.
pub const FAILURE_DETAIL_LIMIT: usize = 200;

impl ProcessFailure {
    /// The process's own first word on why it failed: the first non-empty line
    /// of stderr, trimmed and bounded.
    ///
    /// A status code alone is not something anyone can act on, and everything
    /// that is — a GraphQL error naming the repository `gh` could not resolve,
    /// a missing scope, a rate limit, `fatal: not a git repository` — the
    /// process writes here, where [`Display`](fmt::Display) then replaces it
    /// with `{program}: {message}`. `None` means the process said nothing at
    /// all, which is the only case in which the exit status is the best there
    /// is.
    ///
    /// One line, because a process's stderr is regularly several: `gh`'s
    /// unauthenticated message alone is two sentences, and pasting a paragraph
    /// into a one-line notice shows the user neither.
    ///
    /// A method on the failure rather than a free function per caller: the
    /// `gh` surface and the `git` surface had a character-for-character
    /// identical copy each, with a bound apiece that nothing kept equal.
    ///
    /// Raw, and it stays raw: this answers what the process wrote, not what a
    /// notice should say. Whether those words describe the fault at all
    /// depends on [`Self::kind`], and that judgement lives in
    /// [`Self::reported_detail`] - a caller reading stderr to recognise a
    /// sentence it knows, as `github::is_no_checks` does, must not have that
    /// judgement applied behind its back.
    ///
    /// The bound is taken at a character boundary, because stderr is arbitrary
    /// bytes and `char_indices().nth(n)` yields the start of the (n+1)-th
    /// character - never the middle of one.
    pub fn stderr_line(&self) -> Option<String> {
        let text = String::from_utf8_lossy(&self.stderr);
        let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
        Some(match line.char_indices().nth(FAILURE_DETAIL_LIMIT) {
            Some((index, _)) => format!("{}\u{2026}", &line[..index]),
            None => line.to_owned(),
        })
    }

    /// What to report about this failure, in whichever words actually describe
    /// it.
    ///
    /// [`ProcessFailureKind::Exit`] is the one kind whose own `message` says
    /// nothing: `Display` renders it as `{program}: process exited with status
    /// Some(4)`, a number standing in for the two sentences `gh` wrote about
    /// how to log in, or for `fatal: not a git repository`. So it is the one
    /// kind whose stderr is preferred, and only when there is some.
    ///
    /// Every other kind is named by this program rather than by the process: a
    /// timeout, a cancellation, a spawn that never happened, an output limit,
    /// a wait that failed. Each of those carries captured stderr too, and
    /// quoting it would replace the only description of the fault with
    /// whatever the process last said before it - a timed-out `gh pr create`
    /// reported as an unrelated progress line, a `git fetch` this program
    /// killed on its deadline reported as `Receiving objects:  73% (…)`, and a
    /// git that never ran reported as whatever a previous one left behind.
    ///
    /// The guard is here rather than at the callers because it is not a
    /// property of either surface. It was written once, for `gh`, while the
    /// `git` caller quoted stderr for all six kinds under a comment claiming
    /// this very rule; one helper holding the whole rule is what stops the two
    /// from drifting apart again.
    pub fn reported_detail(&self) -> String {
        if self.kind != ProcessFailureKind::Exit {
            return self.to_string();
        }
        self.stderr_line().unwrap_or_else(|| self.to_string())
    }
}

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
        CancellationWaker { state: &self.0, id }
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

        // Interrupting a thread blocked on a pipe needs a descriptor that
        // thread can poll alongside the pipe. Closing `stop_signal` makes
        // `poll` report `POLLHUP` on `stop`, so the thread wakes immediately,
        // with no timer and no polling interval.
        let (stop, stop_signal) = match stop_pipe() {
            Ok(pair) => pair,
            Err(error) => {
                let status = terminate_process_tree(child);
                return Err(ProcessFailure {
                    kind: ProcessFailureKind::Io,
                    program: spec.program.clone(),
                    exit_code: status.as_ref().and_then(ExitStatus::code),
                    signal: status.as_ref().and_then(exit_signal),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    message: format!("interrupt pipe was unavailable: {error}"),
                    started: true,
                });
            }
        };
        let stop = Arc::new(stop);

        let stdout_exceeded = Arc::new(AtomicBool::new(false));
        let stdout_reader = {
            let exceeded = Arc::clone(&stdout_exceeded);
            let events = events.clone();
            let limit = spec.stdout_limit;
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let result = read_pipe(stdout, limit, &exceeded, &stop);
                let _ = events.send(WaitEvent::Stdout);
                result
            })
        };
        let stderr_exceeded = Arc::new(AtomicBool::new(false));
        let stderr_reader = {
            let exceeded = Arc::clone(&stderr_exceeded);
            let events = events.clone();
            let limit = spec.stderr_limit;
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let result = read_pipe(stderr, limit, &exceeded, &stop);
                let _ = events.send(WaitEvent::Stderr);
                result
            })
        };

        let mut stdin_done = spec.stdin.is_none();
        let stdin_writer = match (spec.stdin.clone(), child.stdin.take()) {
            (Some(bytes), Some(stdin)) => {
                let events = events.clone();
                let stop = Arc::clone(&stop);
                Some(thread::spawn(move || {
                    let result = write_stdin(stdin, &bytes, &stop);
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

        // Bounded cleanup. Every path that breaks out of the loop early has
        // already signalled the child's process group, but a process the hook
        // chain left *outside* that group can still hold a pipe open. So the
        // readers get a short grace to drain what was already written, and are
        // then interrupted. Without that interrupt the joins below would never
        // return, and the repository mutation lock the caller holds around this
        // call would never be released.
        let cleanup_deadline = Instant::now() + CLEANUP_GRACE;
        while !(stdout_done && stderr_done && stdin_done) {
            let remaining = cleanup_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match event_source.recv_timeout(remaining) {
                Ok(WaitEvent::Stdout) => stdout_done = true,
                Ok(WaitEvent::Stderr) => stderr_done = true,
                Ok(WaitEvent::Stdin) => stdin_done = true,
                Ok(WaitEvent::Exited) if status.is_none() => {
                    status = exit_source
                        .recv_timeout(remaining)
                        .ok()
                        .and_then(Result::ok);
                }
                Ok(WaitEvent::Exited | WaitEvent::Cancelled) => {}
                Err(_) => break,
            }
        }
        // Wakes both readers and the stdin writer out of `poll`, which is what
        // bounds all three joins below.
        drop(stop_signal);

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
        // Joined only once the child has actually been reaped. When the kernel
        // has not reported the exit within the grace above, this leaves the
        // waiter thread running and the child unreaped until that exit finally
        // arrives: one thread and one zombie per command that reaches this
        // state, which is a bounded leak rather than a free choice. It is the
        // deliberate price of not reintroducing the unbounded wait this
        // cleanup exists to remove — the thread owns nothing but the `Child`
        // and ends by itself at the reap.
        if status.is_some() {
            let _ = waiter.join();
        }

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
/// live in the thread that blocks on `wait`. Crate-internal so that
/// `crate::lsp` reuses this single implementation — and its single `unsafe`
/// block — instead of declaring `kill` a second time; it is not public API.
#[cfg(unix)]
pub(crate) fn signal_process_group(process_group: u32, signal: i32) {
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
pub(crate) fn signal_process_group(_process_group: u32, _signal: i32) {}

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
    // Bounded: a killed process is normally reaped at once, but a task parked
    // in an uninterruptible kernel wait would otherwise hold this caller — and
    // the repository mutation lock above it — for as long as it lasted.
    status.or_else(|| {
        exit_source
            .recv_timeout(CLEANUP_GRACE)
            .ok()
            .and_then(Result::ok)
    })
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
    // Joined only once the child was reaped, for the reason given above.
    if status.is_some() {
        let _ = waiter.join();
    }
    status
}

struct PipeRead {
    output: Vec<u8>,
    limit_exceeded: bool,
}

/// A descriptor pair whose only purpose is to interrupt a thread blocked on a
/// pipe, returned as `(read, write)`.
///
/// `poll` reports `POLLHUP` on the read end once every write end is closed, and
/// reports it whether or not the caller asked for any event, so dropping the
/// write end is a complete wakeup that carries no data and needs no timer.
/// `O_CLOEXEC` keeps the write end out of the child, which would otherwise hold
/// the hangup off for as long as the child — or anything it spawned — lived.
fn stop_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1_i32; 2];
    // SAFETY: `descriptors` is a live two-element array of `c_int`, which is
    // exactly the buffer `pipe2` writes its read and write ends into.
    let created = unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) };
    if created < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe2` reported success, so both descriptors are freshly opened
    // and owned by this process alone; taking ownership cannot double-close.
    unsafe {
        Ok((
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        ))
    }
}

/// Puts `descriptor` into non-blocking mode, so a read or write reports
/// `WouldBlock` instead of parking uninterruptibly inside the kernel.
fn set_nonblocking(descriptor: BorrowedFd<'_>) -> io::Result<()> {
    let raw = descriptor.as_raw_fd();
    // SAFETY: the descriptor is borrowed for the whole call, so it cannot be
    // closed underneath it, and `F_GETFL` takes no further argument.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above, with the `int` argument `F_SETFL` requires.
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Blocks until `subject` is ready for `events`, or until `stop` is signalled.
///
/// `false` means the wait was interrupted and the caller must not touch
/// `subject` again.
fn wait_until_ready(
    subject: BorrowedFd<'_>,
    events: i16,
    stop: BorrowedFd<'_>,
) -> io::Result<bool> {
    let mut watched = [
        libc::pollfd {
            fd: subject.as_raw_fd(),
            events,
            revents: 0,
        },
        // No requested events: a hangup is reported regardless, and a hangup is
        // the only thing this descriptor ever produces.
        libc::pollfd {
            fd: stop.as_raw_fd(),
            events: 0,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: `watched` is a fully initialised two-element array that
        // outlives the call, and both descriptors are borrowed for its
        // duration, so neither can be closed while the kernel inspects them.
        let ready = unsafe { libc::poll(watched.as_mut_ptr(), watched.len() as libc::nfds_t, -1) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        // Checked first: once cleanup has begun there is no reason to keep
        // draining, even if both descriptors woke together.
        if watched[1].revents != 0 {
            return Ok(false);
        }
        if watched[0].revents != 0 {
            return Ok(true);
        }
    }
}

fn read_pipe(
    mut pipe: impl Read + AsFd,
    limit: usize,
    limit_exceeded: &AtomicBool,
    stop: &OwnedFd,
) -> io::Result<PipeRead> {
    set_nonblocking(pipe.as_fd())?;
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
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if wait_until_ready(pipe.as_fd(), libc::POLLIN, stop.as_fd())? {
                    continue;
                }
                // Interrupted. Whatever still holds the write end is outside
                // this executor's reach — a process the hook chain put in
                // another session — and the caller is already returning the
                // failure that explains why the command was abandoned.
                return Ok(PipeRead {
                    output,
                    limit_exceeded: false,
                });
            }
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

/// Writes `bytes` to a child's stdin and closes it, interruptibly.
///
/// A child that exits without reading leaves this reporting `BrokenPipe`, which
/// is the caller's business. The interrupt exists for the other case: a
/// background process that inherited the read end and never reads from it,
/// against which a blocking `write_all` has no defence at all.
fn write_stdin(
    mut stdin: std::process::ChildStdin,
    bytes: &[u8],
    stop: &OwnedFd,
) -> io::Result<()> {
    set_nonblocking(stdin.as_fd())?;
    let mut written = 0;
    while written < bytes.len() {
        match stdin.write(&bytes[written..]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if !wait_until_ready(stdin.as_fd(), libc::POLLOUT, stop.as_fd())? {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "interrupted before the whole input was written",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    stdin.flush()
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

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HeadState {
    Unborn {
        branch: Option<String>,
    },
    Branch {
        name: String,
        oid: String,
    },
    Detached {
        oid: String,
    },
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrackingState {
    #[default]
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

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RepoPathError {
    #[error("repository path is empty")]
    Empty,
    #[error("repository path must be relative: {path}")]
    NotRelative { path: String },
    #[error("repository path has an empty component: {path}")]
    EmptyComponent { path: String },
    #[error("repository path may not contain a {component:?} component: {path}")]
    InvalidComponent { path: String, component: String },
    #[error("repository path contains a NUL byte")]
    InteriorNul,
    #[error("repository paths require Unix byte paths")]
    UnsupportedPlatform,
}

/// A worktree-relative path, byte-identical to what Git reported.
///
/// Git speaks bytes: a filename is an arbitrary NUL-free byte string, and the
/// same bytes must come back unchanged when the path is used as a pathspec.
/// Round-tripping through `String` would replace an undecodable byte with
/// U+FFFD and silently select a *different* file, so this type stores the
/// `OsString` and offers a lossy rendering only for display. Construction
/// rejects anything that could name something outside the worktree, which is
/// what makes [`RepoPath::resolve`] safe to hand to the filesystem.
///
/// Git marks a directory it will not look inside — an untracked nested
/// repository — with a trailing separator, in both `status` and `ls-files`.
/// That single trailing separator carries no path information, so it is
/// canonicalized away here and such an entry names the directory itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPath(OsString);

impl RepoPath {
    /// Validates raw Git path bytes.
    #[cfg(unix)]
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RepoPathError> {
        use std::os::unix::ffi::OsStringExt;

        let bytes = canonical_repo_path_bytes(bytes)?;
        Ok(Self(OsString::from_vec(bytes.to_vec())))
    }

    /// Non-Unix targets have no byte-exact path representation, so a path is
    /// rejected outright rather than transcoded into a different filename.
    #[cfg(not(unix))]
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RepoPathError> {
        canonical_repo_path_bytes(bytes)?;
        Err(RepoPathError::UnsupportedPlatform)
    }

    pub fn parse(value: impl AsRef<OsStr>) -> Result<Self, RepoPathError> {
        Self::from_bytes(value.as_ref().as_encoded_bytes())
    }

    /// The exact bytes Git reported. On Unix these are the filesystem bytes;
    /// no other target can construct a `RepoPath`.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_encoded_bytes()
    }

    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    /// A lossy rendering for labels and accessibility strings only. Never feed
    /// this back to Git or the filesystem.
    pub fn display(&self) -> Cow<'_, str> {
        self.0.to_string_lossy()
    }

    /// The final path component, lossily rendered for display.
    pub fn file_name(&self) -> Cow<'_, str> {
        self.as_path()
            .file_name()
            .map(OsStr::to_string_lossy)
            .unwrap_or(Cow::Borrowed(""))
    }

    /// The lowercase extension, or `None` when there is none or it is not
    /// UTF-8. Language selection compares against UTF-8 extension lists, so an
    /// undecodable extension must not match one by accident.
    pub fn extension(&self) -> Option<String> {
        self.as_path()
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
    }

    /// The absolute path of this entry inside `worktree`.
    pub fn resolve(&self, worktree: &Path) -> PathBuf {
        worktree.join(self.as_path())
    }

    /// Slash-separated components, as bytes.
    pub fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.as_bytes().split(|byte| *byte == b'/')
    }

    /// The containing directory, or `None` for a top-level entry.
    pub fn parent(&self) -> Option<Self> {
        let bytes = self.as_bytes();
        let separator = bytes.iter().rposition(|byte| *byte == b'/')?;
        Self::from_bytes(&bytes[..separator]).ok()
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.display())
    }
}

/// Serialized as raw bytes through `serde_bytes`, not as a `PathBuf`: a
/// `PathBuf` field would force the value through `str` in text formats and
/// corrupt any filename that is not UTF-8.
impl Serialize for RepoPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde_bytes::Bytes::new(self.as_bytes()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = serde_bytes::ByteBuf::deserialize(deserializer)?;
        Self::from_bytes(bytes.as_ref()).map_err(serde::de::Error::custom)
    }
}

/// Validates Git path bytes and returns the canonical form: exactly one
/// trailing separator, which is how Git names a directory, is removed. A
/// second one is still an empty component and is rejected, so only the form
/// Git actually emits is accepted.
fn canonical_repo_path_bytes(bytes: &[u8]) -> Result<&[u8], RepoPathError> {
    let canonical = match bytes.split_last() {
        Some((b'/', rest)) => rest,
        _ => bytes,
    };
    if canonical.is_empty() {
        return Err(RepoPathError::Empty);
    }
    if canonical.contains(&0) {
        return Err(RepoPathError::InteriorNul);
    }
    let rendering = || String::from_utf8_lossy(canonical).into_owned();
    if canonical[0] == b'/' {
        return Err(RepoPathError::NotRelative { path: rendering() });
    }
    // Validated on raw bytes rather than through `Path::components`, which
    // normalises `.` and repeated separators away instead of reporting them.
    for component in canonical.split(|byte| *byte == b'/') {
        match component {
            b"" => return Err(RepoPathError::EmptyComponent { path: rendering() }),
            b"." | b".." => {
                return Err(RepoPathError::InvalidComponent {
                    path: rendering(),
                    component: String::from_utf8_lossy(component).into_owned(),
                });
            }
            _ => {}
        }
    }
    Ok(canonical)
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OidError {
    #[error("object id must be 40 or 64 lowercase hexadecimal characters: {0}")]
    Invalid(String),
    #[error("the null object id does not name an object")]
    Null,
}

/// A full Git object id.
///
/// Lowercase hex of SHA-1 or SHA-256 length; Git never prints anything else for
/// `%H`, `rev-parse`, or `for-each-ref`, so accepting anything else would mean
/// passing an unvalidated string back as a revision argument.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Oid(String);

impl Oid {
    pub fn parse(value: impl Into<String>) -> Result<Self, OidError> {
        let value = value.into();
        let valid = matches!(value.len(), 40 | 64)
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
        if valid {
            Ok(Self(value))
        } else {
            Err(OidError::Invalid(value))
        }
    }

    /// Parses an id that must name a real object, rejecting the all-zero id
    /// Git prints for "no object".
    pub fn parse_object(value: impl Into<String>) -> Result<Self, OidError> {
        let oid = Self::parse(value)?;
        if oid.is_null() {
            Err(OidError::Null)
        } else {
            Ok(oid)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_null(&self) -> bool {
        self.0.bytes().all(|byte| byte == b'0')
    }

    /// An abbreviated id for labels. Hex is ASCII, so this never splits a
    /// character.
    pub fn short(&self) -> &str {
        &self.0[..8.min(self.0.len())]
    }
}

impl TryFrom<String> for Oid {
    type Error = OidError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<Oid> for String {
    fn from(value: Oid) -> Self {
        value.0
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A fully qualified ref name such as `refs/heads/main` or
/// `refs/remotes/origin/main`.
///
/// Comparison bases arrive from GitHub metadata and user selection, so the
/// value that reaches a revision argument must be validated: an unqualified
/// name is ambiguous between a branch, a tag, and a path, and a leading `-`
/// would be read as an option.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ValidatedRefName(String);

impl ValidatedRefName {
    pub fn parse(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        match value.strip_prefix("refs/") {
            Some(rest) if ValidatedBranchName::parse(rest).is_ok() => Ok(Self(value)),
            _ => Err(ValidationError::InvalidBase(value)),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name without its `refs/heads/` or `refs/remotes/` prefix, for
    /// display only.
    pub fn short_name(&self) -> &str {
        self.0
            .strip_prefix("refs/heads/")
            .or_else(|| self.0.strip_prefix("refs/remotes/"))
            .or_else(|| self.0.strip_prefix("refs/tags/"))
            .unwrap_or(&self.0)
    }
}

impl TryFrom<String> for ValidatedRefName {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ValidatedRefName> for String {
    fn from(value: ValidatedRefName) -> Self {
        value.0
    }
}

impl fmt::Display for ValidatedRefName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Which of Git's three views of a path a change belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeArea {
    /// HEAD to index.
    Staged,
    /// Index to worktree.
    Unstaged,
    Untracked,
    Conflict,
}

/// One porcelain-v2 status code.
///
/// `Unknown` keeps a future Git status letter from failing the whole snapshot;
/// every code Git documents today has a named variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Ignored,
    Unmerged,
    Unknown(char),
}

/// The seven unmerged code pairs Git reports in a porcelain-v2 `u` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// `DD`
    BothDeleted,
    /// `AU`
    AddedByUs,
    /// `UD`
    DeletedByThem,
    /// `UA`
    AddedByThem,
    /// `DU`
    DeletedByUs,
    /// `AA`
    BothAdded,
    /// `UU`
    BothModified,
}

/// One changed path in one area.
///
/// A path that is modified in the index *and* again in the worktree produces
/// two of these, because staging, discarding, and diffing all act on one area
/// at a time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedPath {
    pub path: RepoPath,
    /// The rename or copy source, on the entry whose kind is a rename or copy.
    pub old_path: Option<RepoPath>,
    pub area: ChangeArea,
    pub kind: ChangeKind,
    pub conflict: Option<ConflictKind>,
    pub submodule: bool,
    /// `None` until a text diff has actually been measured; binary, conflicted,
    /// and untracked paths have no line counts.
    pub added_lines: Option<u64>,
    pub removed_lines: Option<u64>,
}

/// One path in a committed comparison (branch or commit diff), which has no
/// index/worktree area and cannot be conflicted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    pub path: RepoPath,
    pub old_path: Option<RepoPath>,
    pub kind: ChangeKind,
    pub added_lines: Option<u64>,
    pub removed_lines: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub head: HeadState,
    pub tracking: TrackingState,
    pub dirty: DirtyState,
    /// Every changed path, one entry per area. `dirty` stays derived from the
    /// same records so existing badges keep counting exactly what they did
    /// before this field existed.
    #[serde(default)]
    pub entries: Vec<ChangedPath>,
}

impl StatusSnapshot {
    /// Entries in one area, in Git's reporting order.
    pub fn area(&self, area: ChangeArea) -> impl Iterator<Item = &ChangedPath> {
        self.entries.iter().filter(move |entry| entry.area == area)
    }

    /// The checked-out branch, or `None` when HEAD is detached or unknown.
    pub fn branch(&self) -> Option<&str> {
        match &self.head {
            HeadState::Branch { name, .. } => Some(name),
            HeadState::Unborn { branch } => branch.as_deref(),
            HeadState::Detached { .. } | HeadState::Unknown => None,
        }
    }
}

/// One row of the synthesized file tree.
///
/// Directory rows exist only in memory: they are derived from the paths Git
/// listed, never from reading the filesystem, so no symlinked directory can be
/// walked into and no ignored subtree can appear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTreeEntry {
    pub path: RepoPath,
    /// Number of ancestors, for indentation in a flat virtualized list.
    pub depth: usize,
    pub directory: bool,
}

#[derive(Debug, Default)]
struct FileTreeNode {
    children: BTreeMap<Vec<u8>, FileTreeNode>,
    file: bool,
}

/// Expands a flat path list into a pre-order tree of directory and file rows,
/// directories first and byte-sorted within each level.
pub fn build_file_tree(paths: &[RepoPath]) -> Vec<FileTreeEntry> {
    let mut root = FileTreeNode::default();
    for path in paths {
        let mut node = &mut root;
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            node = node.children.entry(component.to_vec()).or_default();
            if components.peek().is_none() {
                node.file = true;
            }
        }
    }
    let mut rows = Vec::with_capacity(paths.len());
    emit_file_tree(&root, &mut Vec::new(), 0, &mut rows);
    rows
}

fn emit_file_tree(
    node: &FileTreeNode,
    prefix: &mut Vec<u8>,
    depth: usize,
    rows: &mut Vec<FileTreeEntry>,
) {
    for directory in [true, false] {
        for (name, child) in
            node.children.iter().filter(
                |(_, child)| {
                    if directory { !child.file } else { child.file }
                },
            )
        {
            let restore = prefix.len();
            if !prefix.is_empty() {
                prefix.push(b'/');
            }
            prefix.extend_from_slice(name);
            // Every component came from an already validated `RepoPath`, so a
            // rejection here is impossible; skipping rather than panicking
            // keeps a future validation change from taking the tree down.
            if let Ok(path) = RepoPath::from_bytes(prefix) {
                rows.push(FileTreeEntry {
                    path,
                    depth,
                    directory,
                });
            }
            if directory {
                emit_file_tree(child, prefix, depth + 1, rows);
            }
            prefix.truncate(restore);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRecord {
    pub name: String,
    pub fetch_url: Option<String>,
    /// Set only when `remote.<name>.pushurl` overrides the fetch URL.
    pub push_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalRefRecord {
    /// The full ref name, retained so it can be used as a revision argument.
    pub name: ValidatedRefName,
    pub oid: Oid,
    /// The configured upstream's full ref name, when there is one.
    pub upstream: Option<ValidatedRefName>,
    pub head: bool,
}

/// One remote-tracking branch.
///
/// A symbolic ref is never one of these: see [`RemoteHead`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRefRecord {
    pub name: ValidatedRefName,
    pub oid: Oid,
    /// Remote name parsed out of `refs/remotes/<remote>/<branch>`, split at
    /// the same point [`RemoteHead::remote`] is, so a remote whose own name
    /// contains a slash is named identically by both and the two can be
    /// joined on this field. `git remote add te/am` is legal, and taking the
    /// first component alone gave `te` here against `te/am` there.
    pub remote: String,
    pub branch: String,
}

/// A remote's default branch: the target of `refs/remotes/<remote>/HEAD`.
///
/// That ref is a symbolic ref rather than a branch — it has no commit of its
/// own and every clone has one — so it is reported here instead of among the
/// remote-tracking branches, where it would otherwise appear as a branch
/// literally named `HEAD` beside the branch it points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteHead {
    /// The remote this pointer belongs to, taken as everything between
    /// `refs/remotes/` and the trailing `/HEAD`, so a remote whose own name
    /// contains a slash is named in full. This is the unambiguous half of the
    /// pair: `HEAD` is always the last component, while
    /// [`RemoteRefRecord::remote`] has to be split against the remote names
    /// these entries establish.
    pub remote: String,
    /// The remote-tracking ref it points at. The ref need not exist: a
    /// symbolic ref survives the deletion of its target.
    pub target: ValidatedRefName,
}

/// Everything one `for-each-ref` walk yields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefListing {
    pub local: Vec<LocalRefRecord>,
    pub remote: Vec<RemoteRefRecord>,
    pub remote_heads: Vec<RemoteHead>,
}

/// The files one worktree offers, split by whether Git tracks them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkingFiles {
    /// Tracked and untracked paths together, excluding ignored ones,
    /// deduplicated and sorted: the file tree shows both.
    pub paths: Vec<RepoPath>,
    /// How many of `paths` Git tracks; the remainder are untracked. A count
    /// rather than a second list, because nothing needs the tracked paths on
    /// their own and duplicating the listing of a large worktree to answer one
    /// number would not be free.
    pub tracked_count: usize,
}

/// Everything the source-control surfaces need about one worktree in one shot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositorySnapshot {
    pub status: StatusSnapshot,
    pub files: WorkingFiles,
    pub remotes: Vec<RemoteRecord>,
    pub local_refs: Vec<LocalRefRecord>,
    /// Real remote-tracking branches only.
    pub remote_refs: Vec<RemoteRefRecord>,
    /// One entry per remote whose `HEAD` pointer this repository has fetched.
    pub remote_heads: Vec<RemoteHead>,
}

impl RepositorySnapshot {
    /// The default branch a remote advertises, or `None` when this repository
    /// has no `refs/remotes/<remote>/HEAD` — which a remote added by hand,
    /// rather than by cloning, never gets.
    pub fn remote_head(&self, remote: &str) -> Option<&ValidatedRefName> {
        self.remote_heads
            .iter()
            .find(|head| head.remote == remote)
            .map(|head| &head.target)
    }

    /// The branch comparison base to open with, or `None` when nothing
    /// resolves.
    ///
    /// The upstream's symbolic remote HEAD is tried first, then
    /// `refs/remotes/origin/HEAD`. A name is never guessed: without a
    /// resolvable symbolic HEAD the caller must show the ref selector.
    #[must_use]
    pub fn default_branch_base(&self) -> Option<&ValidatedRefName> {
        let upstream_remote = self
            .local_refs
            .iter()
            .find(|record| record.head)
            .and_then(|record| record.upstream.as_ref())
            .and_then(|upstream| {
                self.remote_refs
                    .iter()
                    .find(|record| record.name == *upstream)
                    .map(|record| record.remote.as_str())
            });
        upstream_remote
            .and_then(|remote| self.remote_head(remote))
            .or_else(|| self.remote_head("origin"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryRevision {
    #[default]
    Head,
    Reference {
        name: ValidatedRefName,
    },
    Object {
        oid: Oid,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRequest {
    pub worktree: PathBuf,
    pub revision: HistoryRevision,
    /// Zero-based page; each page holds [`HISTORY_PAGE_SIZE`] commits.
    pub page: u32,
    /// Restricts the walk to one path when set.
    pub path: Option<RepoPath>,
}

/// An author or committer signature with its own recorded UTC offset, so a
/// commit renders in the timezone it was made in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitIdentity {
    pub name: String,
    pub email: String,
    /// Seconds since the Unix epoch.
    pub timestamp: i64,
    /// Offset from UTC in minutes, as recorded in the commit.
    pub offset_minutes: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSummary {
    pub oid: Oid,
    /// First parent first; empty for a root commit.
    pub parents: Vec<Oid>,
    pub author: CommitIdentity,
    pub committer: CommitIdentity,
    pub subject: String,
    /// Message body after the subject, with trailing newlines removed.
    pub body: String,
}

/// Which pair of trees a comparison is between.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "basis", rename_all = "snake_case")]
pub enum DiffBasis {
    /// Index to worktree — the unstaged view.
    IndexToWorktree,
    /// HEAD to index — the staged view.
    HeadToIndex,
    /// Merge base of `base_ref` and HEAD, to HEAD.
    MergeBaseToHead { base_ref: ValidatedRefName },
    /// One commit against a chosen parent; `None` for a root commit.
    CommitToParent { oid: Oid, parent: Option<Oid> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRequest {
    pub worktree: PathBuf,
    pub basis: DiffBasis,
    pub path: RepoPath,
    /// Rename or copy source; the old side reads it instead of `path`.
    pub old_path: Option<RepoPath>,
}

/// One side of a comparison.
///
/// Only [`DiffSide::Text`] is UTF-8 content. Bytes are never decoded lossily:
/// a non-UTF-8 or binary side is reported as metadata so the UI shows what it
/// is instead of an editable buffer full of replacement characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSide {
    /// The path does not exist on this side: added, deleted, or untracked.
    Absent,
    Text {
        text: String,
        bytes: u64,
    },
    Binary {
        bytes: u64,
    },
    InvalidUtf8 {
        bytes: u64,
    },
    /// Larger than [`MAX_DIFF_SIDE_BYTES`]; never partially read.
    Oversize {
        bytes: u64,
    },
    /// A gitlink: the recorded commit of a submodule, not a blob.
    Submodule {
        oid: Option<Oid>,
    },
    Symlink {
        target: PathBuf,
    },
    /// A worktree entry that is neither a regular file nor a symlink.
    NotRegularFile,
    Unreadable {
        reason: String,
    },
}

impl DiffSide {
    /// UTF-8 content, when this side has any.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text { text, .. } => Some(text),
            _ => None,
        }
    }

    /// True when a text diff can be computed from this side; [`Self::Absent`]
    /// counts, because an added or deleted file compares against nothing.
    pub fn is_diffable(&self) -> bool {
        matches!(self, Self::Absent | Self::Text { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffContent {
    Comparison {
        old: DiffSide,
        new: DiffSide,
        /// Human-readable side labels, e.g. `HEAD` and `Index`.
        old_label: String,
        new_label: String,
    },
    /// An unmerged path, read from index stages 1, 2 and 3 plus the worktree.
    /// A missing stage-0 blob is never reported as an empty added side.
    Unmerged {
        base: DiffSide,
        ours: DiffSide,
        theirs: DiffSide,
        worktree: DiffSide,
    },
}

/// A path chosen for a mutation, carrying its rename source so both halves of
/// a rename are acted on together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathSelection {
    pub path: RepoPath,
    pub old_path: Option<RepoPath>,
}

impl PathSelection {
    pub fn new(path: RepoPath) -> Self {
        Self {
            path,
            old_path: None,
        }
    }

    pub fn from_changed_path(entry: &ChangedPath) -> Self {
        Self {
            path: entry.path.clone(),
            old_path: entry.old_path.clone(),
        }
    }
}

/// Flattens a selection into the pathspec list a Git mutation receives.
///
/// A rename contributes both halves — staging only the destination would leave
/// the source's deletion behind — and duplicates are removed so overlapping
/// selections cannot list a path twice.
pub fn selection_pathspecs(selection: &[PathSelection]) -> Vec<RepoPath> {
    let mut paths = Vec::with_capacity(selection.len() * 2);
    for entry in selection {
        for path in [entry.old_path.as_ref(), Some(&entry.path)]
            .into_iter()
            .flatten()
        {
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }
    }
    paths
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageRequest {
    pub worktree: PathBuf,
    pub selection: Vec<PathSelection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnstageRequest {
    pub worktree: PathBuf,
    pub selection: Vec<PathSelection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscardRequest {
    pub worktree: PathBuf,
    /// Tracked paths, restored from the index.
    pub tracked: Vec<PathSelection>,
    /// Untracked paths, deleted from the worktree.
    pub untracked: Vec<RepoPath>,
    /// The user's explicit confirmation. Discard destroys work that exists
    /// nowhere else, so the service refuses to run without it.
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRequest {
    pub worktree: PathBuf,
    pub message: String,
}

/// A ref name as the *remote* knows it: the right-hand side of a push refspec,
/// and the ref argument to `git ls-remote`.
///
/// The distinction this type exists to hold is between that name and the
/// remote-tracking ref this clone keeps for the same branch.
/// `refs/remotes/origin/main` is this repository's own bookkeeping — the
/// remote calls that branch `refs/heads/main` and has never heard the other
/// name. Putting the tracking name on the wire is not a mistake Git can
/// report: `git push origin refs/heads/main:refs/remotes/origin/main` creates
/// a literal `refs/remotes/origin/main` *in the remote repository*, reports
/// the update as successful, exits 0, and leaves the branch exactly where it
/// was. Every outcome check therefore passes while nothing was published, and
/// the only symptom is that the branch stays "1 ahead" forever.
///
/// So no value of this type can be built except through a conversion that was
/// told which of the two spellings it was given, and neither `push_refspec`
/// nor [`RepositoryService::remote_ref_oid`] accepts anything else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RemoteSideRef(String);

impl RemoteSideRef {
    /// The remote-side name of a branch named directly, as on a first publish
    /// where the user chose the name the remote will use.
    pub fn branch(branch: &ValidatedBranchName) -> Self {
        Self(branch.full_ref())
    }

    /// A name already spelled the way the remote spells it.
    pub fn parse(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        let qualified = value
            .strip_prefix("refs/")
            .is_some_and(|rest| ValidatedBranchName::parse(rest).is_ok());
        if !qualified || value.starts_with("refs/remotes/") {
            return Err(ValidationError::NotARemoteSideRef(value));
        }
        Ok(Self(value))
    }

    /// What `remote` calls the ref this clone names `reference`.
    ///
    /// A `refs/remotes/...` name is split at the *known* remote name rather
    /// than at its first slash, because the name alone cannot say where the
    /// boundary is: with a remote called `te`, `refs/remotes/te/am/main` is the
    /// branch `am/main`, and with one called `te/am` it is `main`. The branch
    /// name that comes out is the upstream's, never the local branch's — an
    /// upstream of `refs/remotes/origin/published` for a local `work` is what a
    /// rename on either side leaves behind, and it publishes to `published`.
    ///
    /// Anything not under `refs/remotes/` is already a remote-side name and is
    /// taken as it stands: `branch.<name>.merge`, which is what Git records and
    /// what [`RepositoryService::upstream_push_target`] reads back, is spelled
    /// `refs/heads/<branch>`.
    pub fn resolve(remote: &str, reference: &ValidatedRefName) -> Result<Self, ValidationError> {
        let Some(rest) = reference.as_str().strip_prefix("refs/remotes/") else {
            return Self::parse(reference.as_str());
        };
        // A tracking ref belonging to a different remote names no branch on
        // this one, and guessing which branch it meant is precisely how a wrong
        // ref reaches the wire.
        let branch = rest
            .strip_prefix(remote)
            .and_then(|rest| rest.strip_prefix('/'))
            .ok_or_else(|| ValidationError::ForeignTrackingRef {
                remote: remote.to_owned(),
                reference: reference.as_str().to_owned(),
            })?;
        Ok(Self::branch(&ValidatedBranchName::parse(branch)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RemoteSideRef {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<RemoteSideRef> for String {
    fn from(value: RemoteSideRef) -> Self {
        value.0
    }
}

impl fmt::Display for RemoteSideRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum PushTarget {
    /// The branch already has an upstream; its remote and the upstream's ref
    /// are pushed explicitly rather than relying on push defaults.
    Upstream {
        remote: String,
        /// The upstream as *this clone* names it, which is either spelling:
        /// the remote-tracking ref `refs/remotes/<remote>/<branch>` that
        /// `%(upstream)` reports and the sidebar carries, or the remote-side
        /// `refs/heads/<branch>` that `branch.<name>.merge` holds. Neither is
        /// a name to put on the wire without going through
        /// [`RemoteSideRef::resolve`].
        remote_ref: ValidatedRefName,
    },
    /// First publish: the remote branch name is chosen by the user and the
    /// upstream is set as part of the push.
    SetUpstream {
        remote: String,
        remote_branch: ValidatedBranchName,
    },
}

impl PushTarget {
    pub fn remote(&self) -> &str {
        match self {
            Self::Upstream { remote, .. } | Self::SetUpstream { remote, .. } => remote,
        }
    }

    /// The ref this push updates, named the way the remote names it.
    pub fn remote_side_ref(&self) -> Result<RemoteSideRef, ValidationError> {
        match self {
            Self::Upstream { remote, remote_ref } => RemoteSideRef::resolve(remote, remote_ref),
            Self::SetUpstream { remote_branch, .. } => Ok(RemoteSideRef::branch(remote_branch)),
        }
    }

    pub fn sets_upstream(&self) -> bool {
        matches!(self, Self::SetUpstream { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRequest {
    pub worktree: PathBuf,
    /// The checked-out branch. A detached HEAD has none, and this service
    /// never guesses one.
    pub branch: ValidatedBranchName,
    pub target: PushTarget,
}

/// Why a mutation did not complete, keeping the raw evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MutationFailure {
    Process(ProcessFailure),
    /// A filesystem operation on one path failed, or the path failed its
    /// pre-delete validation.
    Path {
        path: RepoPath,
        reason: String,
    },
    /// The request itself was not runnable, e.g. nothing staged to commit.
    Rejected {
        reason: String,
    },
}

impl fmt::Display for MutationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Process(failure) => write!(formatter, "{failure}"),
            Self::Path { path, reason } => write!(formatter, "{path}: {reason}"),
            Self::Rejected { reason } => formatter.write_str(reason),
        }
    }
}

impl From<ProcessFailure> for MutationFailure {
    fn from(failure: ProcessFailure) -> Self {
        Self::Process(failure)
    }
}

/// Outcome of a stage, unstage, or discard.
///
/// Mirrors [`WorktreeCreationOutcome`]: a failure that never started the
/// process cannot have changed anything, while a failure after launch is only
/// reported as unknown when the service could not establish what happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum MutationOutcome {
    Applied {
        /// Paths the mutation acted on, after rename expansion and
        /// deduplication.
        paths: Vec<RepoPath>,
    },
    /// Part of the request was applied and the rest was refused, with both
    /// halves known exactly.
    ///
    /// This is not [`Self::OutcomeUnknown`]: a discard deletes each untracked
    /// path through its own syscall, so one path Git will not let go of
    /// neither hides the deletions that already happened nor makes them
    /// uncertain. The caller can retry precisely the paths in `failures`.
    PartiallyApplied {
        paths: Vec<RepoPath>,
        failures: Vec<MutationFailure>,
    },
    NotApplied {
        failure: MutationFailure,
    },
    OutcomeUnknown {
        failure: MutationFailure,
    },
}

/// What a Git command's own non-zero exit says about how much of a multi-path
/// request it applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationScope {
    /// The command works through its pathspecs and reports the ones it could
    /// not handle at the end: `git add -A -- good ignored` exits 1 *having
    /// staged* `good`, so a non-zero exit leaves the split between the two
    /// halves unknown.
    Incremental,
    /// The command assembles its whole result in memory and commits it with a
    /// single write, so a non-zero exit means nothing was written at all:
    /// `git restore --staged` and `git rm --cached` each write the index once,
    /// after the last pathspec, under `index.lock`.
    AllOrNothing,
    /// The command resolves every pathspec before it writes anything, and
    /// then writes entry by entry.
    ///
    /// `git restore --worktree` refuses a pathspec that matches nothing, or a
    /// path that is unmerged, without having restored any of the others, in
    /// either pathspec order — that half really is all-or-nothing. Its
    /// checkout loop is not: it walks the index ORing each entry's error into
    /// one return value, so a failure raised *inside* it — an unwritable
    /// parent directory, a blob a partial clone does not have — still leaves
    /// every entry the loop had already reached overwritten, and the entry it
    /// failed on unlinked and not put back. Reporting "this did not happen"
    /// while the user's uncommitted work is gone is the one thing the one
    /// operation that destroys work must never do.
    AtomicRefusal,
}

impl MutationScope {
    /// Classifies a non-zero exit from a command of this scope.
    ///
    /// Git's own exit code separates [`Self::AtomicRefusal`]'s two modes: a
    /// refusal is an `error()` and exit 1, while a failure inside the write
    /// loop is a negative return that reaches `exit` as 255, or a `die()` as
    /// 128. Anything that is not the refusal's own 1 is therefore read as
    /// possibly-written, which is both the safe direction and one that does
    /// not tie this to the wording of a Git diagnostic.
    fn classify(self, exit_code: Option<i32>, failure: MutationFailure) -> MutationOutcome {
        match self {
            Self::Incremental => MutationOutcome::OutcomeUnknown { failure },
            Self::AllOrNothing => MutationOutcome::NotApplied { failure },
            Self::AtomicRefusal if exit_code == Some(1) => MutationOutcome::NotApplied { failure },
            Self::AtomicRefusal => MutationOutcome::OutcomeUnknown { failure },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CommitOutcome {
    Created {
        oid: Oid,
        /// A post-commit failure, such as a failing hook, that did not prevent
        /// the commit HEAD now points at.
        warnings: Vec<MutationFailure>,
    },
    NotCreated {
        failure: MutationFailure,
    },
    OutcomeUnknown {
        failure: MutationFailure,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PushOutcome {
    Pushed {
        remote: String,
        /// The ref the *remote* moved, named the way the remote names it —
        /// this is what the user is told was published, so a tracking ref here
        /// would be a report of something that never happened.
        remote_ref: RemoteSideRef,
        /// The local commit that is now on the remote.
        oid: Oid,
    },
    NotPushed {
        failure: MutationFailure,
    },
    OutcomeUnknown {
        failure: MutationFailure,
    },
}

#[derive(Debug, Default)]
struct KeyedLock {
    held: Mutex<bool>,
    released: Condvar,
}

/// Serializes mutations per Git common directory.
///
/// Two `git add` runs against one index corrupt each other's `index.lock`, but
/// a read of an unrelated repository — or of the same one — has no reason to
/// wait. Keying on the common directory is what makes that distinction: every
/// linked worktree of one repository shares an index lock and therefore one
/// key, while separate repositories never contend.
#[derive(Debug, Clone, Default)]
pub struct MutationLocks(Arc<Mutex<BTreeMap<PathBuf, Arc<KeyedLock>>>>);

impl MutationLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Blocks until no other holder owns `common_dir`, then returns the guard
    /// that must be held for the whole mutation.
    pub fn lock(&self, common_dir: &Path) -> MutationGuard {
        let entry = {
            let mut registry = lock_registry(&self.0);
            Arc::clone(
                registry
                    .entry(common_dir.to_path_buf())
                    .or_insert_with(|| Arc::new(KeyedLock::default())),
            )
        };
        let mut held = entry
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *held {
            held = entry
                .released
                .wait(held)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *held = true;
        drop(held);
        MutationGuard {
            registry: Arc::clone(&self.0),
            key: common_dir.to_path_buf(),
            entry,
        }
    }

    /// Number of tracked keys, for tests.
    #[cfg(test)]
    fn tracked_keys(&self) -> usize {
        lock_registry(&self.0).len()
    }
}

/// Held for the duration of one mutation; releasing it wakes one waiter.
#[derive(Debug)]
pub struct MutationGuard {
    registry: Arc<Mutex<BTreeMap<PathBuf, Arc<KeyedLock>>>>,
    key: PathBuf,
    entry: Arc<KeyedLock>,
}

impl Drop for MutationGuard {
    fn drop(&mut self) {
        let mut held = self
            .entry
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *held = false;
        drop(held);
        self.entry.released.notify_one();
        // The registry entry is only worth keeping while somebody is waiting on
        // it; dropping the last one keeps a long session from accumulating a
        // key per repository it ever touched.
        let mut registry = lock_registry(&self.registry);
        if let Some(entry) = registry.get(&self.key)
            && Arc::strong_count(entry) == 2
        {
            registry.remove(&self.key);
        }
    }
}

/// A panic while holding this registry cannot leave the map inconsistent, and
/// refusing to lock afterwards would deadlock every later mutation.
fn lock_registry(
    registry: &Mutex<BTreeMap<PathBuf, Arc<KeyedLock>>>,
) -> MutexGuard<'_, BTreeMap<PathBuf, Arc<KeyedLock>>> {
    registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
                let value = value
                    .ok_or_else(|| malformed("git worktree list", index, "branch is missing"))?;
                // A ref name is an arbitrary byte string and this label is
                // shown, never handed back to Git — the same standing as
                // `branch.head` in `git status`. Forcing it through a
                // conversion that can fail let one worktree checked out on an
                // undecodable branch fail the entire listing, and with it the
                // project load, `discover`, and every worktree reconciliation
                // that reads the list back.
                record.branch = Some(String::from_utf8_lossy(value).into_owned());
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
    let mut entries = Vec::new();
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
                // A ref name is an arbitrary NUL- and LF-free byte string, not
                // UTF-8, and `git checkout -b` accepts one that no encoding
                // decodes. These two labels are display data — the branch is
                // shown, and the upstream is shown beside its ahead/behind
                // counts — so an undecodable byte renders as U+FFFD instead of
                // discarding every changed path in the snapshot with it.
                //
                // The exact bytes are deliberately not kept alongside, because
                // acting on the label is not what stops a lossy name from
                // naming the wrong ref. `ValidatedBranchName::parse` screens
                // ASCII only — U+FFFD contains no byte it forbids, so the
                // lossy name parses — and what actually refuses it is the
                // existence check every branch operation makes first:
                // `Self::push` resolves `refs/heads/<name>` through
                // `resolve_optional_revision` and reports "does not exist"
                // before a `git push` is ever spawned, because
                // `refs/heads/b<U+FFFD>ad` is not a ref this repository has.
                // That check is load-bearing, not incidental, and
                // `a_lossy_branch_label_cannot_act_on_the_branch_it_names`
                // pins it.
                branch = Some(String::from_utf8_lossy(value).into_owned());
            } else if let Some(value) = header.strip_prefix(b"branch.upstream ") {
                upstream = Some(String::from_utf8_lossy(value).into_owned());
            } else if let Some(value) = header.strip_prefix(b"branch.ab ") {
                ahead_behind = Some(parse_ahead_behind(value, index)?);
            }
            index += 1;
            continue;
        }

        match field[0] {
            b'1' => {
                let record = parse_changed_record(field, index, 9)?;
                push_changed_entries(&mut entries, &mut dirty, &record, None, index)?;
            }
            b'2' => {
                let record = parse_changed_record(field, index, 10)?;
                // The rename or copy source is its own NUL-delimited field
                // immediately after the record.
                index += 1;
                let source = fields.get(index).copied().ok_or_else(|| {
                    malformed("git status", index, "rename source path is missing")
                })?;
                if source.is_empty() {
                    return Err(malformed(
                        "git status",
                        index,
                        "rename source path is empty",
                    ));
                }
                let source = repo_path_field(source, "git status", index)?;
                push_changed_entries(&mut entries, &mut dirty, &record, Some(source), index)?;
            }
            b'u' => {
                let record = fixed_fields(field, index, 11)?;
                let xy = record[1];
                if xy.len() != 2 {
                    return Err(malformed(
                        "git status",
                        index,
                        "XY status must contain two bytes",
                    ));
                }
                increment(&mut dirty.conflicted)?;
                let submodule = record[2].first() == Some(&b'S');
                if submodule {
                    increment(&mut dirty.submodule)?;
                }
                entries.push(ChangedPath {
                    path: repo_path_field(record[10], "git status", index)?,
                    old_path: None,
                    area: ChangeArea::Conflict,
                    kind: ChangeKind::Unmerged,
                    conflict: Some(parse_conflict_kind(xy, index)?),
                    submodule,
                    added_lines: None,
                    removed_lines: None,
                });
            }
            b'?' => {
                let path = path_record(field, index)?;
                increment(&mut dirty.untracked)?;
                entries.push(ChangedPath {
                    path: repo_path_field(path, "git status", index)?,
                    old_path: None,
                    area: ChangeArea::Untracked,
                    kind: ChangeKind::Untracked,
                    conflict: None,
                    submodule: false,
                    added_lines: None,
                    removed_lines: None,
                });
            }
            // Only reported with `--ignored`, which this service never passes;
            // tolerated so a caller that adds it does not fail parsing.
            b'!' => {
                path_record(field, index)?;
            }
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
        entries,
    })
}

/// The fields of a porcelain-v2 `1` or `2` record that this service uses.
struct ChangedRecord<'a> {
    xy: &'a [u8],
    submodule: bool,
    path: &'a [u8],
}

fn parse_changed_record<'a>(
    field: &'a [u8],
    index: usize,
    count: usize,
) -> Result<ChangedRecord<'a>, GitParseError> {
    let fields = fixed_fields(field, index, count)?;
    let xy = fields[1];
    if xy.len() != 2 {
        return Err(malformed(
            "git status",
            index,
            "XY status must contain two bytes",
        ));
    }
    Ok(ChangedRecord {
        xy,
        submodule: fields[2].first() == Some(&b'S'),
        path: fields[count - 1],
    })
}

/// Emits one entry per non-`.` half of an XY status.
///
/// A path staged *and* modified again in the worktree is two separate pieces of
/// work — the staged version can be committed while the worktree version is
/// still being edited — so it becomes two entries rather than one ambiguous
/// row. `dirty` is incremented exactly as it was before entries existed, so
/// the sidebar badges keep their current counts.
fn push_changed_entries(
    entries: &mut Vec<ChangedPath>,
    dirty: &mut DirtyState,
    record: &ChangedRecord<'_>,
    source: Option<RepoPath>,
    index: usize,
) -> Result<(), GitParseError> {
    let path = repo_path_field(record.path, "git status", index)?;
    if record.xy[0] != b'.' {
        increment(&mut dirty.staged)?;
    }
    if record.xy[1] != b'.' {
        increment(&mut dirty.unstaged)?;
    }
    if record.submodule {
        increment(&mut dirty.submodule)?;
    }
    for (code, area) in [
        (record.xy[0], ChangeArea::Staged),
        (record.xy[1], ChangeArea::Unstaged),
    ] {
        if code == b'.' {
            continue;
        }
        let kind = change_kind_from_code(code);
        entries.push(ChangedPath {
            path: path.clone(),
            // The source belongs to the half that actually renamed or copied:
            // for `R.` the index moved the file, and the worktree half of `RM`
            // is an ordinary modification of the destination.
            old_path: match kind {
                ChangeKind::Renamed | ChangeKind::Copied => source.clone(),
                _ => None,
            },
            area,
            kind,
            conflict: None,
            submodule: record.submodule,
            added_lines: None,
            removed_lines: None,
        });
    }
    Ok(())
}

fn change_kind_from_code(code: u8) -> ChangeKind {
    match code {
        b'M' => ChangeKind::Modified,
        b'T' => ChangeKind::TypeChanged,
        b'A' => ChangeKind::Added,
        b'D' => ChangeKind::Deleted,
        b'R' => ChangeKind::Renamed,
        b'C' => ChangeKind::Copied,
        b'U' => ChangeKind::Unmerged,
        b'?' => ChangeKind::Untracked,
        b'!' => ChangeKind::Ignored,
        other => ChangeKind::Unknown(char::from(other)),
    }
}

fn parse_conflict_kind(xy: &[u8], index: usize) -> Result<ConflictKind, GitParseError> {
    Ok(match (xy[0], xy[1]) {
        (b'D', b'D') => ConflictKind::BothDeleted,
        (b'A', b'U') => ConflictKind::AddedByUs,
        (b'U', b'D') => ConflictKind::DeletedByThem,
        (b'U', b'A') => ConflictKind::AddedByThem,
        (b'D', b'U') => ConflictKind::DeletedByUs,
        (b'A', b'A') => ConflictKind::BothAdded,
        (b'U', b'U') => ConflictKind::BothModified,
        _ => {
            return Err(malformed("git status", index, "unknown unmerged XY status"));
        }
    })
}

fn repo_path_field(
    bytes: &[u8],
    format: &'static str,
    index: usize,
) -> Result<RepoPath, GitParseError> {
    RepoPath::from_bytes(bytes).map_err(|error| malformed(format, index, error.to_string()))
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

/// The path of a `?` or `!` record, which is one space-separated field.
fn path_record<'a>(field: &'a [u8], index: usize) -> Result<&'a [u8], GitParseError> {
    if field.len() < 3 || field[1] != b' ' {
        Err(malformed("git status", index, "path is missing"))
    } else {
        Ok(&field[2..])
    }
}

/// One `git diff --numstat -z` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumstatRecord {
    pub path: RepoPath,
    pub old_path: Option<RepoPath>,
    /// `None` where Git printed `-`, which it does for binary files.
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

pub fn parse_numstat(output: &[u8]) -> Result<Vec<NumstatRecord>, GitParseError> {
    const FORMAT: &str = "git diff --numstat";
    let fields = nul_fields(output, FORMAT)?;
    let mut records = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        if field.is_empty() {
            return Err(malformed(FORMAT, index, "record is empty"));
        }
        let mut columns = field.splitn(3, |byte| *byte == b'\t');
        let added = columns
            .next()
            .ok_or_else(|| malformed(FORMAT, index, "added count is missing"))?;
        let removed = columns
            .next()
            .ok_or_else(|| malformed(FORMAT, index, "removed count is missing"))?;
        let path = columns
            .next()
            .ok_or_else(|| malformed(FORMAT, index, "path is missing"))?;
        let added = parse_numstat_count(added, FORMAT, index)?;
        let removed = parse_numstat_count(removed, FORMAT, index)?;
        let (path, old_path) = if path.is_empty() {
            // A rename or copy: the in-record path column is empty and the
            // source and destination follow as two more NUL-delimited fields.
            let source = fields
                .get(index + 1)
                .copied()
                .ok_or(GitParseError::Truncated { format: FORMAT })?;
            let destination = fields
                .get(index + 2)
                .copied()
                .ok_or(GitParseError::Truncated { format: FORMAT })?;
            index += 2;
            (
                repo_path_field(destination, FORMAT, index)?,
                Some(repo_path_field(source, FORMAT, index)?),
            )
        } else {
            (repo_path_field(path, FORMAT, index)?, None)
        };
        records.push(NumstatRecord {
            path,
            old_path,
            added,
            removed,
        });
        index += 1;
    }
    Ok(records)
}

fn parse_numstat_count(
    value: &[u8],
    format: &'static str,
    index: usize,
) -> Result<Option<u64>, GitParseError> {
    if value == b"-" {
        return Ok(None);
    }
    utf8(value, format, index, "count")?
        .parse::<u64>()
        .map(Some)
        .map_err(|_| malformed(format, index, "count is not a number"))
}

/// Fills measured line counts into the entries of one area.
///
/// Counts stay `None` where Git reported none, which is every binary file, and
/// no conflicted or untracked entry is ever touched because neither area
/// appears in a `git diff` comparison.
pub fn merge_numstat(entries: &mut [ChangedPath], area: ChangeArea, records: &[NumstatRecord]) {
    let counts: BTreeMap<&[u8], (Option<u64>, Option<u64>)> = records
        .iter()
        .map(|record| (record.path.as_bytes(), (record.added, record.removed)))
        .collect();
    for entry in entries.iter_mut().filter(|entry| entry.area == area) {
        if let Some((added, removed)) = counts.get(entry.path.as_bytes()) {
            entry.added_lines = *added;
            entry.removed_lines = *removed;
        }
    }
}

fn merge_diff_numstat(entries: &mut [DiffEntry], records: &[NumstatRecord]) {
    let counts: BTreeMap<&[u8], (Option<u64>, Option<u64>)> = records
        .iter()
        .map(|record| (record.path.as_bytes(), (record.added, record.removed)))
        .collect();
    for entry in entries.iter_mut() {
        if let Some((added, removed)) = counts.get(entry.path.as_bytes()) {
            entry.added_lines = *added;
            entry.removed_lines = *removed;
        }
    }
}

pub fn parse_name_status(output: &[u8]) -> Result<Vec<DiffEntry>, GitParseError> {
    const FORMAT: &str = "git diff --name-status";
    let fields = nul_fields(output, FORMAT)?;
    let mut entries = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let status = fields[index];
        let code = *status
            .first()
            .ok_or_else(|| malformed(FORMAT, index, "status is empty"))?;
        let kind = change_kind_from_code(code);
        let path_index = index + 1;
        let path = fields
            .get(path_index)
            .copied()
            .ok_or(GitParseError::Truncated { format: FORMAT })?;
        if matches!(kind, ChangeKind::Renamed | ChangeKind::Copied) {
            let destination = fields
                .get(path_index + 1)
                .copied()
                .ok_or(GitParseError::Truncated { format: FORMAT })?;
            entries.push(DiffEntry {
                path: repo_path_field(destination, FORMAT, path_index)?,
                old_path: Some(repo_path_field(path, FORMAT, path_index)?),
                kind,
                added_lines: None,
                removed_lines: None,
            });
            index = path_index + 2;
        } else {
            entries.push(DiffEntry {
                path: repo_path_field(path, FORMAT, path_index)?,
                old_path: None,
                kind,
                added_lines: None,
                removed_lines: None,
            });
            index = path_index + 1;
        }
    }
    Ok(entries)
}

/// Fields per commit in [`RepositoryService::history_command`]'s format.
const HISTORY_FIELDS: usize = 10;

pub fn parse_history(output: &[u8]) -> Result<Vec<CommitSummary>, GitParseError> {
    const FORMAT: &str = "git log";
    let fields = nul_fields(output, FORMAT)?;
    if !fields.len().is_multiple_of(HISTORY_FIELDS) {
        return Err(malformed(
            FORMAT,
            fields.len() / HISTORY_FIELDS,
            "record does not have the requested field count",
        ));
    }
    let mut commits = Vec::with_capacity(fields.len() / HISTORY_FIELDS);
    for (record, chunk) in fields.chunks_exact(HISTORY_FIELDS).enumerate() {
        let oid = Oid::parse_object(utf8(chunk[0], FORMAT, record, "commit id")?)
            .map_err(|error| malformed(FORMAT, record, error.to_string()))?;
        let parents = utf8(chunk[1], FORMAT, record, "parents")?
            .split_ascii_whitespace()
            .map(|parent| {
                Oid::parse_object(parent)
                    .map_err(|error| malformed(FORMAT, record, error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        commits.push(CommitSummary {
            oid,
            parents,
            author: parse_identity(chunk[2], chunk[3], chunk[4]),
            committer: parse_identity(chunk[5], chunk[6], chunk[7]),
            // Message text is display data, not a protocol field: a commit
            // written in a legacy encoding still has a usable subject, so it is
            // rendered lossily instead of failing the page.
            subject: String::from_utf8_lossy(chunk[8]).into_owned(),
            body: String::from_utf8_lossy(chunk[9])
                .trim_end_matches('\n')
                .to_owned(),
        });
    }
    Ok(commits)
}

/// Builds one signature, degrading a date this cannot read rather than
/// discarding the page it belongs to.
///
/// A commit object's date line is whatever was written into it, and a history
/// imported from CVS or Subversion carries offsets Git itself will not
/// normalise: `+051800` comes back as `+51800`, `+0060` comes back verbatim,
/// and an offset that is not a number at all makes `%ad` print nothing. None
/// of that is evidence about the other 199 commits in the chunk, and a date is
/// display data — so an unreadable one becomes the epoch in UTC for that one
/// commit, which is visibly wrong in the row it belongs to instead of
/// invisibly fatal for the whole page.
fn parse_identity(name: &[u8], email: &[u8], date: &[u8]) -> CommitIdentity {
    let (timestamp, offset) = match std::str::from_utf8(date)
        .unwrap_or_default()
        .split_once(' ')
    {
        Some((timestamp, offset)) => (timestamp.parse::<i64>().unwrap_or_default(), offset),
        None => (0, ""),
    };
    CommitIdentity {
        name: String::from_utf8_lossy(name).into_owned(),
        email: String::from_utf8_lossy(email).into_owned(),
        timestamp,
        offset_minutes: parse_timezone_offset(offset).unwrap_or_default(),
    }
}

/// Parses a commit's recorded UTC offset into minutes, exactly as Git reads it.
///
/// Git treats the field as a signed decimal number `N` and splits it as
/// `N / 100` hours plus `N % 100` minutes, with no range check on either: that
/// is why `+0060` displays an hour east of UTC and `+51800` displays 518 hours
/// east of it. Reproducing the same arithmetic is what makes a rendered
/// timestamp agree with `git log` on the commits that have such an offset,
/// rather than merely not crashing on them. `None` means the field was not a
/// number, or named an offset too large to hold.
fn parse_timezone_offset(value: &str) -> Option<i32> {
    let (sign, digits) = match value.as_bytes().first()? {
        b'+' => (1, &value[1..]),
        b'-' => (-1, &value[1..]),
        _ => return None,
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value = digits.parse::<i32>().ok()?;
    (value / 100)
        .checked_mul(60)?
        .checked_add(value % 100)?
        .checked_mul(sign)
}

/// Parses the `for-each-ref` format used by
/// [`RepositoryService::refs_command`]: NUL-separated fields, one newline-
/// terminated line per ref. Ref names cannot contain either byte, so the
/// framing is unambiguous.
///
/// A ref this cannot classify is skipped rather than fatal, exactly as
/// [`parse_worktree_list`] skips a field it does not know and
/// [`ChangeKind::Unknown`] absorbs a status letter it does not know. The
/// alternative — one `git update-ref refs/remotes/weird` or one branch whose
/// name is not UTF-8 discarding every changed path in the snapshot — treats a
/// ref nobody asked about as evidence that Git's whole answer is untrustworthy.
pub fn parse_refs(output: &[u8]) -> Result<RefListing, GitParseError> {
    const FORMAT: &str = "git for-each-ref";
    const REMOTES: &str = "refs/remotes/";
    let mut listing = RefListing::default();
    // A remote name may contain a slash, so `refs/remotes/te/am/main` is
    // `main` on a remote called `te/am` or `am/main` on one called `te`, and
    // the ref name alone cannot say which. The symbolic
    // `refs/remotes/<remote>/HEAD` refs in this same output are the one place
    // the boundary is unambiguous, because `HEAD` is always the last
    // component — so the remote-tracking refs are held back until every one of
    // those has been read, and then split at the longest remote name the
    // listing actually proved exists.
    let mut pending: Vec<(ValidatedRefName, Oid)> = Vec::new();
    for (record, line) in output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let fields: Vec<_> = line.split(|byte| *byte == 0).collect();
        if fields.len() != 5 {
            return Err(malformed(
                FORMAT,
                record,
                "ref record has a wrong field count",
            ));
        }
        // The field count is the framing and stays a hard requirement; the
        // name is not, because a ref name is bytes and only a `ValidatedRefName`
        // can ever be handed back to Git as a revision.
        let Some(name) = std::str::from_utf8(fields[0])
            .ok()
            .and_then(|name| ValidatedRefName::parse(name).ok())
        else {
            continue;
        };
        let oid = Oid::parse_object(utf8(fields[1], FORMAT, record, "objectname")?)
            .map_err(|error| malformed(FORMAT, record, error.to_string()))?;
        if name.as_str().starts_with("refs/heads/") {
            listing.local.push(LocalRefRecord {
                name,
                oid,
                upstream: optional_ref(fields[3]),
                head: fields[4] == b"*",
            });
            continue;
        }
        let Some(rest) = name.as_str().strip_prefix(REMOTES) else {
            continue;
        };
        // Symbolic-ness is read from the raw field rather than from a parsed
        // target, so a target this cannot represent still keeps the ref out of
        // the branch list.
        if !fields[2].is_empty() {
            if let (Some(remote), Some(target)) =
                (rest.strip_suffix("/HEAD"), optional_ref(fields[2]))
            {
                listing.remote_heads.push(RemoteHead {
                    remote: remote.to_owned(),
                    target,
                });
            }
            continue;
        }
        pending.push((name, oid));
    }
    let remotes: Vec<&str> = listing
        .remote_heads
        .iter()
        .map(|head| head.remote.as_str())
        .collect();
    for (name, oid) in pending {
        // Guaranteed: only a name carrying this prefix reaches `pending`.
        let rest = &name.as_str()[REMOTES.len()..];
        let split = remotes
            .iter()
            .filter(|remote| {
                rest.strip_prefix(**remote)
                    .is_some_and(|branch| branch.starts_with('/'))
            })
            .max_by_key(|remote| remote.len())
            .map(|remote| ((*remote).to_owned(), rest[remote.len() + 1..].to_owned()))
            // No remote in this listing advertises a `HEAD` that covers this
            // ref, so the first component is the only reading there is — and
            // it is the one Git's own default refspec produces.
            .or_else(|| {
                rest.split_once('/')
                    .map(|(remote, branch)| (remote.to_owned(), branch.to_owned()))
            });
        let Some((remote, branch)) = split else {
            continue;
        };
        listing.remote.push(RemoteRefRecord {
            remote,
            branch,
            name,
            oid,
        });
    }
    Ok(listing)
}

/// A ref name Git reported in an optional field, or `None` when the field was
/// empty or named something this cannot hand back to Git.
fn optional_ref(value: &[u8]) -> Option<ValidatedRefName> {
    ValidatedRefName::parse(std::str::from_utf8(value).ok()?).ok()
}

/// Parses `git config -z --get-regexp ^remote\.` output, whose records are
/// `key LF value NUL`.
pub fn parse_remote_config(output: &[u8]) -> Result<Vec<RemoteRecord>, GitParseError> {
    const FORMAT: &str = "git config";
    let mut remotes: BTreeMap<String, RemoteRecord> = BTreeMap::new();
    for (record, field) in nul_fields(output, FORMAT)?.into_iter().enumerate() {
        if field.is_empty() {
            continue;
        }
        let (key, value) = match field.iter().position(|byte| *byte == b'\n') {
            Some(split) => (&field[..split], &field[split + 1..]),
            None => (field, &field[field.len()..]),
        };
        let key = utf8(key, FORMAT, record, "config key")?;
        let Some(rest) = key.strip_prefix("remote.") else {
            continue;
        };
        // A remote name may itself contain dots, so the *last* dot separates
        // the name from the variable.
        let Some((name, variable)) = rest.rsplit_once('.') else {
            continue;
        };
        if name.is_empty() || !matches!(variable, "url" | "pushurl") {
            continue;
        }
        let entry = remotes
            .entry(name.to_owned())
            .or_insert_with(|| RemoteRecord {
                name: name.to_owned(),
                fetch_url: None,
                push_url: None,
            });
        // URLs are opaque configuration text rather than a path this service
        // reopens, so an undecodable byte is rendered rather than fatal.
        let value = String::from_utf8_lossy(value).into_owned();
        if variable == "url" {
            entry.fetch_url = Some(value);
        } else {
            entry.push_url = Some(value);
        }
    }
    Ok(remotes.into_values().collect())
}

/// Parses a NUL-delimited path list, deduplicated and sorted.
///
/// `git ls-files -c` prints one line per index stage, so an unmerged path
/// appears up to three times; the file tree wants it once.
pub fn parse_path_list(
    output: &[u8],
    format: &'static str,
) -> Result<Vec<RepoPath>, GitParseError> {
    let mut paths = nul_fields(output, format)?
        .into_iter()
        .filter(|field| !field.is_empty())
        .enumerate()
        .map(|(index, field)| repo_path_field(field, format, index))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Parses `git ls-files -t`, deduplicated, sorted, and counted by
/// trackedness.
///
/// `-t` puts a one-byte status tag and a space ahead of each path. `?` is an
/// "other", i.e. an untracked file; every other tag a listing of cached and
/// other files can carry — `H` cached, `S` skip-worktree, `M` unmerged —
/// describes an index entry, so anything but `?` is tracked. `git ls-files`
/// prints one record per index stage, so an unmerged path arrives three times
/// and the count is taken after deduplication.
pub fn parse_working_files(output: &[u8]) -> Result<WorkingFiles, GitParseError> {
    const FORMAT: &str = "git ls-files -t";
    let mut tagged: BTreeMap<RepoPath, bool> = BTreeMap::new();
    for (record, field) in nul_fields(output, FORMAT)?.into_iter().enumerate() {
        if field.is_empty() {
            continue;
        }
        if field.len() < 2 || field[1] != b' ' {
            return Err(malformed(FORMAT, record, "record has no status tag"));
        }
        let tracked = tagged
            .entry(repo_path_field(&field[2..], FORMAT, record)?)
            .or_default();
        *tracked |= field[0] != b'?';
    }
    let tracked_count = tagged.values().filter(|tracked| **tracked).count();
    Ok(WorkingFiles {
        paths: tagged.into_keys().collect(),
        tracked_count,
    })
}

/// Git's file mode for a gitlink, i.e. a submodule's recorded commit.
const GITLINK_MODE: u32 = 0o160_000;
/// Git's file mode for a symbolic link blob.
const SYMLINK_MODE: u32 = 0o120_000;

/// One index entry from `git ls-files -s -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexEntry {
    mode: u32,
    oid: Oid,
    /// 0 for a merged path, 1/2/3 for base/ours/theirs of a conflict.
    stage: u8,
    path: RepoPath,
}

fn parse_index_entries(output: &[u8]) -> Result<Vec<IndexEntry>, GitParseError> {
    const FORMAT: &str = "git ls-files -s";
    let mut entries = Vec::new();
    for (record, field) in nul_fields(output, FORMAT)?.into_iter().enumerate() {
        if field.is_empty() {
            continue;
        }
        let split = field
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| malformed(FORMAT, record, "index entry has no path separator"))?;
        let mut columns = utf8(&field[..split], FORMAT, record, "index entry")?.split(' ');
        let mode = columns
            .next()
            .and_then(|mode| u32::from_str_radix(mode, 8).ok())
            .ok_or_else(|| malformed(FORMAT, record, "index entry mode is invalid"))?;
        let oid = columns
            .next()
            .ok_or_else(|| malformed(FORMAT, record, "index entry object id is missing"))
            .and_then(|oid| {
                Oid::parse_object(oid).map_err(|error| malformed(FORMAT, record, error.to_string()))
            })?;
        let stage = columns
            .next()
            .and_then(|stage| stage.parse::<u8>().ok())
            .filter(|stage| *stage <= 3)
            .ok_or_else(|| malformed(FORMAT, record, "index entry stage is invalid"))?;
        entries.push(IndexEntry {
            mode,
            oid,
            stage,
            path: repo_path_field(&field[split + 1..], FORMAT, record)?,
        });
    }
    Ok(entries)
}

/// One tree entry from `git ls-tree -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeEntry {
    mode: u32,
    oid: Oid,
    path: RepoPath,
}

fn parse_tree_entries(output: &[u8]) -> Result<Vec<TreeEntry>, GitParseError> {
    const FORMAT: &str = "git ls-tree";
    let mut entries = Vec::new();
    for (record, field) in nul_fields(output, FORMAT)?.into_iter().enumerate() {
        if field.is_empty() {
            continue;
        }
        let split = field
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| malformed(FORMAT, record, "tree entry has no path separator"))?;
        let mut columns = utf8(&field[..split], FORMAT, record, "tree entry")?.split(' ');
        let mode = columns
            .next()
            .and_then(|mode| u32::from_str_radix(mode, 8).ok())
            .ok_or_else(|| malformed(FORMAT, record, "tree entry mode is invalid"))?;
        // The object type column is redundant with the mode this service acts
        // on, so it is validated for shape and then skipped.
        columns
            .next()
            .ok_or_else(|| malformed(FORMAT, record, "tree entry type is missing"))?;
        let oid = columns
            .next()
            .ok_or_else(|| malformed(FORMAT, record, "tree entry object id is missing"))
            .and_then(|oid| {
                Oid::parse_object(oid).map_err(|error| malformed(FORMAT, record, error.to_string()))
            })?;
        entries.push(TreeEntry {
            mode,
            oid,
            path: repo_path_field(&field[split + 1..], FORMAT, record)?,
        });
    }
    Ok(entries)
}

/// One object of a `git cat-file --batch` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BatchObject {
    Content(Vec<u8>),
    /// Git answered `<oid> missing`, which it does for an object this
    /// repository does not have — the state a blobless or otherwise partial
    /// clone is left in when its promisor remote cannot be reached.
    Missing,
}

/// Splits `git cat-file --batch` output into one payload per requested object.
///
/// Each present object is a `<oid> <type> <size>` header line, exactly `size`
/// bytes of content, and a terminating newline, so the content itself needs no
/// escaping and a NUL or newline inside a blob cannot desynchronise the
/// stream. An absent object is a bare `<oid> missing` line with no content and
/// no trailing newline of its own.
fn parse_batch_blobs(
    output: &[u8],
    format: &'static str,
) -> Result<Vec<BatchObject>, GitParseError> {
    let mut objects = Vec::new();
    let mut cursor = 0;
    while cursor < output.len() {
        let record = objects.len();
        let header_end = output[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| cursor + offset)
            .ok_or(GitParseError::Truncated { format })?;
        let header = utf8(&output[cursor..header_end], format, record, "batch header")?;
        if header.ends_with(" missing") {
            objects.push(BatchObject::Missing);
            cursor = header_end + 1;
            continue;
        }
        let size = header
            .rsplit(' ')
            .next()
            .and_then(|size| size.parse::<usize>().ok())
            .ok_or_else(|| malformed(format, record, "batch header size is invalid"))?;
        let content_start = header_end + 1;
        let content_end = content_start
            .checked_add(size)
            .filter(|end| *end < output.len())
            .ok_or(GitParseError::Truncated { format })?;
        objects.push(BatchObject::Content(
            output[content_start..content_end].to_vec(),
        ));
        // Skip the newline Git appends after the content.
        cursor = content_end + 1;
    }
    Ok(objects)
}

/// The push status flag Git printed for one refspec, or `None` when the
/// refspec is absent from the output.
///
/// `--porcelain` lines are `<flag> TAB <from>:<to> TAB <summary>`, and `!`
/// means the remote refused the update, which is definitive evidence that the
/// remote ref did not move.
fn push_ref_flag(output: &[u8], refspec: &str) -> Option<u8> {
    for line in output.split(|byte| *byte == b'\n') {
        let mut columns = line.split(|byte| *byte == b'\t');
        let flag = columns.next()?;
        let Some(pair) = columns.next() else {
            continue;
        };
        if pair == refspec.as_bytes() {
            return Some(flag.first().copied().unwrap_or(b' '));
        }
    }
    None
}

/// Classifies raw bytes without ever decoding them lossily.
fn classify_bytes(bytes: Vec<u8>) -> DiffSide {
    let size = bytes.len() as u64;
    if bytes.iter().take(BINARY_SNIFF_BYTES).any(|byte| *byte == 0) {
        return DiffSide::Binary { bytes: size };
    }
    match String::from_utf8(bytes) {
        Ok(text) => DiffSide::Text { text, bytes: size },
        Err(_) => DiffSide::InvalidUtf8 { bytes: size },
    }
}

/// Reads one comparison side from the worktree.
///
/// Every step here is anchored to a descriptor rather than to a name, because a
/// name resolves to whatever is there at the moment it is resolved. A symlink
/// is still reported as a link with its target rather than read as though it
/// were the file itself, but no component of the path is ever followed, the
/// size comes from the same descriptor the bytes come from, and the read is
/// bounded — so a classification and the read that acts on it cannot describe
/// different objects.
fn read_worktree_side(worktree: &Path, path: &RepoPath) -> DiffSide {
    let unreadable = |error: &dyn fmt::Display| DiffSide::Unreadable {
        reason: error.to_string(),
    };
    let root = match fs_safety::Root::open(worktree) {
        Ok(root) => root,
        Err(error) => return unreadable(&error),
    };
    let relative = path.as_path();
    let metadata = match fs_safety::metadata_beneath(&root, relative) {
        Ok(metadata) => metadata,
        Err(error) if error.is_not_found() => return DiffSide::Absent,
        Err(error) => return unreadable(&error),
    };
    if metadata.file_type().is_symlink() {
        return match fs_safety::read_link_beneath(&root, relative) {
            Ok(target) => DiffSide::Symlink { target },
            Err(error) => unreadable(&error),
        };
    }
    if !metadata.is_file() {
        return DiffSide::NotRegularFile;
    }

    let mut file = match fs_safety::open_beneath(&root, relative) {
        Ok(file) => file,
        Err(error) if error.is_not_found() => return DiffSide::Absent,
        Err(error) => return unreadable(&error),
    };
    let opened = match fs_safety::metadata_of(&file) {
        Ok(metadata) => metadata,
        Err(error) => return unreadable(&error),
    };
    if !opened.is_file() {
        return DiffSide::NotRegularFile;
    }
    if opened.len() > MAX_DIFF_SIDE_BYTES {
        return DiffSide::Oversize {
            bytes: opened.len(),
        };
    }
    match fs_safety::read_bounded(&mut file, MAX_DIFF_SIDE_BYTES) {
        // `read_bounded` reads one byte past the limit precisely so that "at the
        // limit" and "over it" stay distinguishable. A file that grew after its
        // size was taken from the descriptor lands here.
        Ok(bytes) if bytes.len() as u64 > MAX_DIFF_SIDE_BYTES => DiffSide::Oversize {
            bytes: bytes.len() as u64,
        },
        Ok(bytes) => classify_bytes(bytes),
        Err(error) => unreadable(&error),
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
    #[error("no paths were selected")]
    EmptySelection,
    #[error("commit message is empty")]
    EmptyCommitMessage,
    #[error("remote name is invalid: {0}")]
    InvalidRemote(String),
    #[error("discarding changes requires explicit confirmation")]
    ConfirmationRequired,
    #[error("worktree path must be absolute: {0}")]
    InvalidWorktree(String),
    #[error("repository path is invalid: {0}")]
    InvalidPath(String),
    #[error("{reference} is not a ref remote {remote} knows")]
    ForeignTrackingRef { remote: String, reference: String },
    #[error("{0} names no ref on a remote")]
    NotARemoteSideRef(String),
}

impl From<RepoPathError> for ValidationError {
    fn from(error: RepoPathError) -> Self {
        Self::InvalidPath(error.to_string())
    }
}

impl From<OidError> for ValidationError {
    fn from(error: OidError) -> Self {
        Self::InvalidBase(error.to_string())
    }
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
    #[error("{} is a Git directory with no resolvable work tree", .git_dir.display())]
    NoWorkTree { git_dir: PathBuf },
}

#[derive(Clone)]
pub struct RepositoryService<E = SystemProcessExecutor> {
    git: PathBuf,
    executor: Arc<E>,
}

impl RepositoryService<SystemProcessExecutor> {
    pub fn system(git: impl Into<PathBuf>) -> Self {
        Self::new(git, Arc::new(SystemProcessExecutor))
    }
}

impl<E: ProcessExecutor> RepositoryService<E> {
    pub fn new(git: impl Into<PathBuf>, executor: Arc<E>) -> Self {
        Self {
            git: git.into(),
            executor,
        }
    }

    /// The queries every repository load starts from, in the order
    /// [`Self::discover`] consumes them.
    ///
    /// `--show-toplevel` is deliberately absent. It is the one query that is
    /// not valid everywhere a repository can be selected from: run inside a
    /// Git directory it exits 128 with "this operation must be run in a work
    /// tree", and a reopened project hands its common directory —
    /// `<root>/.git` — straight back here, so batching it in failed a Git call
    /// on every single load. `--is-inside-work-tree` answers the same question
    /// from anywhere without failing, so discovery asks that and resolves the
    /// root separately.
    pub fn discovery_commands(&self, selected_path: &Path) -> Vec<CommandSpec> {
        [
            "--is-bare-repository",
            "--git-common-dir",
            "--git-dir",
            "--is-inside-work-tree",
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

    /// The root of the work tree containing `directory`. Only valid once
    /// [`Self::discovery_commands`] has reported `directory` to be inside one.
    pub fn worktree_root_command(&self, directory: &Path) -> CommandSpec {
        self.git_read(
            directory,
            ["rev-parse", "--path-format=absolute", "--show-toplevel"],
        )
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
                // Every untracked file individually, not just the directory
                // that contains them: staging and discarding act on files.
                "--untracked-files=all",
                "-z",
            ],
        )
    }

    /// Every file the working tree offers: tracked entries plus untracked ones
    /// that are not ignored, which is exactly what the file tree shows.
    ///
    /// `-t` prefixes each record with a status tag, and that tag is what lets
    /// the two be told apart: `-o` is "others", i.e. untracked, so a combined
    /// list counted as tracked overstates the tracked total by exactly the
    /// number of untracked files. `-z` keeps every path byte-exact.
    pub fn working_files_command(&self, worktree: &Path) -> CommandSpec {
        self.git_read(worktree, ["ls-files", "-tco", "--exclude-standard", "-z"])
    }

    /// Reads remote URLs from configuration rather than parsing `git remote
    /// -v`, whose columns are ambiguous for a URL containing whitespace.
    pub fn remotes_command(&self, worktree: &Path) -> CommandSpec {
        self.git_read(worktree, ["config", "-z", "--get-regexp", r"^remote\."])
    }

    pub fn refs_command(&self, worktree: &Path) -> CommandSpec {
        self.git_read(
            worktree,
            [
                "for-each-ref",
                // `%00` separates the fields; a ref name can contain neither a
                // NUL nor a newline, so record framing stays unambiguous.
                "--format=%(refname)%00%(objectname)%00%(symref)%00%(upstream)%00%(HEAD)",
                "refs/heads",
                "refs/remotes",
            ],
        )
    }

    /// Line counts for one uncommitted area. The staged form compares HEAD to
    /// the index and also works on an unborn branch, where Git compares against
    /// the empty tree.
    pub fn numstat_command(&self, worktree: &Path, staged: bool) -> CommandSpec {
        let mut arguments = vec![OsString::from("diff")];
        if staged {
            arguments.push(OsString::from("--cached"));
        }
        arguments.extend([
            OsString::from("--numstat"),
            OsString::from("-z"),
            OsString::from("--find-renames"),
            OsString::from("--no-color"),
        ]);
        self.git_read(worktree, arguments)
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
        // A bare repository has no work tree, so neither of the two remaining
        // queries is asked: `--is-inside-work-tree` has nothing to report and
        // `--show-toplevel` would fail there exactly as it does inside a Git
        // directory.
        let worktree_root = if bare {
            None
        } else {
            let inside_output = self.run_checked(&commands[3], cancellation)?;
            let inside_work_tree = attach_repository_process(
                parse_bool_line(&inside_output.stdout, "inside_work_tree"),
                &inside_output,
            )?;
            Some(if inside_work_tree {
                let command = self.worktree_root_command(selected_path);
                let output = self.run_checked(&command, cancellation)?;
                attach_repository_process(
                    parse_path_line(&output.stdout, "worktree_root"),
                    &output,
                )?
            } else {
                // The selected path is the Git directory itself, which has no
                // current work tree to be asked about. The repository still
                // has one, and a non-bare repository must never be rooted at
                // its Git directory: that is what titles a reopened card
                // `.git` and points its file tree at the repository's own
                // metadata.
                self.worktree_root_of(selected_path, &git_dir, cancellation)?
            })
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

    /// The work tree a Git directory belongs to, resolved from outside every
    /// work tree.
    ///
    /// `git worktree list` is the way in: it answers from inside a Git
    /// directory, and it prints each work tree's own root, so no path
    /// arithmetic is needed. A candidate is matched by asking it for its Git
    /// directory rather than by position or by name — the main work tree's Git
    /// directory is the common directory, a linked work tree's is
    /// `<common>/worktrees/<id>`, and `<id>` cannot be derived from the work
    /// tree's path. A candidate whose directory has been deleted cannot
    /// answer, so its failure is skipped rather than propagated; a
    /// cancellation belongs to the whole request and ends the search.
    fn worktree_root_of(
        &self,
        git_directory: &Path,
        git_dir: &Path,
        cancellation: &CancellationToken,
    ) -> Result<PathBuf, RepositoryError> {
        for candidate in self.list_worktrees(git_directory, cancellation)? {
            if candidate.bare {
                continue;
            }
            let command = self.git_read(
                &candidate.path,
                ["rev-parse", "--path-format=absolute", "--git-dir"],
            );
            let output = match self.run_checked(&command, cancellation) {
                Ok(output) => output,
                Err(failure) if failure.kind == ProcessFailureKind::Cancelled => {
                    return Err(failure.into());
                }
                Err(_) => continue,
            };
            let Ok(candidate_git_dir) = parse_path_line(&output.stdout, "git_dir") else {
                continue;
            };
            if candidate_git_dir.as_path() == git_dir {
                return Ok(candidate.path);
            }
        }
        Err(RepositoryError::NoWorkTree {
            git_dir: git_dir.to_path_buf(),
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

    /// Everything the source-control surfaces need about one worktree.
    pub fn snapshot(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<RepositorySnapshot, RepositoryError> {
        let mut status = self.status(worktree, cancellation)?;
        for (staged, area) in [(true, ChangeArea::Staged), (false, ChangeArea::Unstaged)] {
            let output = self.run_checked(&self.numstat_command(worktree, staged), cancellation)?;
            let parsed = parse_numstat(&output.stdout);
            let records = invalid_output(parsed, output)?;
            merge_numstat(&mut status.entries, area, &records);
        }

        let output = self.run_checked(&self.working_files_command(worktree), cancellation)?;
        let parsed = parse_working_files(&output.stdout);
        let files = invalid_output(parsed, output)?;

        let remotes = self.remotes(worktree, cancellation)?;

        let output = self.run_checked(&self.refs_command(worktree), cancellation)?;
        let parsed = parse_refs(&output.stdout);
        let refs = invalid_output(parsed, output)?;

        Ok(RepositorySnapshot {
            status,
            files,
            remotes,
            local_refs: refs.local,
            remote_refs: refs.remote,
            remote_heads: refs.remote_heads,
        })
    }

    /// The remotes configured for one worktree.
    ///
    /// Separate from [`Self::snapshot`] because a caller that only needs to
    /// know whether a repository has a remote at all must not be made to pay
    /// for a status, two numstats, a file listing and a ref walk.
    pub fn remotes(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Vec<RemoteRecord>, RepositoryError> {
        let command = self.remotes_command(worktree);
        let output = self.run(&command, cancellation)?;
        // `--get-regexp` exits 1 when nothing matches, which only means the
        // repository has no remotes configured.
        if output.success {
            let parsed = parse_remote_config(&output.stdout);
            invalid_output(parsed, output)
        } else if output.exit_code == Some(1) {
            Ok(Vec::new())
        } else {
            Err(git_failure(&command, output).into())
        }
    }

    /// The commit HEAD points at, or `None` on an unborn branch.
    pub fn resolve_head(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Option<Oid>, RepositoryError> {
        self.resolve_optional_revision(worktree, "HEAD", cancellation)
    }

    /// Resolves a revision to a commit id, or `None` when it does not exist.
    ///
    /// `--quiet` makes an unresolvable revision exit 1 with no output, which is
    /// how an unborn branch is distinguished from a broken repository.
    fn resolve_optional_revision(
        &self,
        worktree: &Path,
        revision: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<Oid>, RepositoryError> {
        let command = self.git_read(
            worktree,
            [
                OsString::from("rev-parse"),
                OsString::from("--verify"),
                OsString::from("--quiet"),
                OsString::from("--end-of-options"),
                format!("{revision}^{{commit}}").into(),
            ],
        );
        let output = self.run(&command, cancellation)?;
        if !output.success {
            return if output.exit_code == Some(1) {
                Ok(None)
            } else {
                Err(git_failure(&command, output).into())
            };
        }
        let line = parse_utf8_line(&output.stdout, "revision")?;
        Oid::parse_object(line)
            .map(Some)
            .map_err(|error| RepositoryError::InvalidOutput {
                field: "revision",
                reason: error.to_string(),
            })
    }

    /// The `git log` page for one history request.
    pub fn history_command(
        &self,
        worktree: &Path,
        start: &Oid,
        page: u32,
        path: Option<&RepoPath>,
    ) -> CommandSpec {
        let mut arguments = vec![
            OsString::from("log"),
            OsString::from("--no-color"),
            OsString::from("-z"),
            format!("--format={HISTORY_FORMAT}").into(),
            OsString::from("--date=raw"),
            format!("--skip={}", u64::from(page) * u64::from(HISTORY_PAGE_SIZE)).into(),
            format!("--max-count={HISTORY_PAGE_SIZE}").into(),
            OsString::from(start.as_str()),
        ];
        if let Some(path) = path {
            arguments.push(OsString::from("--"));
            arguments.push(path.as_os_str().to_owned());
        }
        self.git_literal_read(worktree, arguments)
    }

    /// One page of history, newest first.
    pub fn history(
        &self,
        request: &HistoryRequest,
        cancellation: &CancellationToken,
    ) -> Result<Vec<CommitSummary>, RepositoryError> {
        let start = match &request.revision {
            // Resolved before walking so that every page of one paging session
            // is taken from the same starting commit, and so that an unborn
            // branch is an empty history rather than a `git log` failure.
            HistoryRevision::Head => match self.resolve_head(&request.worktree, cancellation)? {
                Some(oid) => oid,
                None => return Ok(Vec::new()),
            },
            HistoryRevision::Object { oid } => oid.clone(),
            HistoryRevision::Reference { name } => {
                match self.resolve_optional_revision(
                    &request.worktree,
                    name.as_str(),
                    cancellation,
                )? {
                    Some(oid) => oid,
                    None => return Ok(Vec::new()),
                }
            }
        };
        let command = self.history_command(
            &request.worktree,
            &start,
            request.page,
            request.path.as_ref(),
        );
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_history(&output.stdout);
        invalid_output(parsed, output)
    }

    /// The merge base of `base_ref` and HEAD, which is what a branch
    /// comparison must diff against: comparing against the base tip would also
    /// show every commit the base gained since this branch left it.
    pub fn resolve_compare_base(
        &self,
        worktree: &Path,
        base_ref: &ValidatedRefName,
        cancellation: &CancellationToken,
    ) -> Result<Oid, RepositoryError> {
        let command = self.git_read(
            worktree,
            [
                OsString::from("merge-base"),
                base_ref.as_str().into(),
                OsString::from("HEAD"),
            ],
        );
        let output = self.run_checked(&command, cancellation)?;
        let line = parse_utf8_line(&output.stdout, "merge_base")?;
        Oid::parse_object(line).map_err(|error| RepositoryError::InvalidOutput {
            field: "merge_base",
            reason: error.to_string(),
        })
    }

    /// Files that differ between the merge base and HEAD.
    pub fn branch_diff_entries(
        &self,
        worktree: &Path,
        base_ref: &ValidatedRefName,
        cancellation: &CancellationToken,
    ) -> Result<Vec<DiffEntry>, RepositoryError> {
        let base = self.resolve_compare_base(worktree, base_ref, cancellation)?;
        let head =
            self.resolve_head(worktree, cancellation)?
                .ok_or(RepositoryError::InvalidOutput {
                    field: "head",
                    reason: "an unborn branch has nothing to compare".into(),
                })?;
        self.range_entries(worktree, Some(&base), &head, cancellation)
    }

    /// Files a commit changed relative to `parent`, or to the empty tree when
    /// `parent` is `None`.
    pub fn commit_diff_entries(
        &self,
        worktree: &Path,
        oid: &Oid,
        parent: Option<&Oid>,
        cancellation: &CancellationToken,
    ) -> Result<Vec<DiffEntry>, RepositoryError> {
        self.range_entries(worktree, parent, oid, cancellation)
    }

    fn range_entries(
        &self,
        worktree: &Path,
        old: Option<&Oid>,
        new: &Oid,
        cancellation: &CancellationToken,
    ) -> Result<Vec<DiffEntry>, RepositoryError> {
        let command = self.range_diff_command(worktree, old, new, "--name-status");
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_name_status(&output.stdout);
        let mut entries = invalid_output(parsed, output)?;
        let command = self.range_diff_command(worktree, old, new, "--numstat");
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_numstat(&output.stdout);
        let records = invalid_output(parsed, output)?;
        merge_diff_numstat(&mut entries, &records);
        Ok(entries)
    }

    fn range_diff_command(
        &self,
        worktree: &Path,
        old: Option<&Oid>,
        new: &Oid,
        format: &str,
    ) -> CommandSpec {
        let mut arguments = vec![
            OsString::from("diff-tree"),
            OsString::from("-r"),
            OsString::from("-z"),
            OsString::from("--no-commit-id"),
            OsString::from("--find-renames"),
            OsString::from("--no-color"),
            OsString::from(format),
        ];
        match old {
            Some(old) => {
                arguments.extend([OsString::from(old.as_str()), OsString::from(new.as_str())])
            }
            // `--root` compares a root commit against the empty tree without
            // this service having to know the repository's hash algorithm and
            // therefore its empty-tree id.
            None => arguments.extend([OsString::from("--root"), OsString::from(new.as_str())]),
        }
        self.git_read(worktree, arguments)
    }

    /// Reads both sides of one path's comparison.
    ///
    /// Blobs are read lazily, one path at a time, because a repository-wide
    /// prefetch would read megabytes the user never looks at.
    pub fn diff(
        &self,
        request: &DiffRequest,
        cancellation: &CancellationToken,
    ) -> Result<DiffContent, RepositoryError> {
        let worktree = request.worktree.as_path();
        let old_path = request.old_path.as_ref().unwrap_or(&request.path);
        match &request.basis {
            DiffBasis::IndexToWorktree => {
                let stages = self.index_entries(worktree, old_path, cancellation)?;
                let worktree_side = read_worktree_side(worktree, &request.path);
                let stage = |number: u8| stages.iter().find(|entry| entry.stage == number);
                if stage(0).is_none() && (1..=3).any(|number| stage(number).is_some()) {
                    // An unmerged path has no stage 0. Treating that absence as
                    // an empty old side would claim the file was added, so the
                    // three conflict stages are reported instead.
                    return Ok(DiffContent::Unmerged {
                        base: self.entry_side(worktree, stage(1), cancellation)?,
                        ours: self.entry_side(worktree, stage(2), cancellation)?,
                        theirs: self.entry_side(worktree, stage(3), cancellation)?,
                        worktree: worktree_side,
                    });
                }
                Ok(DiffContent::Comparison {
                    old: self.entry_side(worktree, stage(0), cancellation)?,
                    new: worktree_side,
                    old_label: "Index".into(),
                    new_label: "Working tree".into(),
                })
            }
            DiffBasis::HeadToIndex => {
                let old = match self.resolve_head(worktree, cancellation)? {
                    Some(head) => self.tree_side(worktree, &head, old_path, cancellation)?,
                    None => DiffSide::Absent,
                };
                let stages = self.index_entries(worktree, &request.path, cancellation)?;
                let staged = stages.iter().find(|entry| entry.stage == 0);
                Ok(DiffContent::Comparison {
                    old,
                    new: self.entry_side(worktree, staged, cancellation)?,
                    old_label: "HEAD".into(),
                    new_label: "Index".into(),
                })
            }
            DiffBasis::MergeBaseToHead { base_ref } => {
                let base = self.resolve_compare_base(worktree, base_ref, cancellation)?;
                let head = self.resolve_head(worktree, cancellation)?.ok_or(
                    RepositoryError::InvalidOutput {
                        field: "head",
                        reason: "an unborn branch has nothing to compare".into(),
                    },
                )?;
                Ok(DiffContent::Comparison {
                    old: self.tree_side(worktree, &base, old_path, cancellation)?,
                    new: self.tree_side(worktree, &head, &request.path, cancellation)?,
                    old_label: format!("{} ({})", base_ref.short_name(), base.short()),
                    new_label: "HEAD".into(),
                })
            }
            DiffBasis::CommitToParent { oid, parent } => {
                let old = match parent {
                    Some(parent) => self.tree_side(worktree, parent, old_path, cancellation)?,
                    None => DiffSide::Absent,
                };
                Ok(DiffContent::Comparison {
                    old,
                    new: self.tree_side(worktree, oid, &request.path, cancellation)?,
                    old_label: match parent {
                        Some(parent) => parent.short().to_owned(),
                        None => "root commit".into(),
                    },
                    new_label: oid.short().to_owned(),
                })
            }
        }
    }

    /// Index entries for exactly one path, including every conflict stage.
    fn index_entries(
        &self,
        worktree: &Path,
        path: &RepoPath,
        cancellation: &CancellationToken,
    ) -> Result<Vec<IndexEntry>, RepositoryError> {
        let command = self.git_literal_read(
            worktree,
            [
                OsString::from("ls-files"),
                OsString::from("-s"),
                OsString::from("-z"),
                OsString::from("--"),
                path.as_os_str().to_owned(),
            ],
        );
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_index_entries(&output.stdout);
        let entries = invalid_output(parsed, output)?;
        // A pathspec also matches everything beneath a directory of the same
        // name, so only the exact path is kept.
        Ok(entries
            .into_iter()
            .filter(|entry| entry.path == *path)
            .collect())
    }

    fn entry_side(
        &self,
        worktree: &Path,
        entry: Option<&IndexEntry>,
        cancellation: &CancellationToken,
    ) -> Result<DiffSide, RepositoryError> {
        match entry {
            Some(entry) => self.object_side(worktree, entry.mode, &entry.oid, cancellation),
            None => Ok(DiffSide::Absent),
        }
    }

    fn tree_side(
        &self,
        worktree: &Path,
        revision: &Oid,
        path: &RepoPath,
        cancellation: &CancellationToken,
    ) -> Result<DiffSide, RepositoryError> {
        let command = self.git_literal_read(
            worktree,
            [
                OsString::from("ls-tree"),
                OsString::from("-z"),
                OsString::from(revision.as_str()),
                OsString::from("--"),
                path.as_os_str().to_owned(),
            ],
        );
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_tree_entries(&output.stdout);
        let entries = invalid_output(parsed, output)?;
        match entries.into_iter().find(|entry| entry.path == *path) {
            Some(entry) => self.object_side(worktree, entry.mode, &entry.oid, cancellation),
            None => Ok(DiffSide::Absent),
        }
    }

    /// One side read from the object database, classified by its file mode.
    fn object_side(
        &self,
        worktree: &Path,
        mode: u32,
        oid: &Oid,
        cancellation: &CancellationToken,
    ) -> Result<DiffSide, RepositoryError> {
        if mode == GITLINK_MODE {
            // A gitlink's object is a commit in another repository, not
            // content this repository can read.
            return Ok(DiffSide::Submodule {
                oid: Some(oid.clone()),
            });
        }
        let command = self
            .git_read(worktree, ["cat-file", "--batch"])
            .stdin(format!("{oid}\n").into_bytes())
            // The header arrives before the content, so even a truncated read
            // still carries the true size and can report oversize precisely.
            .output_limits(
                MAX_DIFF_SIDE_BYTES as usize + BATCH_HEADER_SLACK,
                DEFAULT_STDERR_LIMIT,
            );
        let output = match self.executor.execute(&command, cancellation) {
            Ok(output) if output.success => output,
            Ok(output) => return Err(git_failure(&command, output).into()),
            // An object too large for one side of a comparison is an answer,
            // not a failure, so this one kind is deliberately not logged as
            // one.
            Err(failure) if failure.kind == ProcessFailureKind::OutputLimit => {
                return Ok(DiffSide::Oversize {
                    bytes: batch_header_size(&failure.stdout).unwrap_or(MAX_DIFF_SIDE_BYTES + 1),
                });
            }
            Err(failure) => {
                log_git_failure(&command, &failure);
                return Err(failure.into());
            }
        };
        let parsed = parse_batch_blobs(&output.stdout, "git cat-file --batch");
        let mut objects = invalid_output(parsed, output)?;
        let bytes = match objects.pop() {
            Some(BatchObject::Content(bytes)) => bytes,
            // Git understood itself perfectly well; the object simply is not
            // here. Reporting that as malformed output blamed the wrong thing
            // and hid the only actionable fact, which is that this side of the
            // comparison cannot be read from this repository.
            Some(BatchObject::Missing) => {
                return Ok(DiffSide::Unreadable {
                    reason: format!("{oid} is not in this repository"),
                });
            }
            None => {
                return Err(RepositoryError::InvalidOutput {
                    field: "cat_file_batch",
                    reason: "no object was returned".into(),
                });
            }
        };
        if bytes.len() as u64 > MAX_DIFF_SIDE_BYTES {
            return Ok(DiffSide::Oversize {
                bytes: bytes.len() as u64,
            });
        }
        if mode == SYMLINK_MODE {
            // A symlink's blob is its target, which is a path rather than file
            // content and must keep its bytes.
            return Ok(DiffSide::Symlink {
                target: bytes_to_path(&bytes),
            });
        }
        Ok(classify_bytes(bytes))
    }

    /// The `git add` invocation for an already-resolved pathspec list.
    pub fn stage_command(
        &self,
        worktree: &Path,
        paths: &[RepoPath],
    ) -> Result<CommandSpec, ValidationError> {
        if paths.is_empty() {
            return Err(ValidationError::EmptySelection);
        }
        Ok(self.git_literal_mutation(worktree, pathspec_arguments(["add", "-A"], paths)))
    }

    /// Stages the selected paths, including the deletion half of a rename the
    /// index still has.
    pub fn stage(
        &self,
        request: &StageRequest,
        cancellation: &CancellationToken,
    ) -> Result<MutationOutcome, RepositoryError> {
        let paths =
            self.matchable_pathspecs(&request.worktree, &request.selection, cancellation)?;
        let command = self.stage_command(&request.worktree, &paths)?;
        Ok(self.run_mutation(&command, paths, MutationScope::Incremental, cancellation))
    }

    /// The pathspecs a worktree-side mutation can actually match.
    ///
    /// `git add` and `git restore --worktree` resolve a pathspec against the
    /// index and the working tree, never against HEAD. The source half of a
    /// *staged* rename is in neither — only HEAD still has that name — so
    /// listing it makes `add` exit 128 having staged nothing and `restore` exit
    /// 1 having restored nothing, for the whole selection. Whether the source
    /// is matchable is a property of the index at this moment rather than of
    /// the selection, which is why it is asked here and not at the point the
    /// selection was built: the same [`PathSelection`] names an unmatchable
    /// source while the rename is staged and a matchable one once it is not,
    /// and the unstaged rename genuinely has to be staged as a pair.
    fn matchable_pathspecs(
        &self,
        worktree: &Path,
        selection: &[PathSelection],
        cancellation: &CancellationToken,
    ) -> Result<Vec<RepoPath>, RepositoryError> {
        let sources: Vec<RepoPath> = selection
            .iter()
            .filter_map(|entry| entry.old_path.clone())
            .collect();
        let indexed = if sources.is_empty() {
            Vec::new()
        } else {
            self.indexed_paths(worktree, &sources, cancellation)?
        };
        let mut paths = Vec::with_capacity(selection.len() + indexed.len());
        for entry in selection {
            let source = entry
                .old_path
                .as_ref()
                .filter(|source| indexed.contains(source));
            for path in [source, Some(&entry.path)].into_iter().flatten() {
                if !paths.contains(path) {
                    paths.push(path.clone());
                }
            }
        }
        Ok(paths)
    }

    pub fn unstage(
        &self,
        request: &UnstageRequest,
        cancellation: &CancellationToken,
    ) -> Result<MutationOutcome, RepositoryError> {
        let paths = selection_pathspecs(&request.selection);
        if paths.is_empty() {
            return Err(ValidationError::EmptySelection.into());
        }
        if self
            .resolve_head(&request.worktree, cancellation)?
            .is_some()
        {
            let command = self.git_literal_mutation(
                &request.worktree,
                pathspec_arguments(["restore", "--staged"], &paths),
            );
            return Ok(self.run_mutation(
                &command,
                paths,
                MutationScope::AllOrNothing,
                cancellation,
            ));
        }
        // An unborn branch has no HEAD to restore from, so unstaging means
        // removing the index entry — and only for paths that are really in the
        // index, because `git rm --cached` on an unknown path is an error.
        let indexed = self.indexed_paths(&request.worktree, &paths, cancellation)?;
        if indexed.is_empty() {
            return Ok(MutationOutcome::Applied { paths: Vec::new() });
        }
        let command = self.git_literal_mutation(
            &request.worktree,
            pathspec_arguments(["rm", "--cached", "--ignore-unmatch"], &indexed),
        );
        Ok(self.run_mutation(&command, indexed, MutationScope::AllOrNothing, cancellation))
    }

    /// Filters a selection down to the paths the index actually contains,
    /// preserving the caller's order.
    fn indexed_paths(
        &self,
        worktree: &Path,
        paths: &[RepoPath],
        cancellation: &CancellationToken,
    ) -> Result<Vec<RepoPath>, RepositoryError> {
        let mut arguments = vec![
            OsString::from("ls-files"),
            OsString::from("-z"),
            OsString::from("--"),
        ];
        arguments.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
        let command = self.git_literal_read(worktree, arguments);
        let output = self.run_checked(&command, cancellation)?;
        let parsed = parse_path_list(&output.stdout, "git ls-files");
        let listed = invalid_output(parsed, output)?;
        Ok(paths
            .iter()
            .filter(|path| listed.contains(path))
            .cloned()
            .collect())
    }

    /// Restores tracked paths from the index and deletes untracked files.
    ///
    /// This destroys work that exists nowhere else, so it refuses to run
    /// without the caller's confirmation and never removes a directory.
    pub fn discard(
        &self,
        request: &DiscardRequest,
        cancellation: &CancellationToken,
    ) -> Result<MutationOutcome, RepositoryError> {
        if !request.confirmed {
            return Err(ValidationError::ConfirmationRequired.into());
        }
        if !request.worktree.is_absolute() {
            return Err(
                ValidationError::InvalidWorktree(request.worktree.display().to_string()).into(),
            );
        }
        let tracked =
            self.matchable_pathspecs(&request.worktree, &request.tracked, cancellation)?;
        let mut untracked: Vec<RepoPath> = Vec::with_capacity(request.untracked.len());
        for path in &request.untracked {
            if !untracked.contains(path) {
                untracked.push(path.clone());
            }
        }
        if tracked.is_empty() && untracked.is_empty() {
            return Err(ValidationError::EmptySelection.into());
        }

        let mut applied = Vec::new();
        if !tracked.is_empty() {
            let command = self.git_literal_mutation(
                &request.worktree,
                pathspec_arguments(["restore", "--worktree"], &tracked),
            );
            // A `restore` that reports a refusal changed nothing, and one that
            // failed inside its write loop changed an amount only the user can
            // establish. Neither leaves a half of the request that this may
            // finish on its own: unlinking the untracked half afterwards would
            // destroy files as part of an operation the caller is being told
            // did not happen, or is being told to go and check.
            match self.run_mutation(
                &command,
                tracked,
                MutationScope::AtomicRefusal,
                cancellation,
            ) {
                MutationOutcome::Applied { mut paths } => applied.append(&mut paths),
                outcome => return Ok(outcome),
            }
        }
        if untracked.is_empty() {
            return Ok(MutationOutcome::Applied { paths: applied });
        }

        // `request.untracked` was chosen from a snapshot the user has since been
        // looking at in a confirmation dialog. Another Git client may have
        // staged or committed one of those paths in the meantime, and unlinking
        // a *tracked* file is not what the user confirmed — so the
        // classification is re-established here, immediately before the
        // deletions, from the index rather than from the stale snapshot.
        let tracked_now = self.indexed_paths(&request.worktree, &untracked, cancellation)?;

        // Opened once, and every unlink below is anchored to this descriptor
        // rather than to the worktree's name.
        let root = match fs_safety::Root::open(&request.worktree) {
            Ok(root) => root,
            Err(error) => {
                // Attributed to each path rather than reported once, so every
                // failure a discard reports is path-scoped and disjoint from
                // the paths it applied: the caller can always retry exactly
                // the remainder.
                let reason = format!("{}: {error}", request.worktree.display());
                let failures = untracked
                    .into_iter()
                    .map(|path| MutationFailure::Path {
                        path,
                        reason: reason.clone(),
                    })
                    .collect();
                return Ok(report_discard(applied, failures));
            }
        };
        // Every selected path is attempted. Git reports an untracked nested
        // repository as its directory, and a directory is one thing a discard
        // will never unlink — so stopping at the first refusal made the set of
        // files that survived depend on nothing but where that directory's name
        // happened to sort among them.
        let mut failures = Vec::new();
        for path in untracked {
            if tracked_now.contains(&path) {
                failures.push(MutationFailure::Path {
                    path,
                    reason: "the path is tracked now but was untracked when it was selected".into(),
                });
                continue;
            }
            match delete_untracked(&root, &path) {
                Ok(()) => applied.push(path),
                Err(failure) => failures.push(failure),
            }
        }
        Ok(report_discard(applied, failures))
    }

    pub fn commit_command(&self, request: &CommitRequest) -> Result<CommandSpec, ValidationError> {
        if request.message.trim().is_empty() {
            return Err(ValidationError::EmptyCommitMessage);
        }
        Ok(self
            .git_mutation(
                &request.worktree,
                // The message travels on stdin so that no length limit,
                // quoting rule, or temporary file is involved.
                //
                // `whitespace`, not `strip`, and for the same reason Git itself
                // defaults `--file` to it: `strip` removes every line beginning
                // with `core.commentChar`, which is what an editor session
                // needs and is silent data loss for a message that was never in
                // an editor. A message beginning `#42 fix the thing` would be
                // committed without its first line, and one written entirely in
                // such lines would be committed as empty.
                ["commit", "--file=-", "--cleanup=whitespace"],
            )
            .stdin(request.message.clone().into_bytes()))
    }

    /// True when the index differs from HEAD.
    ///
    /// `git diff --cached --quiet` exits 1 to mean "there are differences", and
    /// works on an unborn branch because Git compares against the empty tree.
    pub fn has_staged_changes(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<bool, RepositoryError> {
        let command = self.git_read(worktree, ["diff", "--cached", "--quiet"]);
        let output = self.run(&command, cancellation)?;
        match output.exit_code {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(git_failure(&command, output).into()),
        }
    }

    /// The sequencer state files whose presence means Git will accept a commit
    /// with an index identical to HEAD's tree.
    ///
    /// These are per-worktree pseudo-refs, so their location comes from Git
    /// rather than from an assumed `.git/` layout: a linked worktree keeps them
    /// in its own administrative directory.
    const PENDING_COMMIT_STATES: [&'static str; 3] =
        ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"];

    /// True when Git has a merge, cherry-pick or revert waiting to be
    /// concluded by a commit.
    ///
    /// This is how `git commit` itself decides, and it is what makes the
    /// "nothing staged" refusal correct rather than merely usual: a conflict
    /// resolved in favour of ours leaves the index equal to HEAD's tree, and
    /// `git status` reports nothing changed, yet the merge commit is still
    /// required and plain `git commit` still creates it.
    pub fn has_pending_commit_state(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<bool, RepositoryError> {
        let mut arguments = vec![OsString::from("rev-parse")];
        for state in Self::PENDING_COMMIT_STATES {
            arguments.push(OsString::from("--git-path"));
            arguments.push(OsString::from(state));
        }
        let command = self.git_read(worktree, arguments);
        let output = self.run_checked(&command, cancellation)?;
        Ok(output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(bytes_to_path)
            // `--git-path` answers relative to the working directory it ran
            // in, which is the worktree.
            .any(|path| worktree.join(path).exists()))
    }

    /// Commits the index, reconciling the outcome against HEAD.
    pub fn commit(
        &self,
        request: &CommitRequest,
        cancellation: &CancellationToken,
    ) -> Result<CommitOutcome, RepositoryError> {
        let command = self.commit_command(request)?;
        if !self.has_staged_changes(&request.worktree, cancellation)?
            && !self.has_pending_commit_state(&request.worktree, cancellation)?
        {
            return Ok(CommitOutcome::NotCreated {
                failure: MutationFailure::Rejected {
                    reason: "there are no staged changes to commit".into(),
                },
            });
        }
        let before = self.resolve_head(&request.worktree, cancellation)?;
        let failure = match self.run(&command, cancellation) {
            Ok(output) if output.success => None,
            Ok(output) => Some(MutationFailure::from(git_failure(&command, output))),
            // A process that never started cannot have committed.
            Err(failure) if !failure.started => {
                return Ok(CommitOutcome::NotCreated {
                    failure: failure.into(),
                });
            }
            Err(failure) => Some(MutationFailure::from(failure)),
        };
        // HEAD is the only authority on whether the commit exists: a failing
        // post-commit hook, or a cancellation after `git commit` already wrote
        // the object, both leave a real commit behind. The reconciliation runs
        // under a fresh token so a cancelled operation still learns the truth.
        let reconciliation = CancellationToken::new();
        let after = match self.resolve_head(&request.worktree, &reconciliation) {
            Ok(after) => after,
            Err(error) => {
                return Ok(CommitOutcome::OutcomeUnknown {
                    failure: failure.unwrap_or_else(|| {
                        MutationFailure::Process(repository_warning(&self.git, error))
                    }),
                });
            }
        };
        let moved = match &after {
            Some(oid) if Some(oid) != before.as_ref() => Some(oid),
            _ => None,
        };
        let Some(oid) = moved else {
            return Ok(match failure {
                Some(failure) => CommitOutcome::NotCreated { failure },
                None => CommitOutcome::OutcomeUnknown {
                    failure: MutationFailure::Rejected {
                        reason: "git reported success but HEAD did not move".into(),
                    },
                },
            });
        };
        let Some(failure) = failure else {
            // Git exited 0, which is Git saying it made this commit, and the
            // ref it moved is the one it made. A hook is entitled to rewrite
            // the message Git stored — prepending a ticket prefix is the whole
            // purpose of `prepare-commit-msg`, and reflowing one is a
            // `commit-msg` hook's — so no comparison against the message this
            // call sent can be the authority here. Making one the authority
            // reported every commit in such a repository as a failure.
            return Ok(CommitOutcome::Created {
                oid: oid.clone(),
                warnings: Vec::new(),
            });
        };
        // Git failed and HEAD moved anyway. Either this call's commit object
        // was written before whatever failed — a cancellation arriving after
        // `git commit` had already updated the ref — or the user's own
        // terminal committed in this worktree, which `MutationLocks` cannot
        // exclude, and claiming that object id would attribute a stranger's
        // commit, with a stranger's message and contents, to this call.
        //
        // The stored message is the only evidence there is, and it points one
        // way only: finding the message this call sent identifies the commit
        // as this call's, while not finding it establishes nothing, because a
        // hook may have rewritten it. So a match is reported as created with
        // the failure demoted to a warning, and everything else — a mismatch,
        // or a reconciliation read that failed itself — is the unknown outcome
        // it really is, rather than a claim in either direction. A read is
        // never allowed to turn a mutation that completed into an error.
        if matches!(
            self.carries_sent_message(&request.worktree, oid, &request.message, &reconciliation),
            Ok(true)
        ) {
            return Ok(CommitOutcome::Created {
                oid: oid.clone(),
                warnings: vec![failure],
            });
        }
        Ok(CommitOutcome::OutcomeUnknown { failure })
    }

    /// True when the commit at `oid` carries the message this call sent.
    ///
    /// Positive evidence of authorship and nothing more. A `commit-msg` hook
    /// may append to a message — a `Change-Id` trailer is the common case — so
    /// the sent message is looked for as a prefix rather than as an equal, and
    /// both sides are cleaned the way `--cleanup=whitespace` cleaned the one
    /// Git stored. A hook that *prepends*, *reflows* or replaces the message
    /// makes this false for a commit this call did write, which is why a false
    /// answer is never read as "someone else wrote it".
    fn carries_sent_message(
        &self,
        worktree: &Path,
        oid: &Oid,
        message: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, RepositoryError> {
        let command = self.git_read(
            worktree,
            [
                OsString::from("log"),
                OsString::from("-z"),
                OsString::from("--max-count=1"),
                OsString::from("--format=%B"),
                OsString::from("--end-of-options"),
                OsString::from(oid.as_str()),
            ],
        );
        let output = self.run_checked(&command, cancellation)?;
        // `-z` terminates the record, so the stored message is exactly what
        // precedes the NUL and no trailing newline has to be guessed at.
        let stored = output.stdout.strip_suffix(&[0]).unwrap_or(&output.stdout);
        Ok(cleanup_whitespace(&String::from_utf8_lossy(stored))
            .starts_with(&cleanup_whitespace(message)))
    }

    /// The branch's configured upstream as an explicit push target, or `None`
    /// when it has no upstream and the user must choose one.
    pub fn upstream_push_target(
        &self,
        worktree: &Path,
        branch: &ValidatedBranchName,
        cancellation: &CancellationToken,
    ) -> Result<Option<PushTarget>, RepositoryError> {
        let remote = self.branch_config(worktree, branch, "remote", cancellation)?;
        let merge = self.branch_config(worktree, branch, "merge", cancellation)?;
        let (Some(remote), Some(merge)) = (remote, merge) else {
            return Ok(None);
        };
        validate_remote_name(&remote)?;
        Ok(Some(PushTarget::Upstream {
            remote,
            remote_ref: ValidatedRefName::parse(merge)?,
        }))
    }

    fn branch_config(
        &self,
        worktree: &Path,
        branch: &ValidatedBranchName,
        key: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, RepositoryError> {
        let command = self.git_read(
            worktree,
            [
                OsString::from("config"),
                OsString::from("-z"),
                OsString::from("--get"),
                format!("branch.{}.{key}", branch.as_str()).into(),
            ],
        );
        let output = self.run(&command, cancellation)?;
        if !output.success {
            return if output.exit_code == Some(1) {
                Ok(None)
            } else {
                Err(git_failure(&command, output).into())
            };
        }
        let value = output
            .stdout
            .strip_suffix(&[0])
            .unwrap_or(&output.stdout)
            .to_vec();
        if value.is_empty() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&value).into_owned()))
    }

    /// Validates a proposed remote branch name with Git itself.
    pub fn check_ref_format_command(&self, worktree: &Path, reference: &str) -> CommandSpec {
        self.git_read(
            worktree,
            [OsString::from("check-ref-format"), reference.into()],
        )
    }

    pub fn push_command(&self, request: &PushRequest) -> Result<CommandSpec, ValidationError> {
        self.push_command_to(request, &request.target.remote_side_ref()?)
    }

    /// The push command for one already-resolved destination, so the ref that
    /// goes on the wire and the ref every outcome check consults are one value
    /// rather than two derivations that could drift apart.
    fn push_command_to(
        &self,
        request: &PushRequest,
        destination: &RemoteSideRef,
    ) -> Result<CommandSpec, ValidationError> {
        validate_remote_name(request.target.remote())?;
        let mut arguments = vec![OsString::from("push"), OsString::from("--porcelain")];
        if request.target.sets_upstream() {
            arguments.push(OsString::from("--set-upstream"));
        }
        arguments.push(OsString::from(request.target.remote()));
        // An explicit refspec, never a push default: the remote-side ref this
        // updates must be the one the user was shown. `--set-upstream` records
        // this refspec's own destination as `branch.<name>.merge`, so the
        // fully qualified `refs/heads/<branch>` here is also what makes the
        // tracking configuration land in the form Git expects to read back.
        // Force is never offered.
        arguments.push(push_refspec(&request.branch, destination).into());
        Ok(self.git_mutation(&request.worktree, arguments))
    }

    /// Pushes one branch, reconciling an uncertain outcome against the remote.
    pub fn push(
        &self,
        request: &PushRequest,
        cancellation: &CancellationToken,
    ) -> Result<PushOutcome, RepositoryError> {
        // Resolved exactly once: the refspec Git is given, the name Git is
        // asked to validate, the ref reconciliation looks up on the remote,
        // and the ref this reports as updated are all this one value.
        let destination = request.target.remote_side_ref()?;
        let command = self.push_command_to(request, &destination)?;
        if request.target.sets_upstream() {
            // The remote-side name is user text on a first publish, so Git
            // validates it before anything leaves the machine.
            let check = self.check_ref_format_command(&request.worktree, destination.as_str());
            if !self.run(&check, cancellation)?.success {
                return Ok(PushOutcome::NotPushed {
                    failure: MutationFailure::Rejected {
                        reason: format!("{destination} is not a valid ref name"),
                    },
                });
            }
        }
        // Load-bearing beyond reporting the branch's object id: this is the
        // check that stops a branch label recovered lossily from Git's own
        // output — see the `branch.head` note in `parse_status` — from ever
        // reaching a `git push`. `ValidatedBranchName::parse` screens ASCII
        // and lets U+FFFD through, so refusing a name that names no ref is
        // what keeps a name this application cannot spell out of a refspec.
        let local_ref = request.branch.full_ref();
        let Some(local_oid) =
            self.resolve_optional_revision(&request.worktree, &local_ref, cancellation)?
        else {
            return Ok(PushOutcome::NotPushed {
                failure: MutationFailure::Rejected {
                    reason: format!("{local_ref} does not exist"),
                },
            });
        };
        let refspec = push_refspec(&request.branch, &destination);
        let pushed = |oid: Oid| PushOutcome::Pushed {
            remote: request.target.remote().to_owned(),
            remote_ref: destination.clone(),
            oid,
        };
        let failure = match self.run(&command, cancellation) {
            Ok(output) if output.success => return Ok(pushed(local_oid)),
            Ok(output) => {
                let rejected = push_ref_flag(&output.stdout, &refspec) == Some(b'!');
                let failure = git_failure(&command, output);
                if rejected {
                    // `!` is Git reporting that the remote refused this exact
                    // update, so the remote ref definitively did not move and
                    // there is nothing ambiguous to reconcile.
                    return Ok(PushOutcome::NotPushed {
                        failure: failure.into(),
                    });
                }
                MutationFailure::from(failure)
            }
            Err(failure) if !failure.started => {
                return Ok(PushOutcome::NotPushed {
                    failure: failure.into(),
                });
            }
            Err(failure) => {
                if push_ref_flag(&failure.stdout, &refspec) == Some(b'!') {
                    return Ok(PushOutcome::NotPushed {
                        failure: failure.into(),
                    });
                }
                MutationFailure::from(failure)
            }
        };
        // The push may have completed at the remote before the local process
        // died, so the remote's own ref is consulted under a fresh token —
        // reconciliation has to run even when the original token is cancelled.
        let reconciliation = CancellationToken::new();
        match self.remote_ref_oid(
            &request.worktree,
            request.target.remote(),
            &destination,
            &reconciliation,
        ) {
            Ok(Some(remote_oid)) if remote_oid == local_oid => Ok(pushed(local_oid)),
            _ => Ok(PushOutcome::OutcomeUnknown { failure }),
        }
    }

    /// The object id a remote currently has for one ref, or `None` when the
    /// remote does not have it.
    ///
    /// The ref is named the way the remote names it, because that is the only
    /// name `ls-remote` can match: asking a remote for
    /// `refs/remotes/origin/main` matches nothing there and reads back as
    /// "the remote does not have it".
    pub fn remote_ref_oid(
        &self,
        worktree: &Path,
        remote: &str,
        remote_ref: &RemoteSideRef,
        cancellation: &CancellationToken,
    ) -> Result<Option<Oid>, RepositoryError> {
        validate_remote_name(remote)?;
        let command = self.git_read(
            worktree,
            [
                OsString::from("ls-remote"),
                OsString::from("--exit-code"),
                OsString::from(remote),
                OsString::from(remote_ref.as_str()),
            ],
        );
        let output = self.run(&command, cancellation)?;
        if !output.success {
            // 2 is `--exit-code`'s "no matching ref"; anything else is a
            // genuine lookup failure and must not be read as absence.
            return if output.exit_code == Some(2) {
                Ok(None)
            } else {
                Err(git_failure(&command, output).into())
            };
        }
        let line = output
            .stdout
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or_default();
        let oid = line.split(|byte| *byte == b'\t').next().unwrap_or_default();
        let oid = std::str::from_utf8(oid).map_err(|error| RepositoryError::InvalidOutput {
            field: "ls_remote_oid",
            reason: error.to_string(),
        })?;
        Oid::parse_object(oid)
            .map(Some)
            .map_err(|error| RepositoryError::InvalidOutput {
                field: "ls_remote_oid",
                reason: error.to_string(),
            })
    }

    /// Runs one mutation command and classifies the outcome.
    ///
    /// A command that never started cannot have touched the repository, so it
    /// is definitively `NotApplied`. What a *non-zero exit* means depends on
    /// the command, which is what `scope` carries; a command that died after
    /// launch is `OutcomeUnknown` either way, because a killed `restore` can
    /// have written some of the files it had already validated.
    fn run_mutation(
        &self,
        command: &CommandSpec,
        paths: Vec<RepoPath>,
        scope: MutationScope,
        cancellation: &CancellationToken,
    ) -> MutationOutcome {
        match self.run(command, cancellation) {
            Ok(output) if output.success => MutationOutcome::Applied { paths },
            Ok(output) => {
                let exit_code = output.exit_code;
                scope.classify(exit_code, git_failure(command, output).into())
            }
            Err(failure) if !failure.started => MutationOutcome::NotApplied {
                failure: failure.into(),
            },
            Err(failure) => MutationOutcome::OutcomeUnknown {
                failure: failure.into(),
            },
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
            // A read: `check-ref-format` inspects a name and touches no
            // repository state at all.
            validate_branch: self.git_read(
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
        let branch_check = self.run(&plan.check_branch_absent, cancellation)?;
        match branch_check.exit_code {
            Some(1) => {}
            _ if branch_check.success => {
                return Err(ValidationError::InvalidBranch(format!(
                    "{} already exists",
                    request.branch.as_str()
                ))
                .into());
            }
            _ => {
                return Err(git_failure(&plan.check_branch_absent, branch_check).into());
            }
        }

        let create = self.create_worktree_command(request, &base_oid)?;
        let creation_result = self.run(&create, cancellation);
        let creation_failure = match creation_result {
            Ok(output) if output.success => None,
            Ok(output) => Some(git_failure(&create, output)),
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
            match self.run(&set_upstream, &reconciliation_token) {
                Ok(output) if output.success => {}
                Ok(output) => warnings.push(git_failure(&set_upstream, output)),
                Err(failure) => warnings.push(failure),
            }
        }
        let unlock = self.unlock_worktree_command(request);
        match self.run(&unlock, &reconciliation_token) {
            Ok(output) if output.success => {}
            Ok(output) => warnings.push(git_failure(&unlock, output)),
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
        let command = self.git_read(
            &request.repository,
            [
                OsString::from("show-ref"),
                OsString::from("--verify"),
                OsString::from("--quiet"),
                request.branch.full_ref().into(),
            ],
        );
        let branch_output = self.run(&command, cancellation)?;
        let branch_exists = match branch_output.exit_code {
            _ if branch_output.success => true,
            Some(1) => false,
            _ => return Err(git_failure(&command, branch_output).into()),
        };
        Ok(WorktreeResidualState {
            branch_exists,
            registered_worktree: worktrees
                .into_iter()
                .find(|worktree| worktree.path == request.destination),
            destination: inspect_destination(&request.destination),
        })
    }

    /// Runs one Git command, recording a failure to *launch* it. The exit code
    /// is left to the caller, because several Git commands answer a question
    /// with a non-zero exit rather than by failing.
    fn run(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        self.executor
            .execute(command, cancellation)
            .inspect_err(|failure| log_git_failure(command, failure))
    }

    fn run_checked(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        let output = self.run(command, cancellation)?;
        if output.success {
            Ok(output)
        } else {
            Err(git_failure(command, output))
        }
    }

    fn git_read<I, S>(&self, repository: &Path, arguments: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        read_options(self.git_base(repository, false)).args(arguments)
    }

    /// A read that receives a [`RepoPath`].
    fn git_literal_read<I, S>(&self, repository: &Path, arguments: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        read_options(self.git_base(repository, true)).args(arguments)
    }

    fn git_mutation<I, S>(&self, repository: &Path, arguments: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.git_base(repository, false).args(arguments)
    }

    /// A mutation that receives a [`RepoPath`].
    fn git_literal_mutation<I, S>(&self, repository: &Path, arguments: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.git_base(repository, true).args(arguments)
    }

    /// `--literal-pathspecs` is placed before `-C` because Git only accepts
    /// its global options ahead of the subcommand's. It is required, not
    /// belt-and-braces: `--` stops option parsing but not pathspec magic, so
    /// without it a file literally named `:(top)x` or `:x` would be read as a
    /// magic pathspec and could select a different set of files than the one
    /// the user picked.
    ///
    /// `core.fsmonitor=false` is set here, for *every* invocation, because it
    /// is a trust boundary rather than a latency measure. Configured to
    /// anything other than a boolean, `core.fsmonitor` names a program Git
    /// executes whenever it refreshes the index — and `add`, `restore`,
    /// `rm --cached` and `commit` all refresh the index. A repository that
    /// arrived as an archive carries its own `.git/config`, so without this a
    /// single click on Stage would run code the repository chose.
    fn git_base(&self, repository: &Path, literal_pathspecs: bool) -> CommandSpec {
        let mut command = CommandSpec::new(&self.git);
        if literal_pathspecs {
            command = command.arg("--literal-pathspecs");
        }
        command
            .args([OsString::from("-C"), repository.as_os_str().to_owned()])
            .args(["-c", "core.fsmonitor=false"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
    }
}

/// The ten NUL-separated fields [`parse_history`] expects, in order.
const HISTORY_FORMAT: &str = "%H%x00%P%x00%an%x00%ae%x00%ad%x00%cn%x00%ce%x00%cd%x00%s%x00%b";

/// Room for one `cat-file --batch` header above the content limit, so an
/// oversize object still reports its true size before being rejected.
const BATCH_HEADER_SLACK: usize = 4096;

/// Bytes of a failed Git command's standard error carried into the log. Room
/// for Git's own `fatal:` line and the hint under it, far short of the
/// megabytes a failing command is allowed to print.
const LOG_STDERR_EXCERPT: usize = 512;

/// The failure for a Git command that ran and exited non-zero.
///
/// Every caller that reads a non-zero exit as a real failure goes through here
/// so that the argv behind it is recorded exactly once. Without that record,
/// `git: process exited with status Some(128)` on a banner is unattributable:
/// identifying which command of a repository load returned it needed a shim on
/// `PATH`.
fn git_failure(command: &CommandSpec, output: ProcessOutput) -> ProcessFailure {
    let failure = output
        .require_success(&command.program)
        .expect_err("a command that did not succeed is a failure");
    log_git_failure(command, &failure);
    failure
}

/// Records one failed Git invocation.
///
/// A cancellation is not a failure: superseding an in-flight request is how
/// this application refreshes, and logging every superseded read would bury
/// the failures that matter.
fn log_git_failure(command: &CommandSpec, failure: &ProcessFailure) {
    if failure.kind == ProcessFailureKind::Cancelled {
        return;
    }
    let argv = argv_summary(command);
    let excerpt =
        String::from_utf8_lossy(&failure.stderr[..failure.stderr.len().min(LOG_STDERR_EXCERPT)]);
    tracing::warn!(
        argv = %argv,
        kind = ?failure.kind,
        status = ?failure.exit_code,
        signal = ?failure.signal,
        stderr = %excerpt.trim_end(),
        "git command failed"
    );
}

/// One log line's worth of argv, rendered lossily because a path need not be
/// UTF-8. Only a failure pays for the allocation.
fn argv_summary(command: &CommandSpec) -> String {
    let mut summary = command.program.display().to_string();
    for argument in &command.arguments {
        summary.push(' ');
        summary.push_str(&argument.to_string_lossy());
    }
    summary
}

/// The options only a read may carry. A mutation is entitled to the index lock
/// and to refreshing the index, so it must never be given any of these.
///
/// `--no-optional-locks` is Git's own answer for a tool that polls status. It
/// stops a read from taking `.git/index.lock`, so a background refresh can
/// never block a user action — and, the reason it is spelled out on the
/// command line rather than left to the environment variable alone, it stops
/// `git status` from rewriting `.git/index` when it finds the stat cache
/// stale. The environment variable stays beside it so that any Git this one
/// re-executes inherits the same rule.
///
/// `diff.autoRefreshIndex=false` closes the hole the option leaves open.
/// `git diff` against the working tree finishes by refreshing the index under
/// the index lock, and it does that whatever the optional-lock setting says.
/// With a filesystem watcher on the worktree, that write is seen as a change,
/// which schedules another state read, which runs another `git diff`: a
/// refresh loop that spins Git processes for as long as the window is open.
/// The comparison itself is unaffected, because a diff reads content rather
/// than trusting the stat cache.
fn read_options(command: CommandSpec) -> CommandSpec {
    command
        .args(["--no-optional-locks", "-c", "diff.autoRefreshIndex=false"])
        .env("GIT_OPTIONAL_LOCKS", "0")
}

/// Subcommand arguments followed by `--` and the selected paths.
fn pathspec_arguments<'a>(
    leading: impl IntoIterator<Item = &'a str>,
    paths: &[RepoPath],
) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = leading.into_iter().map(OsString::from).collect();
    arguments.push(OsString::from("--"));
    arguments.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
    arguments
}

/// Names exactly what a discard deleted and what it refused.
fn report_discard(paths: Vec<RepoPath>, mut failures: Vec<MutationFailure>) -> MutationOutcome {
    if failures.is_empty() {
        return MutationOutcome::Applied { paths };
    }
    if paths.is_empty() {
        // Nothing was deleted, so the request is retried whole and the first
        // reason is the one the caller acts on.
        return MutationOutcome::NotApplied {
            failure: failures.swap_remove(0),
        };
    }
    MutationOutcome::PartiallyApplied { paths, failures }
}

/// Applies Git's `--cleanup=whitespace` to a commit message.
///
/// Trailing whitespace goes from every line, leading and trailing empty lines
/// go, and a run of empty lines becomes one. Comment lines are kept, which is
/// the whole difference from `strip`. Used to compare a message this service
/// sent against the one Git stored, so both sides are reduced by the same rule
/// that Git already applied to one of them — which is why "whitespace" has to
/// mean exactly what `git stripspace` means by it, and no more:
/// `strbuf_stripspace` right-trims each line with Git's own `isspace`, whose
/// `sane_ctype` table holds space, tab, LF and CR and deliberately leaves out
/// vertical tab and form feed, so `is_ascii_whitespace` is the wrong set.
/// `str::lines` takes one CR as part of a CRLF terminator and leaves any
/// others, so a line ending `\r\r\n` needs the second one taken here.
fn cleanup_whitespace(message: &str) -> String {
    let mut cleaned = String::with_capacity(message.len());
    let mut pending_blank = false;
    for line in message.lines() {
        let line = line.trim_end_matches([' ', '\t', '\r']);
        if line.is_empty() {
            // Only remembered, so leading and trailing runs never reach the
            // output and an interior run collapses to one.
            pending_blank = !cleaned.is_empty();
            continue;
        }
        if !cleaned.is_empty() {
            cleaned.push('\n');
        }
        if pending_blank {
            cleaned.push('\n');
            pending_blank = false;
        }
        cleaned.push_str(line);
    }
    cleaned
}

/// `<local ref>:<remote-side ref>`, both fully qualified.
///
/// The destination is a [`RemoteSideRef`] rather than a string so that this
/// cannot be handed the remote-tracking ref for the same branch, which Git
/// would push to happily and to no effect.
fn push_refspec(branch: &ValidatedBranchName, destination: &RemoteSideRef) -> String {
    format!("{}:{destination}", branch.full_ref())
}

/// Remote names reach both a configuration key and a command line, so a name
/// that could be read as an option, a path, or a refspec is rejected here
/// rather than handed to Git.
fn validate_remote_name(name: &str) -> Result<(), ValidationError> {
    let invalid = name.is_empty()
        || name.starts_with('-')
        || name.starts_with('.')
        || name.contains('/')
        || name.contains(':')
        || name.contains("..")
        || name.bytes().any(|byte| {
            byte.is_ascii_control()
                || byte == b' '
                || matches!(byte, b'~' | b'^' | b'?' | b'*' | b'[' | b'\\')
        });
    if invalid {
        Err(ValidationError::InvalidRemote(name.to_owned()))
    } else {
        Ok(())
    }
}

/// Deletes one untracked entry, resolving it only beneath the worktree root.
///
/// The hazard a discard has to close is the *second* path resolution: checking
/// the ancestors by name and then unlinking by name lets a concurrent writer
/// swap a checked directory for a symlink in between, after which the unlink
/// lands on a same-named file outside the worktree. So the parent directory is
/// opened beneath `root` following no symlink in any component, and both the
/// classification and the unlink go through *that* descriptor — the name is
/// never resolved from the worktree a second time, and neither step can reach
/// outside the directory the other one verified.
///
/// A directory is never removed, and by construction rather than by inspection:
/// [`fs_safety::unlink_within`] unlinks without `AT_REMOVEDIR`, which the
/// kernel refuses for a directory.
fn delete_untracked(root: &fs_safety::Root, path: &RepoPath) -> Result<(), MutationFailure> {
    let reject = |reason: String| MutationFailure::Path {
        path: path.clone(),
        reason,
    };
    let (parent, name) = fs_safety::open_parent_beneath(root, path.as_path())
        .map_err(|error| reject(error.to_string()))?;
    match fs_safety::metadata_beneath(&parent, Path::new(&name)) {
        Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
            fs_safety::unlink_within(&parent, &name).map_err(|error| reject(error.to_string()))
        }
        Ok(_) => Err(reject("only regular files and symlinks are deleted".into())),
        // Already gone: the requested end state is the current state.
        Err(error) if error.is_not_found() => Ok(()),
        Err(error) => Err(reject(error.to_string())),
    }
}

/// The object size from a `cat-file --batch` header, even when the content that
/// followed it was truncated.
fn batch_header_size(output: &[u8]) -> Option<u64> {
    let line = output.split(|byte| *byte == b'\n').next()?;
    std::str::from_utf8(line)
        .ok()?
        .rsplit(' ')
        .next()?
        .parse()
        .ok()
}

/// Attaches the raw process output to a parse failure, so a report always
/// carries the bytes that could not be understood.
fn invalid_output<T>(
    result: Result<T, GitParseError>,
    process: ProcessOutput,
) -> Result<T, RepositoryError> {
    result.map_err(|error| RepositoryError::InvalidGitOutput { error, process })
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

    /// A hook may `setsid` a background process, which escapes the process
    /// group killed at the deadline while still holding the child's stdout and
    /// stderr. Nothing will ever close those pipes, so an executor that joins
    /// its reader threads unconditionally never returns — and the caller of a
    /// mutation is holding that repository's [`MutationLocks`] guard while it
    /// waits.
    ///
    /// The bound is structural rather than timed: the pipe readers are woken by
    /// a descriptor, so this test fails by not producing a result at all rather
    /// than by being slower than some threshold.
    #[cfg(unix)]
    #[test]
    fn a_descendant_holding_a_pipe_cannot_outlive_the_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let holder = directory.path().join("holder.pid");
        let command = CommandSpec::new("/bin/sh")
            .args([
                OsString::from("-c"),
                // `setsid` puts the sleeper in its own session, so the process
                // group signalled at the deadline does not contain it, and it
                // inherits the stdout and stderr this executor is reading.
                OsString::from(
                    "setsid sh -c 'printf %s \"$$\" > \"$HOLDER\"; exec sleep 300' & exit 0",
                ),
            ])
            .env("HOLDER", holder.as_os_str())
            .timeout(Duration::from_millis(200));

        let (report, results) = mpsc::channel();
        thread::spawn(move || {
            let outcome = SystemProcessExecutor.execute(&command, &CancellationToken::new());
            let _ = report.send(outcome.map(|_| ()).map_err(|failure| failure.kind));
        });
        let outcome = results
            .recv_timeout(Duration::from_secs(20))
            .expect("the executor never returned while a descendant held its pipes");

        assert_eq!(outcome.unwrap_err(), ProcessFailureKind::TimedOut);

        // The sleeper is deliberately outside every group this executor may
        // signal, so the test is what has to end it.
        for _ in 0..200 {
            if let Ok(text) = fs::read_to_string(&holder)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                signal_process_group(pid, SIGKILL);
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
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

    /// The branch a worktree is on is a label. Requiring it to decode made one
    /// worktree checked out on a branch Git accepts fail the whole listing —
    /// the very degradation the status parser already applies to the same
    /// value under a different name.
    #[cfg(unix)]
    #[test]
    fn a_worktree_branch_that_is_not_utf8_costs_nothing_but_its_own_label() {
        let mut input =
            b"worktree /repo\0HEAD 0123456789012345678901234567890123456789\0branch refs/heads/b"
                .to_vec();
        input.push(0xff);
        input.extend_from_slice(b"ad\0\0worktree /repo/wt\0HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0branch refs/heads/main\0\0");
        let records = parse_worktree_list(&input).expect("a label is not a framing failure");

        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0].branch.as_deref(),
            Some(String::from_utf8_lossy(b"refs/heads/b\xffad").as_ref())
        );
        // The worktree that shares the listing is unharmed, which is the whole
        // point: `list_worktrees` is what a project load depends on.
        assert_eq!(records[1].branch.as_deref(), Some("refs/heads/main"));
        // A `branch` field with no value at all is still framing, not a label.
        assert!(matches!(
            parse_worktree_list(b"worktree /repo\0branch\0\0"),
            Err(GitParseError::Malformed { .. })
        ));
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
    fn discovery_asks_for_the_work_tree_root_only_where_one_exists() {
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            success(b"false\n"),
            success(b"/repo/.git\n"),
            success(b"/repo/.git\n"),
            success(b"true\n"),
            success(b"/repo\n"),
        ]));
        let service = RepositoryService::new("git", fake.clone());
        let repository = service
            .discover(Path::new("/repo/subdir"), &CancellationToken::new())
            .unwrap();
        assert_eq!(repository.common_dir, PathBuf::from("/repo/.git"));
        assert_eq!(repository.worktree_root, Some(PathBuf::from("/repo")));

        let commands = fake.commands.lock().unwrap();
        assert_eq!(commands.len(), 5);
        // `--show-toplevel` is asked last, and only of a path the fourth query
        // has just proved to be inside a work tree.
        assert!(
            commands[4]
                .arguments
                .contains(&OsString::from("--show-toplevel"))
        );
        assert!(
            commands[4]
                .arguments
                .contains(&OsString::from("/repo/subdir"))
        );
        assert!(
            commands[..4].iter().all(|command| {
                !command
                    .arguments
                    .contains(&OsString::from("--show-toplevel"))
            }),
            "the first four queries must all be valid inside a Git directory"
        );
    }

    /// A project remembers its common directory, so reopening one hands that
    /// path straight back to discovery. `--show-toplevel` exits 128 there,
    /// which put a red banner on every single load, and the repository has to
    /// resolve to its work tree rather than root itself inside `.git`.
    #[test]
    fn a_git_directory_selected_as_the_repository_resolves_to_its_work_tree() {
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            success(b"false\n"),
            success(b"/repo/.git\n"),
            success(b"/repo/.git\n"),
            success(b"false\n"),
            success(b"worktree /repo\0\0"),
            success(b"/repo/.git\n"),
        ]));
        let service = RepositoryService::new("git", fake.clone());
        let repository = service
            .discover(Path::new("/repo/.git"), &CancellationToken::new())
            .unwrap();

        assert!(!repository.is_bare);
        assert_eq!(repository.worktree_root, Some(PathBuf::from("/repo")));

        let commands = fake.commands.lock().unwrap();
        assert!(
            commands.iter().all(|command| {
                !command
                    .arguments
                    .contains(&OsString::from("--show-toplevel"))
            }),
            "no query may be `--show-toplevel` while the selected path is a Git directory"
        );
        // The candidate is asked for its own Git directory from the work tree,
        // which can answer, rather than from the Git directory, which cannot.
        assert!(commands[5].arguments.contains(&OsString::from("/repo")));
        assert!(commands[5].arguments.contains(&OsString::from("--git-dir")));
    }

    /// Falling back to the Git directory is what titled a reopened card `.git`
    /// and rooted its file tree inside the repository's own metadata. A
    /// non-bare repository whose work trees cannot be resolved has no root at
    /// all, and saying so is better than adopting one that is wrong.
    #[test]
    fn a_git_directory_no_work_tree_claims_is_never_adopted_as_a_root() {
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            success(b"false\n"),
            success(b"/repo/.git\n"),
            success(b"/repo/.git\n"),
            success(b"false\n"),
            success(b"worktree /elsewhere\0\0"),
            success(b"/elsewhere/.git\n"),
        ]));
        let service = RepositoryService::new("git", fake);
        let error = service
            .discover(Path::new("/repo/.git"), &CancellationToken::new())
            .unwrap_err();

        assert!(
            matches!(
                &error,
                RepositoryError::NoWorkTree { git_dir } if git_dir == Path::new("/repo/.git")
            ),
            "{error:?}"
        );
    }

    /// A bare repository has no work tree to resolve, and both of the queries
    /// that would look for one fail there, so discovery must not ask.
    #[test]
    fn a_bare_repository_resolves_no_work_tree_and_asks_for_none() {
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            success(b"true\n"),
            success(b"/repo.git\n"),
            success(b"/repo.git\n"),
        ]));
        let service = RepositoryService::new("git", fake.clone());
        let repository = service
            .discover(Path::new("/repo.git"), &CancellationToken::new())
            .unwrap();

        assert!(repository.is_bare);
        assert_eq!(repository.worktree_root, None);
        // Three queries, not five: neither `--is-inside-work-tree` nor
        // `--show-toplevel` has anything to say about a bare repository.
        assert_eq!(fake.commands.lock().unwrap().len(), 3);
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

    fn exit(code: i32, stdout: &[u8]) -> Result<ProcessOutput, ProcessFailure> {
        Ok(ProcessOutput {
            exit_code: Some(code),
            signal: None,
            success: code == 0,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        })
    }

    fn launched_failure(stdout: &[u8]) -> ProcessFailure {
        ProcessFailure {
            kind: ProcessFailureKind::Cancelled,
            program: "git".into(),
            exit_code: None,
            signal: None,
            stdout: stdout.to_vec(),
            stderr: b"cancelled".to_vec(),
            message: "cancelled".into(),
            started: true,
        }
    }

    fn unstarted_failure() -> ProcessFailure {
        ProcessFailure {
            kind: ProcessFailureKind::Spawn,
            program: "git".into(),
            exit_code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            message: "no such file".into(),
            started: false,
        }
    }

    fn oid(seed: u8) -> Oid {
        Oid::parse(format!("{:02x}", seed).repeat(20)).unwrap()
    }

    fn path(value: &str) -> RepoPath {
        RepoPath::parse(value).unwrap()
    }

    fn fake_service(
        outputs: Vec<Result<ProcessOutput, ProcessFailure>>,
    ) -> RepositoryService<FakeExecutor> {
        RepositoryService::new("git", Arc::new(FakeExecutor::with_outputs(outputs)))
    }

    fn status_headers() -> Vec<u8> {
        let mut input = b"# branch.oid ".to_vec();
        input.extend_from_slice(oid(1).as_str().as_bytes());
        input.extend_from_slice(b"\0# branch.head main\0");
        input
    }

    #[test]
    fn repo_path_rejects_traversal_absolute_and_empty_paths() {
        assert!(matches!(RepoPath::parse(""), Err(RepoPathError::Empty)));
        assert!(matches!(
            RepoPath::parse("/etc/passwd"),
            Err(RepoPathError::NotRelative { .. })
        ));
        assert!(matches!(
            RepoPath::parse("../outside"),
            Err(RepoPathError::InvalidComponent { .. })
        ));
        assert!(matches!(
            RepoPath::parse("src/../../etc"),
            Err(RepoPathError::InvalidComponent { .. })
        ));
        assert!(matches!(
            RepoPath::parse("./here"),
            Err(RepoPathError::InvalidComponent { .. })
        ));
        assert!(matches!(
            RepoPath::parse("src//main.rs"),
            Err(RepoPathError::EmptyComponent { .. })
        ));
        // Git names a directory with one trailing separator, which is
        // canonicalized rather than refused; a second one is still an empty
        // component.
        assert_eq!(RepoPath::parse("src/").unwrap(), path("src"));
        assert!(matches!(
            RepoPath::parse("src//"),
            Err(RepoPathError::EmptyComponent { .. })
        ));
        assert!(matches!(RepoPath::parse("/"), Err(RepoPathError::Empty)));
        assert!(matches!(
            RepoPath::from_bytes(b"src/\0main.rs"),
            Err(RepoPathError::InteriorNul)
        ));

        let valid = path("src/deep/main.rs");
        assert_eq!(valid.extension().as_deref(), Some("rs"));
        assert_eq!(valid.file_name(), "main.rs");
        assert_eq!(valid.parent(), Some(path("src/deep")));
        assert_eq!(path("main.rs").parent(), None);
        assert_eq!(
            valid.resolve(Path::new("/work/tree")),
            PathBuf::from("/work/tree/src/deep/main.rs")
        );
        // A name that looks like pathspec magic is an ordinary path here; only
        // `--literal-pathspecs` keeps Git from reinterpreting it.
        assert_eq!(path(":(top)odd").display(), ":(top)odd");
    }

    #[cfg(unix)]
    #[test]
    fn repo_path_round_trips_non_utf8_bytes_through_serde() {
        let raw = b"src/\xffbroken.rs";
        let path = RepoPath::from_bytes(raw).unwrap();
        assert_eq!(path.as_bytes(), raw);
        // Lossy display must not be what is stored.
        assert_ne!(path.display().as_bytes(), raw);

        let encoded = serde_json::to_string(&path).unwrap();
        let decoded: RepoPath = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.as_bytes(), raw);
        assert!(
            serde_json::from_str::<RepoPath>("[47, 101, 116, 99]").is_err(),
            "deserialization must re-validate the path"
        );
    }

    #[test]
    fn oid_requires_lowercase_hex_and_rejects_the_null_id() {
        assert!(Oid::parse("a".repeat(40)).is_ok());
        assert!(Oid::parse("a".repeat(64)).is_ok());
        assert!(Oid::parse("A".repeat(40)).is_err());
        assert!(Oid::parse("a".repeat(39)).is_err());
        assert!(Oid::parse("z".repeat(40)).is_err());
        assert!(Oid::parse(ZERO_SHA1).is_ok());
        assert!(matches!(Oid::parse_object(ZERO_SHA1), Err(OidError::Null)));
    }

    #[test]
    fn status_command_lists_every_untracked_file() {
        let service = fake_service(Vec::new());
        let command = service.status_command(Path::new("/repo"));
        assert!(
            command
                .arguments
                .contains(&OsString::from("--untracked-files=all"))
        );
        assert!(
            !command
                .arguments
                .contains(&OsString::from("--untracked-files=normal"))
        );
    }

    #[test]
    fn status_reports_simultaneous_staged_and_unstaged_work_as_two_entries() {
        let mut input = status_headers();
        input.extend_from_slice(
            b"1 MM N... 100644 100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 src/main.rs\0",
        );
        let status = parse_status(&input).unwrap();

        assert_eq!(status.entries.len(), 2);
        assert_eq!(status.entries[0].area, ChangeArea::Staged);
        assert_eq!(status.entries[0].kind, ChangeKind::Modified);
        assert_eq!(status.entries[1].area, ChangeArea::Unstaged);
        assert_eq!(status.entries[1].kind, ChangeKind::Modified);
        assert_eq!(status.entries[0].path, path("src/main.rs"));
        assert_eq!(status.entries[0].path, status.entries[1].path);
        // The badge counts must not change because entries now exist.
        assert_eq!(status.dirty.staged, 1);
        assert_eq!(status.dirty.unstaged, 1);
        assert_eq!(status.area(ChangeArea::Staged).count(), 1);
    }

    #[test]
    fn status_keeps_the_rename_source_on_the_renaming_half_only() {
        let mut input = status_headers();
        input.extend_from_slice(
            b"2 RM N... 100644 100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 R100 new name.txt\0old name.txt\0",
        );
        let status = parse_status(&input).unwrap();

        assert_eq!(status.entries.len(), 2);
        assert_eq!(status.entries[0].kind, ChangeKind::Renamed);
        assert_eq!(status.entries[0].old_path, Some(path("old name.txt")));
        assert_eq!(status.entries[0].path, path("new name.txt"));
        // The worktree half is a modification of the destination, so the source
        // is not part of it.
        assert_eq!(status.entries[1].kind, ChangeKind::Modified);
        assert_eq!(status.entries[1].old_path, None);
    }

    #[test]
    fn status_maps_every_unmerged_code_to_its_conflict_kind() {
        let expected = [
            (&b"DD"[..], ConflictKind::BothDeleted),
            (b"AU", ConflictKind::AddedByUs),
            (b"UD", ConflictKind::DeletedByThem),
            (b"UA", ConflictKind::AddedByThem),
            (b"DU", ConflictKind::DeletedByUs),
            (b"AA", ConflictKind::BothAdded),
            (b"UU", ConflictKind::BothModified),
        ];
        let mut input = status_headers();
        for (code, _) in expected {
            input.extend_from_slice(b"u ");
            input.extend_from_slice(code);
            input.extend_from_slice(b" N... 100644 100644 100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 3333333333333333333333333333333333333333 conflict/");
            input.extend_from_slice(code);
            input.push(0);
        }
        let status = parse_status(&input).unwrap();

        assert_eq!(status.entries.len(), expected.len());
        assert_eq!(status.dirty.conflicted, expected.len() as u32);
        for (entry, (code, kind)) in status.entries.iter().zip(expected) {
            assert_eq!(entry.area, ChangeArea::Conflict, "{:?}", entry.path);
            assert_eq!(entry.kind, ChangeKind::Unmerged);
            assert_eq!(entry.conflict, Some(kind), "{code:?}");
            assert_eq!(entry.added_lines, None);
        }

        let mut invalid = status_headers();
        invalid.extend_from_slice(b"u XY N... 100644 100644 100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 3333333333333333333333333333333333333333 odd\0");
        assert!(parse_status(&invalid).is_err());
    }

    #[test]
    fn status_marks_submodule_entries_and_untracked_files() {
        let mut input = status_headers();
        input.extend_from_slice(
            b"1 .M SC.. 160000 160000 160000 1111111111111111111111111111111111111111 1111111111111111111111111111111111111111 vendor/dep\0",
        );
        input.extend_from_slice(b"? build/output.log\0");
        let status = parse_status(&input).unwrap();

        assert!(status.entries[0].submodule);
        assert_eq!(status.dirty.submodule, 1);
        assert_eq!(status.entries[1].area, ChangeArea::Untracked);
        assert_eq!(status.entries[1].kind, ChangeKind::Untracked);
        assert!(!status.entries[1].submodule);
        assert_eq!(status.dirty.untracked, 1);
    }

    /// `--untracked-files=all` still reports an untracked nested repository as
    /// a directory, because Git will not look inside another repository. That
    /// must not fail the whole snapshot.
    #[test]
    fn a_nested_repository_is_reported_as_its_directory() {
        let mut input = status_headers();
        input.extend_from_slice(b"? subclone/\0");
        input.extend_from_slice(b"? plain/deep/file.txt\0");
        let status = parse_status(&input).unwrap();

        assert_eq!(status.entries[0].path, path("subclone"));
        assert_eq!(status.entries[0].area, ChangeArea::Untracked);
        assert_eq!(status.entries[1].path, path("plain/deep/file.txt"));

        // `ls-files -co` interleaves cached and other entries, so the list is
        // sorted for a stable file tree.
        let listed = parse_path_list(b"subclone/\0a.txt\0", "git ls-files").unwrap();
        assert_eq!(listed, vec![path("a.txt"), path("subclone")]);
    }

    #[cfg(unix)]
    #[test]
    fn status_preserves_newline_and_non_utf8_paths() {
        let mut input = status_headers();
        input.extend_from_slice(b"? two\nlines.txt\0");
        input.extend_from_slice(b"? broken-\xff.txt\0");
        input.extend_from_slice(
            b"1 A. N... 000000 100644 100644 0000000000000000000000000000000000000000 2222222222222222222222222222222222222222 odd \xfe name\0",
        );
        let status = parse_status(&input).unwrap();

        assert_eq!(status.entries[0].path.as_bytes(), b"two\nlines.txt");
        assert_eq!(status.entries[1].path.as_bytes(), b"broken-\xff.txt");
        assert_eq!(status.entries[2].path.as_bytes(), b"odd \xfe name");
    }

    #[test]
    fn status_reports_an_unborn_head_with_its_branch() {
        let input = b"# branch.oid (initial)\0# branch.head main\0? first.txt\0";
        let status = parse_status(input).unwrap();

        assert_eq!(
            status.head,
            HeadState::Unborn {
                branch: Some("main".into())
            }
        );
        assert_eq!(status.branch(), Some("main"));
        assert_eq!(status.entries.len(), 1);
    }

    #[test]
    fn numstat_merging_fills_text_counts_and_leaves_binary_counts_unset() {
        let mut input = status_headers();
        input.extend_from_slice(
            b"1 M. N... 100644 100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 src/main.rs\0",
        );
        input.extend_from_slice(
            b"1 A. N... 000000 100644 100644 0000000000000000000000000000000000000000 2222222222222222222222222222222222222222 art/logo.png\0",
        );
        input.extend_from_slice(
            b"2 R. N... 100644 100644 100644 1111111111111111111111111111111111111111 1111111111111111111111111111111111111111 R100 renamed.txt\0original.txt\0",
        );
        input.extend_from_slice(b"? untracked.txt\0");
        let mut status = parse_status(&input).unwrap();

        let records = parse_numstat(
            b"7\t2\tsrc/main.rs\0-\t-\tart/logo.png\x000\t0\t\0original.txt\0renamed.txt\0",
        )
        .unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[2].path, path("renamed.txt"));
        assert_eq!(records[2].old_path, Some(path("original.txt")));

        merge_numstat(&mut status.entries, ChangeArea::Staged, &records);
        let by_path = |name: &str| {
            status
                .entries
                .iter()
                .find(|entry| entry.path == path(name))
                .unwrap()
        };
        assert_eq!(by_path("src/main.rs").added_lines, Some(7));
        assert_eq!(by_path("src/main.rs").removed_lines, Some(2));
        // Binary files have no line counts, and a `-` must not become zero.
        assert_eq!(by_path("art/logo.png").added_lines, None);
        assert_eq!(by_path("renamed.txt").added_lines, Some(0));
        assert_eq!(by_path("untracked.txt").added_lines, None);
    }

    #[test]
    fn numstat_rejects_a_truncated_rename_record() {
        assert!(matches!(
            parse_numstat(b"0\t0\t\0original.txt\0"),
            Err(GitParseError::Truncated { .. })
        ));
        assert!(parse_numstat(b"x\t1\tfile\0").is_err());
    }

    #[test]
    fn name_status_keeps_rename_pairs_and_merges_counts() {
        let mut entries =
            parse_name_status(b"M\0src/main.rs\0R100\0old.txt\0new.txt\0A\0added.txt\0").unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].kind, ChangeKind::Renamed);
        assert_eq!(entries[1].old_path, Some(path("old.txt")));
        assert_eq!(entries[1].path, path("new.txt"));

        let records = parse_numstat(b"3\t1\tsrc/main.rs\0").unwrap();
        merge_diff_numstat(&mut entries, &records);
        assert_eq!(entries[0].added_lines, Some(3));
        assert_eq!(entries[0].removed_lines, Some(1));
        assert_eq!(entries[2].added_lines, None);
    }

    #[test]
    fn history_parses_identities_offsets_and_bodies() {
        let mut input = Vec::new();
        input.extend_from_slice(oid(0x0a).as_str().as_bytes());
        input.push(0);
        input.extend_from_slice(oid(0x0b).as_str().as_bytes());
        input.push(b' ');
        input.extend_from_slice(oid(0x0c).as_str().as_bytes());
        input.extend_from_slice(
            b"\0Ada\0ada@example.invalid\x001700000000 +0230\0Grace\0grace@example.invalid\x001700000900 -0500\0merge branch\0body line\n\n\0",
        );
        let commits = parse_history(&input).unwrap();

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].oid, oid(0x0a));
        assert_eq!(commits[0].parents, vec![oid(0x0b), oid(0x0c)]);
        assert_eq!(commits[0].author.name, "Ada");
        assert_eq!(commits[0].author.timestamp, 1_700_000_000);
        assert_eq!(commits[0].author.offset_minutes, 150);
        assert_eq!(commits[0].committer.offset_minutes, -300);
        assert_eq!(commits[0].subject, "merge branch");
        assert_eq!(commits[0].body, "body line");
    }

    #[test]
    fn history_rejects_malformed_records_instead_of_skipping_them() {
        // One field short of a record.
        let mut short = Vec::new();
        short.extend_from_slice(oid(1).as_str().as_bytes());
        short.extend_from_slice(b"\0\0Ada\0ada@example.invalid\x001700000000 +0000\0Ada\0ada@example.invalid\x001700000000 +0000\0subject\0");
        assert!(matches!(
            parse_history(&short),
            Err(GitParseError::Malformed { .. })
        ));

        let mut bad_oid = Vec::new();
        bad_oid.extend_from_slice(b"nothex\0\0Ada\0ada@example.invalid\x001700000000 +0000\0Ada\0ada@example.invalid\x001700000000 +0000\0subject\0\0");
        assert!(parse_history(&bad_oid).is_err());
        assert!(matches!(
            parse_history(b"missing terminator"),
            Err(GitParseError::Truncated { .. })
        ));
    }

    /// Git splits the offset field as `N / 100` hours plus `N % 100` minutes
    /// with no range check, so these are the offsets it really reports for
    /// commit objects imported from CVS and Subversion — and a rendered
    /// timestamp has to agree with `git log` on them rather than merely
    /// survive them. The commit objects themselves are pinned against real
    /// Git in `tests/repository_git_contract.rs`.
    #[test]
    fn a_commit_offset_is_read_exactly_as_git_reads_it() {
        assert_eq!(parse_timezone_offset("+0230"), Some(150));
        assert_eq!(parse_timezone_offset("-0500"), Some(-300));
        // Minutes are not capped at 60: `+0060` displays an hour east of UTC.
        assert_eq!(parse_timezone_offset("+0060"), Some(60));
        // Five digits, which the old four-digit length check rejected.
        assert_eq!(parse_timezone_offset("+51800"), Some(518 * 60));
        assert_eq!(parse_timezone_offset("+9999"), Some(99 * 60 + 99));
        assert_eq!(parse_timezone_offset(""), None);
        assert_eq!(parse_timezone_offset("0230"), None);
        assert_eq!(parse_timezone_offset("+garbage"), None);
        assert_eq!(parse_timezone_offset("+99999999999999"), None);
    }

    /// A date this cannot read costs its own commit its timestamp and nothing
    /// else. The whole 200-commit page used to be discarded instead.
    #[test]
    fn one_unreadable_commit_date_does_not_discard_the_page() {
        let mut input = Vec::new();
        for date in [&b"1700000000 0230"[..], b"", b"1700000000 +0060"] {
            input.extend_from_slice(oid(1).as_str().as_bytes());
            input.extend_from_slice(b"\0\0Ada\0ada@example.invalid\0");
            input.extend_from_slice(date);
            input.extend_from_slice(b"\0Ada\0ada@example.invalid\x001700000000 +0000\0subject\0\0");
        }
        let commits = parse_history(&input).unwrap();

        assert_eq!(commits.len(), 3);
        // An offset with no sign, then no date at all: both degrade to the
        // epoch in UTC without taking the record with them.
        assert_eq!(commits[0].author.timestamp, 1_700_000_000);
        assert_eq!(commits[0].author.offset_minutes, 0);
        assert_eq!(commits[1].author.timestamp, 0);
        assert_eq!(commits[1].author.offset_minutes, 0);
        assert_eq!(commits[2].author.offset_minutes, 60);
        // The committer half of every record was readable and is unaffected.
        assert!(
            commits
                .iter()
                .all(|commit| commit.committer.timestamp == 1_700_000_000)
        );
    }

    #[test]
    fn refs_and_remotes_retain_full_names() {
        let mut input = Vec::new();
        input.extend_from_slice(b"refs/heads/main\0");
        input.extend_from_slice(oid(1).as_str().as_bytes());
        input.extend_from_slice(b"\0\0refs/remotes/origin/main\0*\n");
        input.extend_from_slice(b"refs/remotes/origin/main\0");
        input.extend_from_slice(oid(1).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0 \n");
        input.extend_from_slice(b"refs/remotes/origin/HEAD\0");
        input.extend_from_slice(oid(1).as_str().as_bytes());
        input.extend_from_slice(b"\0refs/remotes/origin/main\0\0 \n");
        let refs = parse_refs(&input).unwrap();

        assert_eq!(refs.local.len(), 1);
        assert_eq!(refs.local[0].name.as_str(), "refs/heads/main");
        assert_eq!(refs.local[0].name.short_name(), "main");
        assert!(refs.local[0].head);
        assert_eq!(
            refs.local[0]
                .upstream
                .as_ref()
                .map(ValidatedRefName::as_str),
            Some("refs/remotes/origin/main")
        );
        // `refs/remotes/origin/HEAD` is a symbolic ref, so it is the remote's
        // default-branch pointer and not a branch named `HEAD`.
        assert_eq!(refs.remote.len(), 1);
        assert_eq!(refs.remote[0].remote, "origin");
        assert_eq!(refs.remote[0].branch, "main");
        assert_eq!(refs.remote_heads.len(), 1);
        assert_eq!(refs.remote_heads[0].remote, "origin");
        assert_eq!(
            refs.remote_heads[0].target.as_str(),
            "refs/remotes/origin/main"
        );

        let remotes = parse_remote_config(
            b"remote.origin.url\nssh://git@example.invalid/repo.git\0remote.origin.fetch\n+refs/heads/*\0remote.with.dots.pushurl\nhttps://example.invalid/other.git\0",
        )
        .unwrap();
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(
            remotes[0].fetch_url.as_deref(),
            Some("ssh://git@example.invalid/repo.git")
        );
        assert_eq!(remotes[0].push_url, None);
        assert_eq!(remotes[1].name, "with.dots");
        assert_eq!(
            remotes[1].push_url.as_deref(),
            Some("https://example.invalid/other.git")
        );
    }

    /// A ref nobody asked about is not evidence that Git's answer to the
    /// question that *was* asked is untrustworthy. Each of these used to
    /// return `Malformed` and take the entire snapshot — every changed path,
    /// every count — with it.
    #[test]
    fn an_unclassifiable_ref_is_skipped_rather_than_fatal() {
        let mut input = Vec::new();
        // Legal, and some tools create it: a ref directly under refs/remotes/
        // with no branch component.
        input.extend_from_slice(b"refs/remotes/weird\0");
        input.extend_from_slice(oid(1).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0 \n");
        // A branch name Git accepts and no encoding decodes.
        input.extend_from_slice(b"refs/heads/b\xffad\0");
        input.extend_from_slice(oid(2).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0 \n");
        // A tag, which this format never asks for but a caller could.
        input.extend_from_slice(b"refs/tags/v1\0");
        input.extend_from_slice(oid(3).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0 \n");
        // A symbolic remote ref that is not a HEAD pointer.
        input.extend_from_slice(b"refs/remotes/origin/alias\0");
        input.extend_from_slice(oid(4).as_str().as_bytes());
        input.extend_from_slice(b"\0refs/remotes/origin/main\0\0 \n");
        input.extend_from_slice(b"refs/heads/main\0");
        input.extend_from_slice(oid(5).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0*\n");
        let refs = parse_refs(&input).unwrap();

        assert_eq!(refs.local.len(), 1);
        assert_eq!(refs.local[0].oid, oid(5));
        assert!(refs.remote.is_empty());
        assert!(refs.remote_heads.is_empty());

        // The framing itself is still a hard requirement: a record with the
        // wrong field count means the output cannot be walked at all.
        assert!(parse_refs(b"refs/heads/main\0\0\n").is_err());
    }

    /// `git remote add te/am` is legal, so `refs/remotes/te/am/main` is `main`
    /// on `te/am` or `am/main` on `te` and the ref name alone cannot say
    /// which. The remote's symbolic `HEAD` is where the boundary is
    /// unambiguous. Both halves of the listing must be split there, because
    /// they exist to be joined on that field: naming the remote `te/am` in one
    /// and `te` in the other silently lost every remote-head lookup.
    #[test]
    fn both_halves_of_a_ref_listing_name_a_remote_the_same_way() {
        let mut input = Vec::new();
        input.extend_from_slice(b"refs/remotes/te/am/HEAD\0");
        input.extend_from_slice(oid(1).as_str().as_bytes());
        input.extend_from_slice(b"\0refs/remotes/te/am/main\0\0 \n");
        input.extend_from_slice(b"refs/remotes/te/am/main\0");
        input.extend_from_slice(oid(2).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0 \n");
        // A branch whose own name contains a slash, on a remote whose pointer
        // arrives *after* it: the split is against the whole listing, not
        // against whatever has been read so far.
        input.extend_from_slice(b"refs/remotes/origin/feature/x\0");
        input.extend_from_slice(oid(3).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0 \n");
        input.extend_from_slice(b"refs/remotes/origin/HEAD\0");
        input.extend_from_slice(oid(4).as_str().as_bytes());
        input.extend_from_slice(b"\0refs/remotes/origin/main\0\0 \n");
        // A remote added by hand rather than cloned has no `HEAD` pointer, so
        // there is nothing to split against and the first component is the
        // only reading available — which is also the one Git's own default
        // refspec produces.
        input.extend_from_slice(b"refs/remotes/bare/main\0");
        input.extend_from_slice(oid(5).as_str().as_bytes());
        input.extend_from_slice(b"\0\0\0 \n");
        let refs = parse_refs(&input).unwrap();

        assert_eq!(refs.remote_heads.len(), 2);
        assert_eq!(refs.remote_heads[0].remote, "te/am");
        let named = |name: &str| {
            refs.remote
                .iter()
                .find(|record| record.name.as_str() == name)
                .unwrap_or_else(|| panic!("{name} is missing from {:?}", refs.remote))
        };
        assert_eq!(named("refs/remotes/te/am/main").remote, "te/am");
        assert_eq!(named("refs/remotes/te/am/main").branch, "main");
        assert_eq!(named("refs/remotes/origin/feature/x").remote, "origin");
        assert_eq!(named("refs/remotes/origin/feature/x").branch, "feature/x");
        assert_eq!(named("refs/remotes/bare/main").remote, "bare");
        assert_eq!(named("refs/remotes/bare/main").branch, "main");

        // The join the two fields exist for: every branch of a remote that has
        // a pointer finds that remote's own pointer, and never another's.
        let snapshot = RepositorySnapshot {
            status: StatusSnapshot::default(),
            files: WorkingFiles::default(),
            remotes: Vec::new(),
            local_refs: refs.local.clone(),
            remote_refs: refs.remote.clone(),
            remote_heads: refs.remote_heads.clone(),
        };
        assert_eq!(
            snapshot
                .remote_head(&named("refs/remotes/te/am/main").remote)
                .map(ValidatedRefName::as_str),
            Some("refs/remotes/te/am/main")
        );
        assert_eq!(
            snapshot
                .remote_head(&named("refs/remotes/origin/feature/x").remote)
                .map(ValidatedRefName::as_str),
            Some("refs/remotes/origin/main")
        );
        assert_eq!(
            snapshot.remote_head(&named("refs/remotes/bare/main").remote),
            None
        );
    }

    /// `cleanup_whitespace` reduces a message this service sent by the rule
    /// Git already applied to the one it stored, so any divergence makes an
    /// identical message compare unequal. Differential against the tool
    /// itself, because `git stripspace` with no arguments *is*
    /// `--cleanup=whitespace`: it right-trims with Git's own `isspace`, which
    /// takes CR but leaves vertical tab and form feed alone.
    #[test]
    fn cleanup_whitespace_agrees_with_git_stripspace() {
        const MESSAGES: [&str; 8] = [
            "a\r\r\n",
            "subject\r\n\r\n\r\nbody  \t\r\n",
            "  a leading space is kept\n#a comment is kept\n\n",
            "\n\n\nsubject\n\n\n\nbody\n\n\n",
            "a vertical tab \x0b\nand a form feed \x0c\n",
            "\t \t \n",
            "",
            "one line, no newline",
        ];
        for message in MESSAGES {
            let Some(expected) = git_stripspace(message) else {
                eprintln!("skipping: `git` is not available on PATH");
                return;
            };
            assert_eq!(cleanup_whitespace(message), expected, "message {message:?}");
        }
    }

    /// The real `git stripspace`'s answer, with the trailing newline it always
    /// appends removed, or `None` when Git is not installed.
    fn git_stripspace(message: &str) -> Option<String> {
        use std::io::Write;

        let mut child = std::process::Command::new("git")
            .arg("stripspace")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(message.as_bytes())
            .expect("the message reaches git");
        let output = child.wait_with_output().expect("git stripspace runs");
        assert!(output.status.success(), "git stripspace failed");
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 from a UTF-8 message");
        Some(stdout.strip_suffix('\n').unwrap_or(&stdout).to_owned())
    }

    #[test]
    fn file_tree_synthesizes_directories_without_touching_the_filesystem() {
        let paths = vec![
            path("src/app/main.rs"),
            path("src/lib.rs"),
            path("Cargo.toml"),
            path("src/app/ui/view.rs"),
        ];
        let rows = build_file_tree(&paths);
        let rendered: Vec<_> = rows
            .iter()
            .map(|row| (row.path.display().into_owned(), row.depth, row.directory))
            .collect();

        assert_eq!(
            rendered,
            vec![
                ("src".to_owned(), 0, true),
                ("src/app".to_owned(), 1, true),
                ("src/app/ui".to_owned(), 2, true),
                ("src/app/ui/view.rs".to_owned(), 3, false),
                ("src/app/main.rs".to_owned(), 2, false),
                ("src/lib.rs".to_owned(), 1, false),
                ("Cargo.toml".to_owned(), 0, false),
            ]
        );
    }

    #[test]
    fn path_lists_deduplicate_unmerged_index_stages() {
        let paths =
            parse_path_list(b"src/main.rs\0src/main.rs\0Cargo.toml\0", "git ls-files").unwrap();
        assert_eq!(paths, vec![path("Cargo.toml"), path("src/main.rs")]);
    }

    #[test]
    fn selection_expands_renames_and_removes_duplicates() {
        let selection = vec![
            PathSelection {
                path: path("new.txt"),
                old_path: Some(path("old.txt")),
            },
            PathSelection::new(path("old.txt")),
            PathSelection::new(path("new.txt")),
        ];
        assert_eq!(
            selection_pathspecs(&selection),
            vec![path("old.txt"), path("new.txt")]
        );
    }

    #[test]
    fn pathspec_commands_place_literal_pathspecs_before_the_repository() {
        let service = fake_service(Vec::new());
        let command = service
            .stage_command(
                Path::new("/repo"),
                &[path(":(top)old.txt"), path("new name.txt")],
            )
            .unwrap();

        assert_eq!(command.program, PathBuf::from("git"));
        assert_eq!(
            command.arguments,
            vec![
                OsString::from("--literal-pathspecs"),
                OsString::from("-C"),
                OsString::from("/repo"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("add"),
                OsString::from("-A"),
                OsString::from("--"),
                OsString::from(":(top)old.txt"),
                OsString::from("new name.txt"),
            ]
        );
        // A mutation must not disable optional locks: it needs the index lock.
        assert!(
            !command
                .environment
                .contains_key(OsStr::new("GIT_OPTIONAL_LOCKS"))
        );
        assert_eq!(
            command.environment.get(OsStr::new("GIT_TERMINAL_PROMPT")),
            Some(&OsString::from("0"))
        );

        let empty = service.stage_command(Path::new("/repo"), &[]);
        assert!(matches!(empty, Err(ValidationError::EmptySelection)));
    }

    /// `git add` and `git restore --worktree` match a pathspec against the
    /// index and the working tree. The source half of a staged rename is in
    /// neither, so listing it made the whole invocation fatal — exit 128,
    /// nothing staged — and reported as maybe-partial.
    #[test]
    fn a_rename_source_the_index_has_lost_is_not_sent_as_a_pathspec() {
        let selection = vec![PathSelection {
            path: path("new name.txt"),
            old_path: Some(path("old name.txt")),
        }];

        // The index no longer has the source: only HEAD does.
        let service = fake_service(vec![success(b""), success(b"")]);
        let outcome = service
            .stage(
                &StageRequest {
                    worktree: PathBuf::from("/repo"),
                    selection: selection.clone(),
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(
            outcome,
            MutationOutcome::Applied {
                paths: vec![path("new name.txt")]
            }
        );
        let commands = service.executor.commands.lock().unwrap();
        assert_eq!(
            commands[1].arguments.last(),
            Some(&OsString::from("new name.txt"))
        );
        assert!(
            !commands[1]
                .arguments
                .contains(&OsString::from("old name.txt"))
        );
        drop(commands);

        // The index still has it, which is the unstaged rename this
        // application also has to stage as a pair.
        let service = fake_service(vec![success(b"old name.txt\0"), success(b"")]);
        let outcome = service
            .stage(
                &StageRequest {
                    worktree: PathBuf::from("/repo"),
                    selection,
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(
            outcome,
            MutationOutcome::Applied {
                paths: vec![path("old name.txt"), path("new name.txt")]
            }
        );
    }

    /// `core.fsmonitor` set to a non-boolean value names a program Git runs
    /// whenever it refreshes the index, and `add`, `restore`, `rm --cached` and
    /// `commit` all refresh the index. Disabling it on reads alone would let a
    /// repository that arrived as an archive run code of its own choosing the
    /// first time the user staged a file, so every invocation is checked here.
    #[test]
    fn every_git_invocation_disables_the_repository_filesystem_monitor() {
        let service = fake_service(Vec::new());
        let worktree = PathBuf::from("/repo");
        let mutations = [
            service
                .stage_command(&worktree, &[path("new.txt")])
                .unwrap(),
            service
                .commit_command(&CommitRequest {
                    worktree: worktree.clone(),
                    message: "subject".into(),
                })
                .unwrap(),
            service.push_command(&publish_request()).unwrap(),
            service.git_literal_mutation(
                &worktree,
                pathspec_arguments(["restore", "--staged"], &[path("new.txt")]),
            ),
            service.git_literal_mutation(
                &worktree,
                pathspec_arguments(["rm", "--cached", "--ignore-unmatch"], &[path("new.txt")]),
            ),
            service.git_literal_mutation(
                &worktree,
                pathspec_arguments(["restore", "--worktree"], &[path("new.txt")]),
            ),
        ];
        let reads = [
            service.status_command(&worktree),
            service.remotes_command(&worktree),
        ];

        for command in mutations.iter().chain(reads.iter()) {
            let position = command
                .arguments
                .iter()
                .position(|argument| argument == OsStr::new("core.fsmonitor=false"))
                .unwrap_or_else(|| {
                    panic!("{:?} does not disable core.fsmonitor", command.arguments)
                });
            // Git only accepts `-c` ahead of the subcommand, so the flag has to
            // sit in the global option block rather than anywhere in the argv.
            assert_eq!(command.arguments[position - 1], OsString::from("-c"));
            assert!(
                position <= 4,
                "core.fsmonitor=false is not in the global option block: {:?}",
                command.arguments
            );
        }

        // The read-only lock suppression stays read-only: a mutation needs the
        // index lock it would otherwise be denied.
        for command in &mutations {
            assert!(
                !command
                    .environment
                    .contains_key(OsStr::new("GIT_OPTIONAL_LOCKS"))
            );
        }
        for command in &reads {
            assert_eq!(
                command.environment.get(OsStr::new("GIT_OPTIONAL_LOCKS")),
                Some(&OsString::from("0"))
            );
        }
    }

    #[test]
    fn history_command_pages_two_hundred_commits_from_a_resolved_start() {
        let service = fake_service(Vec::new());
        let command =
            service.history_command(Path::new("/repo"), &oid(2), 3, Some(&path("src/main.rs")));

        assert_eq!(command.arguments[0], OsString::from("--literal-pathspecs"));
        assert_eq!(command.arguments[1], OsString::from("-C"));
        assert!(command.arguments.contains(&OsString::from("--skip=600")));
        assert!(
            command
                .arguments
                .contains(&OsString::from("--max-count=200"))
        );
        assert!(command.arguments.contains(&OsString::from("--date=raw")));
        assert!(
            command
                .arguments
                .contains(&OsString::from(format!("--format={HISTORY_FORMAT}")))
        );
        assert_eq!(
            command.arguments.last(),
            Some(&OsString::from("src/main.rs"))
        );
        assert_eq!(HISTORY_FORMAT.matches("%x00").count() + 1, HISTORY_FIELDS);
    }

    #[test]
    fn history_of_an_unborn_branch_is_empty_without_running_git_log() {
        let service = fake_service(vec![exit(1, b"")]);
        let commits = service
            .history(
                &HistoryRequest {
                    worktree: PathBuf::from("/repo"),
                    revision: HistoryRevision::Head,
                    page: 0,
                    path: None,
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert!(commits.is_empty());
        assert_eq!(service.executor.commands.lock().unwrap().len(), 1);
    }

    #[test]
    fn unstage_restores_from_head_when_head_exists() {
        let service = fake_service(vec![
            success(format!("{}\n", oid(3)).as_bytes()),
            success(b""),
        ]);
        let outcome = service
            .unstage(
                &UnstageRequest {
                    worktree: PathBuf::from("/repo"),
                    selection: vec![PathSelection {
                        path: path("new.txt"),
                        old_path: Some(path("old.txt")),
                    }],
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert!(matches!(outcome, MutationOutcome::Applied { .. }));
        let commands = service.executor.commands.lock().unwrap();
        assert_eq!(
            commands[1].arguments,
            vec![
                OsString::from("--literal-pathspecs"),
                OsString::from("-C"),
                OsString::from("/repo"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("restore"),
                OsString::from("--staged"),
                OsString::from("--"),
                OsString::from("old.txt"),
                OsString::from("new.txt"),
            ]
        );
    }

    #[test]
    fn unstage_on_an_unborn_head_removes_only_indexed_paths() {
        let service = fake_service(vec![
            // No HEAD.
            exit(1, b""),
            // Only one of the two selected paths is in the index.
            success(b"present.txt\0"),
            success(b""),
        ]);
        let outcome = service
            .unstage(
                &UnstageRequest {
                    worktree: PathBuf::from("/repo"),
                    selection: vec![
                        PathSelection::new(path("present.txt")),
                        PathSelection::new(path("absent.txt")),
                    ],
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert_eq!(
            outcome,
            MutationOutcome::Applied {
                paths: vec![path("present.txt")]
            }
        );
        let commands = service.executor.commands.lock().unwrap();
        assert_eq!(
            commands[2].arguments,
            vec![
                OsString::from("--literal-pathspecs"),
                OsString::from("-C"),
                OsString::from("/repo"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("rm"),
                OsString::from("--cached"),
                OsString::from("--ignore-unmatch"),
                OsString::from("--"),
                OsString::from("present.txt"),
            ]
        );
    }

    #[test]
    fn unstage_on_an_unborn_head_with_no_indexed_paths_runs_no_mutation() {
        let service = fake_service(vec![exit(1, b""), success(b"")]);
        let outcome = service
            .unstage(
                &UnstageRequest {
                    worktree: PathBuf::from("/repo"),
                    selection: vec![PathSelection::new(path("absent.txt"))],
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert_eq!(outcome, MutationOutcome::Applied { paths: Vec::new() });
        assert_eq!(service.executor.commands.lock().unwrap().len(), 2);
    }

    #[test]
    fn stage_that_never_started_is_definitively_not_applied() {
        let service = fake_service(vec![Err(unstarted_failure())]);
        let outcome = service
            .stage(
                &StageRequest {
                    worktree: PathBuf::from("/repo"),
                    selection: vec![PathSelection::new(path("a.txt"))],
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(matches!(outcome, MutationOutcome::NotApplied { .. }));

        let service = fake_service(vec![Err(launched_failure(b""))]);
        let outcome = service
            .stage(
                &StageRequest {
                    worktree: PathBuf::from("/repo"),
                    selection: vec![PathSelection::new(path("a.txt"))],
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(matches!(outcome, MutationOutcome::OutcomeUnknown { .. }));
    }

    #[test]
    fn discard_requires_confirmation_and_never_removes_a_directory() {
        let worktree = tempfile::tempdir().unwrap();
        fs::create_dir(worktree.path().join("keep")).unwrap();
        fs::write(worktree.path().join("keep/file.txt"), b"data").unwrap();

        // One `git ls-files` read per request with untracked paths, which
        // re-establishes that they are still untracked.
        let service = fake_service(vec![success(b""), success(b"")]);
        let unconfirmed = service.discard(
            &DiscardRequest {
                worktree: worktree.path().to_path_buf(),
                tracked: Vec::new(),
                untracked: vec![path("keep/file.txt")],
                confirmed: false,
            },
            &CancellationToken::new(),
        );
        assert!(matches!(
            unconfirmed,
            Err(RepositoryError::Validation(
                ValidationError::ConfirmationRequired
            ))
        ));
        assert!(worktree.path().join("keep/file.txt").exists());

        let directory = service
            .discard(
                &DiscardRequest {
                    worktree: worktree.path().to_path_buf(),
                    tracked: Vec::new(),
                    untracked: vec![path("keep")],
                    confirmed: true,
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(matches!(directory, MutationOutcome::NotApplied { .. }));
        assert!(worktree.path().join("keep/file.txt").exists());

        let removed = service
            .discard(
                &DiscardRequest {
                    worktree: worktree.path().to_path_buf(),
                    tracked: Vec::new(),
                    untracked: vec![path("keep/file.txt"), path("keep/file.txt")],
                    confirmed: true,
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(
            removed,
            MutationOutcome::Applied {
                paths: vec![path("keep/file.txt")]
            }
        );
        assert!(!worktree.path().join("keep/file.txt").exists());
        assert!(worktree.path().join("keep").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn discard_refuses_to_follow_a_symlinked_directory_out_of_the_worktree() {
        let worktree = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), worktree.path().join("link")).unwrap();

        let service = fake_service(vec![success(b"")]);
        let outcome = service
            .discard(
                &DiscardRequest {
                    worktree: worktree.path().to_path_buf(),
                    tracked: Vec::new(),
                    untracked: vec![path("link/secret.txt")],
                    confirmed: true,
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert!(matches!(
            outcome,
            MutationOutcome::NotApplied {
                failure: MutationFailure::Path { .. }
            }
        ));
        assert!(outside.path().join("secret.txt").exists());
    }

    /// The ancestor check and the unlink used to be two independent path
    /// resolutions, so a directory that was a real directory when it was
    /// checked could be a symlink by the time `remove_file` followed it. The
    /// unlink is now relative to the descriptor the check verified, which makes
    /// the window unexploitable rather than merely narrow.
    ///
    /// The swap runs concurrently because that is the only way to exercise an
    /// atomicity defect at all; the assertion is one-sided, so this can fail
    /// only when a path outside the worktree really was deleted.
    #[cfg(unix)]
    #[test]
    fn discard_never_unlinks_outside_the_worktree_while_an_ancestor_is_swapped() {
        const ROUNDS: usize = 400;

        let worktree = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let hostage = outside.path().join("victim.txt");
        fs::write(&hostage, b"outside").unwrap();

        let parked_directory = worktree.path().join("parked-directory");
        let parked_link = worktree.path().join("parked-link");
        let active = worktree.path().join("sub");
        fs::create_dir(&parked_directory).unwrap();
        std::os::unix::fs::symlink(outside.path(), &parked_link).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let swapper = {
            let stop = Arc::clone(&stop);
            let parked_directory = parked_directory.clone();
            let parked_link = parked_link.clone();
            let active = active.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    for parked in [&parked_directory, &parked_link] {
                        let _ = fs::rename(parked, &active);
                        let _ = fs::rename(&active, parked);
                    }
                }
            })
        };

        let request = DiscardRequest {
            worktree: worktree.path().to_path_buf(),
            tracked: Vec::new(),
            untracked: vec![path("sub/victim.txt")],
            confirmed: true,
        };
        let service = fake_service((0..=ROUNDS).map(|_| success(b"")).collect());
        for _ in 0..ROUNDS {
            let _ = fs::write(parked_directory.join("victim.txt"), b"inside");
            let _ = fs::write(active.join("victim.txt"), b"inside");
            let _ = service.discard(&request, &CancellationToken::new());
            assert!(
                hostage.exists(),
                "discard followed a swapped ancestor out of the worktree"
            );
        }

        stop.store(true, Ordering::Relaxed);
        swapper.join().unwrap();

        // Each swap is balanced, so the swapper leaves `active` absent and both
        // prepared entries where they started. This quiescent round proves the
        // deletion the loop above was racing really happens, rather than being
        // refused every time.
        fs::rename(&parked_directory, &active).unwrap();
        fs::write(active.join("victim.txt"), b"inside").unwrap();
        let outcome = service
            .discard(&request, &CancellationToken::new())
            .unwrap();
        assert_eq!(
            outcome,
            MutationOutcome::Applied {
                paths: vec![path("sub/victim.txt")]
            }
        );
        assert!(!active.join("victim.txt").exists());
        assert!(hostage.exists());
    }

    /// `symlink_metadata` refuses to follow only the *final* component, so
    /// classifying `link/secret.txt` by name reported the file the link's target
    /// contains and the read that followed happily returned its bytes. Every
    /// component is now resolved beneath the worktree, so a link anywhere in the
    /// path is a refusal rather than a redirect.
    #[cfg(unix)]
    #[test]
    fn a_worktree_side_is_never_read_through_a_symlinked_directory() {
        let worktree = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), worktree.path().join("link")).unwrap();

        // An empty index listing leaves the old side absent, so the comparison
        // is decided entirely by what the worktree side reads.
        let service = fake_service(vec![success(b"")]);
        let content = service
            .diff(
                &DiffRequest {
                    worktree: worktree.path().to_path_buf(),
                    basis: DiffBasis::IndexToWorktree,
                    path: path("link/secret.txt"),
                    old_path: None,
                },
                &CancellationToken::new(),
            )
            .unwrap();

        let DiffContent::Comparison { new, .. } = content else {
            panic!("expected a comparison, got {content:?}");
        };
        assert!(
            matches!(new, DiffSide::Unreadable { .. }),
            "the worktree side was read through a symlinked directory: {new:?}"
        );
    }

    /// `--cleanup=strip` removes every line starting with `core.commentChar`,
    /// which is why Git's own default for `-F`/`--file` is `whitespace`: a
    /// message that was never shown in an editor has no commentary in it, so
    /// a subject like `#42 fix the thing` is content, not a comment.
    #[test]
    fn commit_sends_the_message_on_stdin_without_stripping_comment_lines() {
        let service = fake_service(Vec::new());
        let command = service
            .commit_command(&CommitRequest {
                worktree: PathBuf::from("/repo"),
                message: "#42 fix the thing\n\nbody\n".into(),
            })
            .unwrap();

        assert_eq!(
            command.arguments,
            vec![
                OsString::from("-C"),
                OsString::from("/repo"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("commit"),
                OsString::from("--file=-"),
                OsString::from("--cleanup=whitespace"),
            ]
        );
        assert_eq!(
            command.stdin.as_deref(),
            Some(&b"#42 fix the thing\n\nbody\n"[..])
        );
        assert!(matches!(
            service.commit_command(&CommitRequest {
                worktree: PathBuf::from("/repo"),
                message: "   \n".into(),
            }),
            Err(ValidationError::EmptyCommitMessage)
        ));
    }

    #[test]
    fn commit_without_staged_changes_is_not_created() {
        let service = fake_service(vec![
            // Nothing staged, and no sequencer state waiting to be concluded.
            exit(0, b""),
            success(b".git/MERGE_HEAD\n.git/CHERRY_PICK_HEAD\n.git/REVERT_HEAD\n"),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert!(matches!(
            outcome,
            CommitOutcome::NotCreated {
                failure: MutationFailure::Rejected { .. }
            }
        ));
        // Nothing beyond the two questions ran: no commit was attempted.
        assert_eq!(service.executor.commands.lock().unwrap().len(), 2);
    }

    /// A `git commit` that exits 0 made the commit HEAD now names, and that is
    /// the end of the question. A `prepare-commit-msg` hook prepending a
    /// ticket prefix is the ordinary furniture of a corporate repository, and
    /// a message comparison that outranked Git's own exit code reported every
    /// commit in one as "git reported success but HEAD names a commit this did
    /// not write".
    #[test]
    fn a_hook_that_rewrote_the_message_does_not_unmake_a_successful_commit() {
        let service = fake_service(vec![
            // Staged changes exist.
            exit(1, b""),
            success(format!("{}\n", oid(1)).as_bytes()),
            // `git commit` succeeded.
            success(b""),
            success(format!("{}\n", oid(2)).as_bytes()),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();

        match outcome {
            CommitOutcome::Created {
                oid: created,
                warnings,
            } => {
                assert_eq!(created, oid(2));
                assert!(warnings.is_empty(), "nothing failed: {warnings:?}");
            }
            other => panic!("expected a created commit, got {other:?}"),
        }
        // No fifth command: the message the hook stored was never read, so no
        // canned answer for it had to exist. Reading it could only produce a
        // worse answer than the one Git already gave.
        assert_eq!(service.executor.commands.lock().unwrap().len(), 4);
    }

    #[test]
    fn commit_that_moved_head_despite_a_failure_is_created_with_a_warning() {
        let service = fake_service(vec![
            // Staged changes exist.
            exit(1, b""),
            success(format!("{}\n", oid(1)).as_bytes()),
            // The commit itself failed, e.g. a cancellation that arrived after
            // `git commit` had already moved the ref.
            exit(1, b""),
            success(format!("{}\n", oid(2)).as_bytes()),
            // The commit at the new HEAD carries this call's message, with the
            // `Change-Id` trailer a `commit-msg` hook is allowed to append.
            success(b"subject\n\nChange-Id: I0\n\0"),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();

        match outcome {
            CommitOutcome::Created {
                oid: created,
                warnings,
            } => {
                assert_eq!(created, oid(2));
                assert_eq!(warnings.len(), 1);
            }
            other => panic!("expected a created commit, got {other:?}"),
        }
        // The message really was consulted, and it was consulted about the
        // commit HEAD moved to: on this path the message is the only evidence
        // of authorship there is, so a version of this that reached the same
        // verdict without asking would be reaching it by assumption.
        let commands = service.executor.commands.lock().unwrap();
        assert_eq!(commands.len(), 5);
        assert!(
            commands[4].arguments.ends_with(&[
                OsString::from("log"),
                OsString::from("-z"),
                OsString::from("--max-count=1"),
                OsString::from("--format=%B"),
                OsString::from("--end-of-options"),
                OsString::from(oid(2).as_str()),
            ]),
            "{:?}",
            commands[4].arguments
        );
    }

    /// The embedded terminals share this worktree and `MutationLocks` cannot
    /// exclude them, so HEAD moving while this commit failed may be the user's
    /// own `git commit` landing. Claiming that object id attributed a
    /// stranger's commit — a stranger's message, a stranger's contents — to
    /// this call. Denying it is no better, because the hook that rewrites a
    /// message is entitled to make the same evidence disappear for a commit
    /// this call did write. Unknown is the only honest answer, and a
    /// reconciliation read that fails itself lands in the same place rather
    /// than becoming an error.
    #[test]
    fn a_commit_this_call_did_not_write_is_never_claimed_or_denied() {
        let service = fake_service(vec![
            exit(1, b""),
            success(format!("{}\n", oid(1)).as_bytes()),
            exit(1, b"the hook said no\n"),
            success(format!("{}\n", oid(2)).as_bytes()),
            success(b"someone else's work\n\0"),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(
            matches!(outcome, CommitOutcome::OutcomeUnknown { .. }),
            "got {outcome:?}"
        );

        // The authorship read is a read, and a read never downgrades a
        // mutation that may have completed into a hard error.
        let service = fake_service(vec![
            exit(1, b""),
            success(format!("{}\n", oid(1)).as_bytes()),
            exit(1, b"the hook said no\n"),
            success(format!("{}\n", oid(2)).as_bytes()),
            exit(128, b""),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .expect("a failed reconciliation read is an outcome, not an error");
        assert!(
            matches!(outcome, CommitOutcome::OutcomeUnknown { .. }),
            "got {outcome:?}"
        );
    }

    #[test]
    fn commit_that_left_head_alone_is_not_created() {
        let service = fake_service(vec![
            exit(1, b""),
            success(format!("{}\n", oid(1)).as_bytes()),
            Err(launched_failure(b"")),
            success(format!("{}\n", oid(1)).as_bytes()),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert!(matches!(outcome, CommitOutcome::NotCreated { .. }));
    }

    #[test]
    fn commit_is_unknown_when_head_cannot_be_read_afterwards() {
        let service = fake_service(vec![
            exit(1, b""),
            success(format!("{}\n", oid(1)).as_bytes()),
            success(b""),
            // Reconciliation itself failed.
            exit(128, b""),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert!(matches!(outcome, CommitOutcome::OutcomeUnknown { .. }));
    }

    #[test]
    fn commit_that_reported_success_without_moving_head_is_unknown() {
        let service = fake_service(vec![
            exit(1, b""),
            success(format!("{}\n", oid(1)).as_bytes()),
            success(b""),
            success(format!("{}\n", oid(1)).as_bytes()),
        ]);
        let outcome = service
            .commit(
                &CommitRequest {
                    worktree: PathBuf::from("/repo"),
                    message: "subject".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert!(matches!(
            outcome,
            CommitOutcome::OutcomeUnknown {
                failure: MutationFailure::Rejected { .. }
            }
        ));
    }

    fn publish_request() -> PushRequest {
        PushRequest {
            worktree: PathBuf::from("/repo"),
            branch: ValidatedBranchName::parse("feature").unwrap(),
            target: PushTarget::SetUpstream {
                remote: "origin".into(),
                remote_branch: ValidatedBranchName::parse("feature").unwrap(),
            },
        }
    }

    #[test]
    fn push_command_sets_upstream_with_an_explicit_refspec_and_never_forces() {
        let service = fake_service(Vec::new());
        let command = service.push_command(&publish_request()).unwrap();

        assert_eq!(
            command.arguments,
            vec![
                OsString::from("-C"),
                OsString::from("/repo"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("push"),
                OsString::from("--porcelain"),
                OsString::from("--set-upstream"),
                OsString::from("origin"),
                OsString::from("refs/heads/feature:refs/heads/feature"),
            ]
        );
        assert!(
            !command
                .arguments
                .iter()
                .any(|argument| argument.to_string_lossy().contains("force"))
        );

        let existing = service
            .push_command(&PushRequest {
                worktree: PathBuf::from("/repo"),
                branch: ValidatedBranchName::parse("feature").unwrap(),
                target: PushTarget::Upstream {
                    remote: "upstream".into(),
                    remote_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
                },
            })
            .unwrap();
        assert!(
            !existing
                .arguments
                .contains(&OsString::from("--set-upstream"))
        );
        assert_eq!(
            existing.arguments.last(),
            Some(&OsString::from("refs/heads/feature:refs/heads/main"))
        );

        assert!(matches!(
            service.push_command(&PushRequest {
                worktree: PathBuf::from("/repo"),
                branch: ValidatedBranchName::parse("feature").unwrap(),
                target: PushTarget::Upstream {
                    remote: "--upload-pack=evil".into(),
                    remote_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
                },
            }),
            Err(ValidationError::InvalidRemote(_))
        ));
    }

    /// The refspec's destination is what the *remote* calls the branch, whether
    /// the upstream arrived as a remote-tracking ref or out of
    /// `branch.<name>.merge`.
    #[test]
    fn a_refspec_destination_is_never_a_remote_tracking_ref() {
        let service = fake_service(Vec::new());
        let refspec = |remote: &str, upstream: &str| {
            service
                .push_command(&PushRequest {
                    worktree: PathBuf::from("/repo"),
                    branch: ValidatedBranchName::parse("feature").unwrap(),
                    target: PushTarget::Upstream {
                        remote: remote.into(),
                        remote_ref: ValidatedRefName::parse(upstream).unwrap(),
                    },
                })
                .map(|command| command.arguments.last().unwrap().clone())
        };

        // What a snapshot's `%(upstream)` gives, which is the spelling the
        // sidebar carries. Pushing this one verbatim created a literal
        // `refs/remotes/origin/main` in the remote and left the branch alone.
        assert_eq!(
            refspec("origin", "refs/remotes/origin/main").unwrap(),
            OsString::from("refs/heads/feature:refs/heads/main")
        );
        // A branch published under another name follows the upstream's name.
        assert_eq!(
            refspec("origin", "refs/remotes/origin/published").unwrap(),
            OsString::from("refs/heads/feature:refs/heads/published")
        );
        // A slash in the branch name is not a second boundary to guess at.
        assert_eq!(
            refspec("origin", "refs/remotes/origin/topic/one").unwrap(),
            OsString::from("refs/heads/feature:refs/heads/topic/one")
        );
        // Split at the remote name that is known, not at the first slash: with
        // a remote called `te`, this ref tracks the branch `am/main`.
        assert_eq!(
            refspec("te", "refs/remotes/te/am/main").unwrap(),
            OsString::from("refs/heads/feature:refs/heads/am/main")
        );
        // What `branch.<name>.merge` holds is already the remote's own name.
        assert_eq!(
            refspec("origin", "refs/heads/main").unwrap(),
            OsString::from("refs/heads/feature:refs/heads/main")
        );
        // A tracking ref under a different remote names no branch on this one,
        // and which branch it meant is not something to guess on the wire.
        assert!(matches!(
            refspec("origin", "refs/remotes/upstream/main"),
            Err(ValidationError::ForeignTrackingRef { .. })
        ));
        // Nor can the wire-side type be spelled with a tracking ref directly,
        // which is also what keeps a deserialized outcome honest.
        assert!(matches!(
            RemoteSideRef::parse("refs/remotes/origin/main"),
            Err(ValidationError::NotARemoteSideRef(_))
        ));
        assert!(matches!(
            RemoteSideRef::parse("main"),
            Err(ValidationError::NotARemoteSideRef(_))
        ));
    }

    #[test]
    fn push_validates_the_remote_branch_name_with_git() {
        let service = fake_service(vec![exit(1, b"")]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();

        assert!(matches!(
            outcome,
            PushOutcome::NotPushed {
                failure: MutationFailure::Rejected { .. }
            }
        ));
        let commands = service.executor.commands.lock().unwrap();
        assert_eq!(
            commands[0].arguments,
            vec![
                OsString::from("-C"),
                OsString::from("/repo"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("--no-optional-locks"),
                OsString::from("-c"),
                OsString::from("diff.autoRefreshIndex=false"),
                OsString::from("check-ref-format"),
                OsString::from("refs/heads/feature"),
            ]
        );
    }

    /// The two `check-ref-format` invocations answer the same question about
    /// the same kind of argument and touch no repository state, so they are
    /// both reads. One of them used to be built as a mutation, which took the
    /// index lock and refreshed the index to inspect a string.
    #[test]
    fn every_command_that_plans_a_worktree_is_classified_as_a_read() {
        let service = fake_service(Vec::new());
        // The destination is validated against a real directory before any
        // command is built.
        let managed = tempfile::tempdir().unwrap();
        let plan = service
            .plan_worktree_creation(&CreateWorktreeRequest {
                repository: PathBuf::from("/repo"),
                managed_root: managed.path().to_path_buf(),
                destination: managed.path().join("feature"),
                branch: ValidatedBranchName::parse("feature").unwrap(),
                base: BaseRevision::LocalBranch(ValidatedBranchName::parse("main").unwrap()),
                track: false,
                operation_id: OperationId::new(),
            })
            .unwrap();

        for command in [
            &plan.validate_branch,
            &plan.resolve_base,
            &plan.check_branch_absent,
        ] {
            assert!(
                command
                    .arguments
                    .contains(&OsString::from("--no-optional-locks")),
                "{:?} must decline the index lock",
                command.arguments
            );
            assert!(
                command
                    .arguments
                    .contains(&OsString::from("diff.autoRefreshIndex=false")),
                "{:?} must not refresh the index",
                command.arguments
            );
        }
    }

    #[test]
    fn a_successful_push_reports_the_pushed_commit() {
        let service = fake_service(vec![
            success(b""),
            success(format!("{}\n", oid(4)).as_bytes()),
            success(b"To /origin\n*\trefs/heads/feature:refs/heads/feature\t[new branch]\nDone\n"),
        ]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();

        assert_eq!(
            outcome,
            PushOutcome::Pushed {
                remote: "origin".into(),
                remote_ref: RemoteSideRef::parse("refs/heads/feature").unwrap(),
                oid: oid(4),
            }
        );
    }

    #[test]
    fn a_rejected_push_is_definitively_not_pushed() {
        let service = fake_service(vec![
            success(b""),
            success(format!("{}\n", oid(4)).as_bytes()),
            exit(
                1,
                b"To /origin\n!\trefs/heads/feature:refs/heads/feature\t[rejected] (fetch first)\nDone\n",
            ),
        ]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();

        assert!(matches!(outcome, PushOutcome::NotPushed { .. }));
        // The remote is never consulted: `!` already proved the ref did not move.
        assert_eq!(service.executor.commands.lock().unwrap().len(), 3);
    }

    #[test]
    fn a_push_that_never_started_is_not_pushed() {
        let service = fake_service(vec![
            success(b""),
            success(format!("{}\n", oid(4)).as_bytes()),
            Err(unstarted_failure()),
        ]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();

        assert!(matches!(outcome, PushOutcome::NotPushed { .. }));
        assert_eq!(service.executor.commands.lock().unwrap().len(), 3);
    }

    #[test]
    fn an_uncertain_push_whose_remote_matches_is_reconciled_as_pushed() {
        let service = fake_service(vec![
            success(b""),
            success(format!("{}\n", oid(4)).as_bytes()),
            Err(launched_failure(b"To /origin\n")),
            success(format!("{}\trefs/heads/feature\n", oid(4)).as_bytes()),
        ]);
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let outcome = service.push(&publish_request(), &cancelled).unwrap();

        assert_eq!(
            outcome,
            PushOutcome::Pushed {
                remote: "origin".into(),
                remote_ref: RemoteSideRef::parse("refs/heads/feature").unwrap(),
                oid: oid(4),
            }
        );
        let commands = service.executor.commands.lock().unwrap();
        assert_eq!(
            commands[3].arguments,
            vec![
                OsString::from("-C"),
                OsString::from("/repo"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("--no-optional-locks"),
                OsString::from("-c"),
                OsString::from("diff.autoRefreshIndex=false"),
                OsString::from("ls-remote"),
                OsString::from("--exit-code"),
                OsString::from("origin"),
                OsString::from("refs/heads/feature"),
            ]
        );
    }

    #[test]
    fn an_uncertain_push_whose_remote_differs_stays_unknown() {
        let service = fake_service(vec![
            success(b""),
            success(format!("{}\n", oid(4)).as_bytes()),
            Err(launched_failure(b"")),
            success(format!("{}\trefs/heads/feature\n", oid(5)).as_bytes()),
        ]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();
        assert!(matches!(outcome, PushOutcome::OutcomeUnknown { .. }));

        // An absent remote ref is also not proof that the push landed.
        let service = fake_service(vec![
            success(b""),
            success(format!("{}\n", oid(4)).as_bytes()),
            Err(launched_failure(b"")),
            exit(2, b""),
        ]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();
        assert!(matches!(outcome, PushOutcome::OutcomeUnknown { .. }));

        // Neither is a failed reconciliation lookup.
        let service = fake_service(vec![
            success(b""),
            success(format!("{}\n", oid(4)).as_bytes()),
            Err(launched_failure(b"")),
            exit(128, b""),
        ]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();
        assert!(matches!(outcome, PushOutcome::OutcomeUnknown { .. }));
    }

    #[test]
    fn a_push_of_a_missing_local_branch_is_not_pushed() {
        let service = fake_service(vec![success(b""), exit(1, b"")]);
        let outcome = service
            .push(&publish_request(), &CancellationToken::new())
            .unwrap();

        assert!(matches!(
            outcome,
            PushOutcome::NotPushed {
                failure: MutationFailure::Rejected { .. }
            }
        ));
    }

    #[test]
    fn the_upstream_push_target_comes_from_branch_configuration() {
        let service = fake_service(vec![success(b"origin\0"), success(b"refs/heads/main\0")]);
        let target = service
            .upstream_push_target(
                Path::new("/repo"),
                &ValidatedBranchName::parse("feature").unwrap(),
                &CancellationToken::new(),
            )
            .unwrap();

        assert_eq!(
            target,
            Some(PushTarget::Upstream {
                remote: "origin".into(),
                remote_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
            })
        );

        let service = fake_service(vec![exit(1, b""), exit(1, b"")]);
        assert_eq!(
            service
                .upstream_push_target(
                    Path::new("/repo"),
                    &ValidatedBranchName::parse("feature").unwrap(),
                    &CancellationToken::new(),
                )
                .unwrap(),
            None
        );
    }

    #[test]
    fn an_unmerged_path_reports_its_index_stages_rather_than_an_added_file() {
        let worktree = tempfile::tempdir().unwrap();
        fs::write(worktree.path().join("conflict.txt"), b"merged by hand\n").unwrap();
        let stages = format!(
            "100644 {} 1\tconflict.txt\0100644 {} 2\tconflict.txt\0100644 {} 3\tconflict.txt\0",
            oid(1),
            oid(2),
            oid(3)
        );
        let service = fake_service(vec![
            success(stages.as_bytes()),
            success(format!("{} blob 5\nbase\n\n", oid(1)).as_bytes()),
            success(format!("{} blob 5\nours\n\n", oid(2)).as_bytes()),
            success(format!("{} blob 7\ntheirs\n\n", oid(3)).as_bytes()),
        ]);
        let content = service
            .diff(
                &DiffRequest {
                    worktree: worktree.path().to_path_buf(),
                    basis: DiffBasis::IndexToWorktree,
                    path: path("conflict.txt"),
                    old_path: None,
                },
                &CancellationToken::new(),
            )
            .unwrap();

        match content {
            DiffContent::Unmerged {
                base,
                ours,
                theirs,
                worktree: working,
            } => {
                assert_eq!(base.text(), Some("base\n"));
                assert_eq!(ours.text(), Some("ours\n"));
                assert_eq!(theirs.text(), Some("theirs\n"));
                assert_eq!(working.text(), Some("merged by hand\n"));
            }
            other => panic!("expected an unmerged comparison, got {other:?}"),
        }
    }

    #[test]
    fn an_untracked_file_compares_against_an_absent_index_side() {
        let worktree = tempfile::tempdir().unwrap();
        fs::write(worktree.path().join("new.txt"), b"fresh\n").unwrap();
        let service = fake_service(vec![success(b"")]);
        let content = service
            .diff(
                &DiffRequest {
                    worktree: worktree.path().to_path_buf(),
                    basis: DiffBasis::IndexToWorktree,
                    path: path("new.txt"),
                    old_path: None,
                },
                &CancellationToken::new(),
            )
            .unwrap();

        assert_eq!(
            content,
            DiffContent::Comparison {
                old: DiffSide::Absent,
                new: DiffSide::Text {
                    text: "fresh\n".into(),
                    bytes: 6
                },
                old_label: "Index".into(),
                new_label: "Working tree".into(),
            }
        );
    }

    #[test]
    fn binary_and_submodule_sides_are_reported_as_metadata() {
        let worktree = tempfile::tempdir().unwrap();
        let service = fake_service(vec![success(
            format!("160000 commit {}\tvendor/dep\0", oid(7)).as_bytes(),
        )]);
        let content = service
            .diff(
                &DiffRequest {
                    worktree: worktree.path().to_path_buf(),
                    basis: DiffBasis::CommitToParent {
                        oid: oid(8),
                        parent: None,
                    },
                    path: path("vendor/dep"),
                    old_path: None,
                },
                &CancellationToken::new(),
            )
            .unwrap();

        match content {
            DiffContent::Comparison {
                old,
                new,
                old_label,
                ..
            } => {
                // A root commit has no parent side at all.
                assert_eq!(old, DiffSide::Absent);
                assert_eq!(old_label, "root commit");
                // The gitlink's object is never read as a blob.
                assert_eq!(new, DiffSide::Submodule { oid: Some(oid(7)) });
            }
            other => panic!("expected a comparison, got {other:?}"),
        }
        assert_eq!(service.executor.commands.lock().unwrap().len(), 1);

        assert_eq!(
            classify_bytes(b"text\n".to_vec()),
            DiffSide::Text {
                text: "text\n".into(),
                bytes: 5
            }
        );
        assert_eq!(
            classify_bytes(b"bin\0ary".to_vec()),
            DiffSide::Binary { bytes: 7 }
        );
        assert_eq!(
            classify_bytes(b"invalid \xff utf8".to_vec()),
            DiffSide::InvalidUtf8 { bytes: 14 }
        );
    }

    #[test]
    fn a_head_to_index_diff_uses_the_rename_source_for_the_old_side() {
        let worktree = tempfile::tempdir().unwrap();
        let service = fake_service(vec![
            success(format!("{}\n", oid(9)).as_bytes()),
            success(format!("100644 blob {}\told.txt\0", oid(1)).as_bytes()),
            success(format!("{} blob 4\nold\n\n", oid(1)).as_bytes()),
            success(format!("100644 {} 0\tnew.txt\0", oid(2)).as_bytes()),
            success(format!("{} blob 4\nnew\n\n", oid(2)).as_bytes()),
        ]);
        let content = service
            .diff(
                &DiffRequest {
                    worktree: worktree.path().to_path_buf(),
                    basis: DiffBasis::HeadToIndex,
                    path: path("new.txt"),
                    old_path: Some(path("old.txt")),
                },
                &CancellationToken::new(),
            )
            .unwrap();

        let commands = service.executor.commands.lock().unwrap();
        assert_eq!(
            commands[1].arguments,
            vec![
                OsString::from("--literal-pathspecs"),
                OsString::from("-C"),
                OsString::from(worktree.path()),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
                OsString::from("--no-optional-locks"),
                OsString::from("-c"),
                OsString::from("diff.autoRefreshIndex=false"),
                OsString::from("ls-tree"),
                OsString::from("-z"),
                OsString::from(oid(9).as_str()),
                OsString::from("--"),
                OsString::from("old.txt"),
            ]
        );
        match content {
            DiffContent::Comparison {
                old_label,
                new_label,
                ..
            } => {
                assert_eq!(old_label, "HEAD");
                assert_eq!(new_label, "Index");
            }
            other => panic!("expected a comparison, got {other:?}"),
        }
    }

    #[test]
    fn an_oversize_object_reports_its_size_without_being_read() {
        let service = fake_service(vec![Err(ProcessFailure {
            kind: ProcessFailureKind::OutputLimit,
            program: "git".into(),
            exit_code: None,
            signal: None,
            stdout: format!("{} blob 99999999\npartial", oid(1)).into_bytes(),
            stderr: Vec::new(),
            message: "stdout exceeded its limit".into(),
            started: true,
        })]);
        let side = service
            .object_side(
                Path::new("/repo"),
                0o100_644,
                &oid(1),
                &CancellationToken::new(),
            )
            .unwrap();

        assert_eq!(side, DiffSide::Oversize { bytes: 99_999_999 });
    }

    #[test]
    fn batch_blobs_survive_newlines_and_nul_bytes_in_content() {
        let payload = format!("{} blob 6\na\0b\nc\n\n{} blob 0\n\n", oid(1), oid(2));
        let blobs = parse_batch_blobs(payload.as_bytes(), "git cat-file --batch").unwrap();
        assert_eq!(
            blobs,
            vec![
                BatchObject::Content(b"a\0b\nc\n".to_vec()),
                BatchObject::Content(Vec::new())
            ]
        );
        assert!(matches!(
            parse_batch_blobs(b"deadbeef blob 10\nshort", "git cat-file --batch"),
            Err(GitParseError::Truncated { .. })
        ));
        assert_eq!(batch_header_size(b"abc blob 42\nxx"), Some(42));
    }

    /// A blobless or partial clone whose promisor remote is unreachable
    /// answers `<oid> missing`, which is Git telling the truth rather than
    /// Git malfunctioning. It used to surface as a parse error over the whole
    /// read.
    #[test]
    fn a_missing_object_is_reported_as_unavailable_not_as_malformed_output() {
        let payload = format!("{} missing\n{} blob 2\nhi\n", oid(1), oid(2));
        let objects = parse_batch_blobs(payload.as_bytes(), "git cat-file --batch").unwrap();
        assert_eq!(
            objects,
            vec![BatchObject::Missing, BatchObject::Content(b"hi".to_vec())]
        );

        // A path whose blob Git cannot produce is unreadable, not empty: an
        // empty side would render as a whole-file deletion.
        let service = fake_service(vec![success(format!("{} missing\n", oid(1)).as_bytes())]);
        let side = service
            .object_side(
                Path::new("/repo"),
                0o100_644,
                &oid(1),
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(matches!(side, DiffSide::Unreadable { .. }));
    }

    #[test]
    fn mutation_locks_serialize_one_common_directory_and_release_it() {
        let locks = MutationLocks::new();
        let first = locks.lock(Path::new("/repo/.git"));
        // A different repository never waits on this one.
        let other = locks.lock(Path::new("/other/.git"));
        assert_eq!(locks.tracked_keys(), 2);
        drop(other);

        let contender = locks.clone();
        let entered = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&entered);
        let waiter = thread::spawn(move || {
            let guard = contender.lock(Path::new("/repo/.git"));
            flag.store(true, Ordering::Release);
            drop(guard);
        });
        thread::sleep(Duration::from_millis(50));
        assert!(
            !entered.load(Ordering::Acquire),
            "a second mutation on one common directory must wait"
        );
        drop(first);
        waiter.join().unwrap();
        assert!(entered.load(Ordering::Acquire));
        // Released keys are not retained for the life of the session.
        assert_eq!(locks.tracked_keys(), 0);
    }

    #[test]
    fn snapshot_collects_status_counts_files_remotes_and_refs() {
        let mut status = status_headers();
        status.extend_from_slice(
            b"1 M. N... 100644 100644 100644 1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 src/main.rs\0",
        );
        let mut refs = Vec::new();
        refs.extend_from_slice(b"refs/heads/main\0");
        refs.extend_from_slice(oid(1).as_str().as_bytes());
        refs.extend_from_slice(b"\0\0\0*\n");
        let service = fake_service(vec![
            success(&status),
            success(b"4\t1\tsrc/main.rs\0"),
            success(b""),
            success(b"H src/main.rs\0H Cargo.toml\0"),
            // No remotes configured.
            exit(1, b""),
            success(&refs),
        ]);
        let snapshot = service
            .snapshot(Path::new("/repo"), &CancellationToken::new())
            .unwrap();

        assert_eq!(snapshot.status.entries[0].added_lines, Some(4));
        assert_eq!(snapshot.status.entries[0].removed_lines, Some(1));
        assert_eq!(
            snapshot.files.paths,
            vec![path("Cargo.toml"), path("src/main.rs")]
        );
        assert_eq!(snapshot.files.tracked_count, 2);
        assert!(snapshot.remotes.is_empty());
        assert_eq!(snapshot.local_refs.len(), 1);
        let commands = service.executor.commands.lock().unwrap();
        assert!(commands[1].arguments.contains(&OsString::from("--cached")));
        assert!(!commands[2].arguments.contains(&OsString::from("--cached")));
        assert_eq!(commands[3].arguments.last(), Some(&OsString::from("-z")));
    }

    /// `git ls-files -o` lists *others*, i.e. untracked files. The combined
    /// listing is what the file tree wants — it shows untracked files — but
    /// counting all of it as tracked claimed one extra tracked file for every
    /// untracked one, which is how "6 files tracked" appeared beside a
    /// `git ls-files` that returned five.
    #[test]
    fn a_snapshot_counts_only_the_files_git_tracks() {
        let mut status = status_headers();
        status.extend_from_slice(b"? new.txt\0");
        let service = fake_service(vec![
            success(&status),
            success(b""),
            success(b""),
            success(b"? new.txt\0H Cargo.toml\0H src/main.rs\0"),
            // No remotes configured.
            exit(1, b""),
            success(b""),
        ]);
        let snapshot = service
            .snapshot(Path::new("/repo"), &CancellationToken::new())
            .unwrap();

        // The tree still shows the untracked file.
        assert_eq!(
            snapshot.files.paths,
            vec![path("Cargo.toml"), path("new.txt"), path("src/main.rs")]
        );
        assert_eq!(snapshot.files.tracked_count, 2);
        assert_eq!(snapshot.status.dirty.untracked, 1);
    }

    #[test]
    fn working_files_are_classified_by_tag_and_counted_once_per_path() {
        // An unmerged path arrives once per index stage, and `S` is an index
        // entry marked skip-worktree, which is tracked like any other.
        let files =
            parse_working_files(b"M c.txt\0M c.txt\0M c.txt\0? u.txt\0S skipped.txt\0").unwrap();

        assert_eq!(
            files.paths,
            vec![path("c.txt"), path("skipped.txt"), path("u.txt")]
        );
        assert_eq!(files.tracked_count, 2);
    }

    #[test]
    fn a_working_file_record_without_a_status_tag_is_rejected() {
        // Without `-t` the record is the bare path, and reading its first byte
        // as a tag would silently truncate every filename by two characters.
        assert!(parse_working_files(b"untagged.txt\0").is_err());
        assert!(parse_working_files(b"H\0").is_err());
        assert!(parse_working_files(b"H \0").is_err());

        // Only the tag and its separator are stripped; the path stays exact.
        let files = parse_working_files(b"H sp ace.txt\0").unwrap();
        assert_eq!(files.paths, vec![path("sp ace.txt")]);
        assert_eq!(files.tracked_count, 1);
    }

    /// `git status` rewrites `.git/index` whenever it finds the stat cache
    /// stale, and `git diff` against the working tree rewrites it whatever the
    /// optional-lock setting says. A watcher sees either write as a change and
    /// asks for another state read, which runs both commands again: an idle
    /// window spinning Git processes by the hundred. Both suppressions
    /// therefore belong on every read — and on no mutation, which is entitled
    /// to the index lock and to the refresh.
    #[test]
    fn every_read_declines_the_index_lock_and_the_index_refresh() {
        let service = fake_service(Vec::new());
        let worktree = PathBuf::from("/repo");
        let reads = [
            service.status_command(&worktree),
            service.numstat_command(&worktree, false),
            service.numstat_command(&worktree, true),
            service.working_files_command(&worktree),
            service.remotes_command(&worktree),
            service.refs_command(&worktree),
            service.worktree_list_command(&worktree),
            service.worktree_root_command(&worktree),
            service.history_command(&worktree, &oid(2), 0, None),
        ];
        let mutations = [
            service
                .stage_command(&worktree, &[path("new.txt")])
                .unwrap(),
            service
                .commit_command(&CommitRequest {
                    worktree: worktree.clone(),
                    message: "subject".into(),
                })
                .unwrap(),
            service.push_command(&publish_request()).unwrap(),
        ];

        for command in &reads {
            let lock = command
                .arguments
                .iter()
                .position(|argument| argument == OsStr::new("--no-optional-locks"))
                .unwrap_or_else(|| panic!("{:?} takes the index lock", command.arguments));
            let refresh = command
                .arguments
                .iter()
                .position(|argument| argument == OsStr::new("diff.autoRefreshIndex=false"))
                .unwrap_or_else(|| panic!("{:?} refreshes the index", command.arguments));
            // Git accepts a global option only ahead of the subcommand, so both
            // have to sit in the global option block rather than anywhere in
            // the argv.
            assert_eq!(command.arguments[refresh - 1], OsString::from("-c"));
            assert!(lock < refresh);
            assert!(
                refresh <= 7,
                "the index-refresh suppression left the global option block: {:?}",
                command.arguments
            );
        }
        for command in &mutations {
            assert!(
                !command
                    .arguments
                    .contains(&OsString::from("--no-optional-locks")),
                "{:?} must be allowed the index lock",
                command.arguments
            );
            assert!(
                !command
                    .arguments
                    .contains(&OsString::from("diff.autoRefreshIndex=false")),
                "{:?} must be allowed to refresh the index",
                command.arguments
            );
        }
    }

    /// Collects one subscriber's output so a test can read what was logged.
    #[derive(Clone)]
    struct LogSink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogSink {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Runs `body` with every log line captured, on this thread only.
    fn captured_logs(body: impl FnOnce()) -> String {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || LogSink(writer.clone()))
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        String::from_utf8(sink.lock().unwrap().clone()).expect("UTF-8 log output")
    }

    /// A whole run under `RUST_LOG=cagents=trace` produced no `cagents::` line
    /// at all, so the `git: process exited with status Some(128)` banner named
    /// neither the command that failed nor Git's own reason for failing.
    /// Attributing it needed a shim on `PATH`. Every failure now carries its
    /// argv, its status and Git's message.
    #[test]
    fn a_failing_git_command_is_logged_with_its_argv_status_and_reason() {
        let refusal = Ok(ProcessOutput {
            exit_code: Some(128),
            signal: None,
            success: false,
            stdout: Vec::new(),
            stderr: b"fatal: this operation must be run in a work tree\n".to_vec(),
        });
        let logs = captured_logs(|| {
            let service = fake_service(vec![refusal]);
            service
                .discover(Path::new("/repo/.git"), &CancellationToken::new())
                .expect_err("a refused query is a failure");
        });

        assert!(logs.contains("git command failed"), "{logs}");
        assert!(logs.contains("--is-bare-repository"), "{logs}");
        assert!(logs.contains("128"), "{logs}");
        assert!(logs.contains("must be run in a work tree"), "{logs}");
    }

    /// Superseding an in-flight read is how this application refreshes, so a
    /// cancellation is routine rather than a fault. Logging one would bury the
    /// failures worth reading under the noise of ordinary typing.
    #[test]
    fn a_cancelled_git_command_is_not_logged_as_a_failure() {
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
        let logs = captured_logs(|| {
            let service = fake_service(vec![Err(cancelled)]);
            service
                .discover(Path::new("/repo"), &CancellationToken::new())
                .expect_err("a cancelled query is an error");
        });

        assert!(logs.is_empty(), "{logs}");
    }
}
