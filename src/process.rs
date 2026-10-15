//! Bounded synchronous process execution shared by UI services and daemon workers.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt,
    io::{self, Read, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
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
/// that repository's mutation lock for the whole call.
pub(crate) const CLEANUP_GRACE: Duration = Duration::from_millis(500);
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_STDOUT_LIMIT: usize = 16 * 1024 * 1024;
pub(crate) const DEFAULT_STDERR_LIMIT: usize = 4 * 1024 * 1024;
const FAILURE_EXCERPT_LIMIT: usize = 64 * 1024;
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
    // Borrowed only during execute; the journal service holds the owning file.
    pub(crate) inherited_lease: Option<i32>,
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
            inherited_lease: None,
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

    /// Let user-driven operations run until completion or explicit cancellation.
    pub fn without_timeout(mut self) -> Self {
        self.timeout = None;
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
            if let Some(lease) = self.inherited_lease {
                // SAFETY: fcntl is async-signal-safe; only the child clears CLOEXEC
                // on a duplicate. Descendants retain the flock across parent death.
                unsafe {
                    command.pre_exec(move || {
                        if libc::fcntl(lease, libc::F_DUPFD, 3) == -1 {
                            return Err(io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
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
        write!(formatter, "{}: {}", self.program.display(), self.message)?;
        if self.kind == ProcessFailureKind::Exit
            && let Some(detail) = self.output_excerpt()
        {
            write!(formatter, ": {detail}")?;
        }
        Ok(())
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

    /// Preserve hook diagnostics written to either stream. Status/progress lines
    /// are retained in the full output; the inline excerpt stays compact.
    pub fn output_excerpt(&self) -> Option<String> {
        let stderr = String::from_utf8_lossy(&self.stderr);
        let stdout = String::from_utf8_lossy(&self.stdout);
        let stream = if stderr.trim().is_empty() {
            &stdout
        } else {
            &stderr
        };
        let lines = stream
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(3)
            .collect::<Vec<_>>()
            .join("\n");
        if lines.is_empty() {
            None
        } else {
            Some(lines.chars().take(600).collect())
        }
    }

    /// All captured output for a disclosure, without losing multiline hooks.
    pub fn full_output(&self) -> String {
        let mut output = String::new();
        if !self.stdout.is_empty() {
            output.push_str("stdout:\n");
            output.push_str(&String::from_utf8_lossy(&self.stdout));
        }
        if !self.stderr.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("stderr:\n");
            output.push_str(&String::from_utf8_lossy(&self.stderr));
        }
        output
    }

    /// What to report about this failure, in whichever words actually describe
    /// it.
    ///
    /// [`ProcessFailureKind::Exit`] is the one kind whose own `message` says
    /// nothing beyond an exit code, a number standing in for the two
    /// sentences `gh` wrote about
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
                message: match (self.exit_code, self.signal) {
                    (Some(code), _) => format!("exit {code}"),
                    (_, Some(signal)) => format!("terminated by signal {signal}"),
                    _ => "process exited without an exit code".into(),
                },
                started: true,
            })
        }
    }
}

/// What woke a blocking process wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitEvent {
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
    wakers: Mutex<Vec<(u64, CancellationNotifier)>>,
    next_waker: AtomicU64,
}

/// The process runner needs a typed event; resource gates only need a wake.
/// Bounded signals coalesce repeated notifications and never block cancellation.
#[derive(Debug)]
enum CancellationNotifier {
    Process(mpsc::Sender<WaitEvent>),
    Signal(mpsc::SyncSender<()>),
}

impl CancellationNotifier {
    fn notify(&self) {
        match self {
            Self::Process(sender) => {
                let _ = sender.send(WaitEvent::Cancelled);
            }
            Self::Signal(sender) => {
                let _ = sender.try_send(());
            }
        }
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        // Publish the flag before waking anyone, so a woken waiter is
        // guaranteed to observe it.
        if self.0.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        for (_, waker) in lock_wakers(&self.0).iter() {
            waker.notify();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    /// Registers a waker for the duration of one blocking wait. The returned
    /// guard deregisters it, so a long-lived token does not accumulate senders.
    pub(crate) fn register(&self, waker: mpsc::Sender<WaitEvent>) -> CancellationWaker<'_> {
        self.register_notifier(CancellationNotifier::Process(waker))
    }

    /// Registers a bounded wake signal for a blocking resource gate. Register
    /// before checking cancellation, then recheck the flag after every wake.
    /// A full channel already contains a wake, so cancelling never blocks.
    pub(crate) fn register_signal(&self, waker: mpsc::SyncSender<()>) -> CancellationWaker<'_> {
        self.register_notifier(CancellationNotifier::Signal(waker))
    }

    fn register_notifier(&self, notifier: CancellationNotifier) -> CancellationWaker<'_> {
        let id = self.0.next_waker.fetch_add(1, Ordering::Relaxed);
        lock_wakers(&self.0).push((id, notifier));
        CancellationWaker { state: &self.0, id }
    }
}

pub(crate) struct CancellationWaker<'token> {
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
fn lock_wakers(state: &CancellationState) -> MutexGuard<'_, Vec<(u64, CancellationNotifier)>> {
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
/// and observes exit without reaping until group cleanup finishes; pipe readers
/// announce their own completion, and
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
        #[cfg(feature = "ui")]
        if crate::device::is_remote() {
            return Err(process_failure(
                spec,
                ProcessFailureKind::Spawn,
                "remote commands must run through the selected daemon",
                false,
            ));
        }
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

        // Capture the original process capability before any exit observation.
        // Reopening a pidfd later could identify a recycled replacement instead.
        let identity = child_identity(&child);

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let status = terminate_process_tree(child, identity);
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
                let status = terminate_process_tree(child, identity);
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
                let status = terminate_process_tree(child, identity);
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

        // Observe exit without releasing the leader's PID. The waiter only
        // reaps after every possible group signal and pipe cleanup is finished.
        let mut waiter = GroupWaiter::new(child, identity, Some(events.clone()));
        let exit_source = &waiter.exit_source;

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
                status = terminate_process_group(&waiter, status);
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
                        status = terminate_process_group(&waiter, None);
                        break;
                    }
                    Err(_) => {
                        wait_error = Some("process waiter reported no exit status".into());
                        terminal_kind = Some(ProcessFailureKind::Io);
                        status = terminate_process_group(&waiter, None);
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
            status = terminate_process_group(&waiter, status);
            terminal_kind = Some(ProcessFailureKind::OutputLimit);
        }
        // Reaping permission is released only after the final output-limit
        // cleanup above. An uninterruptible child still leaves one bounded
        // waiter thread until its eventual exit; no polling or unbounded join.
        if let Some(error) = waiter.finish(status.is_some()) {
            wait_error = Some(error);
            if terminal_kind.is_none() {
                terminal_kind = Some(ProcessFailureKind::Io);
            }
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
                stdout: if kind == ProcessFailureKind::OutputLimit {
                    failure_excerpt(stdout)
                } else {
                    stdout
                },
                stderr: if kind == ProcessFailureKind::OutputLimit {
                    failure_excerpt(stderr)
                } else {
                    stderr
                },
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

pub(crate) fn process_failure(
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
/// Callers must retain original process identity and keep the group leader
/// unreaped through every signal. Crate-internal so that
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

/// A sole-reaper waiter keeps the leader unreaped through every group signal.
/// Exit observation wakes the executor immediately; final reaping requires the
/// executor's permission after bounded pipe cleanup and output-limit checks.
struct GroupWaiter {
    group: u32,
    exit_source: mpsc::Receiver<io::Result<ExitStatus>>,
    release: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<io::Result<ExitStatus>>>,
    ownership: Arc<Mutex<bool>>,
    identity: Option<Arc<OwnedFd>>,
}

impl GroupWaiter {
    fn new(
        mut child: std::process::Child,
        identity: io::Result<OwnedFd>,
        events: Option<mpsc::Sender<WaitEvent>>,
    ) -> Self {
        let group = child.id();
        // Establish authority before the caller can signal. An already reaped
        // Child may retain a cached status but no longer owns its numeric PID.
        let (identity, initial) = match identity {
            Ok(identity) => {
                let identity = Arc::new(identity);
                let initial = observe_child_exit(&identity, group, true);
                (Some(identity), initial)
            }
            Err(error) => {
                // Capture failed before this fresh Child was handed off or
                // reaped. Its still-reserved leader authorizes startup-only
                // cleanup; no later lifecycle signal may use numeric fallback.
                signal_process_group(group, SIGKILL);
                (None, Err(error))
            }
        };
        let observed_identity = identity.clone();
        let ownership = Arc::new(Mutex::new(initial.is_ok()));
        let authority = Arc::clone(&ownership);
        let (exit, exit_source) = mpsc::channel();
        let (release, permission) = mpsc::channel();
        let worker = thread::spawn(move || {
            let status = initial.and_then(|status| match status {
                Some(status) => Ok(status),
                None => observe_child_exit(
                    observed_identity.as_ref().expect("pinned child identity"),
                    group,
                    false,
                )?
                .ok_or_else(|| {
                    io::Error::other("blocking child exit observation returned no status")
                }),
            });
            if status.is_err() {
                *authority
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = false;
            }
            // Queue status before wakeup. WNOWAIT still pins the leader here.
            let _ = exit.send(status);
            if let Some(events) = events {
                let _ = events.send(WaitEvent::Exited);
            }
            let _ = permission.recv();
            // Reaping and signalling share this lock, and permission only
            // arrives after the executor gives up all remaining group signals.
            let mut owned = authority
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            *owned = false;
            drop(child.stdin.take());
            match observed_identity.as_ref() {
                Some(identity) => child_exit_status(identity, group, libc::WEXITED)?
                    .ok_or_else(|| io::Error::other("final child reap returned no status")),
                // Only initial capture failure reaches this branch. The fresh
                // Child has never been observed/reaped elsewhere, and startup
                // cleanup already sent its sole signal before spawning us.
                None => child.wait(),
            }
        });
        Self {
            group,
            exit_source,
            release: Some(release),
            worker: Some(worker),
            ownership,
            identity,
        }
    }

    fn signal(&self, signal: i32) -> bool {
        if self.release.is_none() {
            return false;
        }
        let mut owned = self
            .ownership
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !*owned {
            return false;
        }
        // Check the original capability, never whichever process currently has
        // this number. The sole reaper cannot run while this lock is held.
        if self
            .identity
            .as_ref()
            .is_none_or(|identity| observe_child_exit(identity, self.group, true).is_err())
        {
            *owned = false;
            return false;
        }
        signal_process_group(self.group, signal);
        true
    }

    fn finish(&mut self, observed_exit: bool) -> Option<String> {
        drop(self.release.take());
        if observed_exit {
            match self.worker.take().expect("owned child waiter").join() {
                Ok(Ok(_)) => None,
                Ok(Err(error)) => Some(format!("final child reap failed: {error}")),
                Err(_) => Some("child waiter panicked".into()),
            }
        } else {
            None
        }
    }
}

impl Drop for GroupWaiter {
    fn drop(&mut self) {
        if self.release.is_some() {
            // Unwinding cannot release reaping authority ahead of a remaining
            // group kill. Ordinary completion disarms this through finish.
            self.signal(SIGKILL);
            drop(self.release.take());
        }
    }
}

fn child_identity(child: &std::process::Child) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open returns a new close-on-exec descriptor for our newly
    // spawned, unreaped child. The guard never recreates it after reaping.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) } as i32;
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }
}

fn observe_child_exit(
    identity: &OwnedFd,
    pid: u32,
    nonblocking: bool,
) -> io::Result<Option<ExitStatus>> {
    child_exit_status(
        identity,
        pid,
        libc::WEXITED | libc::WNOWAIT | if nonblocking { libc::WNOHANG } else { 0 },
    )
}

fn child_exit_status(identity: &OwnedFd, pid: u32, flags: i32) -> io::Result<Option<ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;
    let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: P_PIDFD identifies the captured original child, including
        // when its old numeric ID has been reused. All observations use WNOWAIT;
        // only the worker's final call reaps after group signals are disarmed.
        if unsafe {
            libc::waitid(
                libc::P_PIDFD,
                identity.as_raw_fd() as libc::id_t,
                &mut information,
                flags,
            )
        } == 0
        {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    let (observed, status) = unsafe { (information.si_pid(), information.si_status()) };
    if observed == 0 {
        return Ok(None);
    }
    if observed as u32 != pid {
        return Err(io::Error::other(
            "child exit observation returned another process",
        ));
    }
    let raw = match information.si_code {
        libc::CLD_EXITED => status << 8,
        libc::CLD_KILLED => status,
        libc::CLD_DUMPED => status | 0x80,
        _ => {
            return Err(io::Error::other(
                "child exit observation returned an unexpected status",
            ));
        }
    };
    Ok(Some(ExitStatus::from_raw(raw)))
}

/// Group signals finish before the owned waiter's final reap permission.
fn terminate_process_group(
    waiter: &GroupWaiter,
    observed: Option<ExitStatus>,
) -> Option<ExitStatus> {
    if !waiter.signal(SIGTERM) {
        return observed;
    }
    let status = observed.or_else(|| {
        waiter
            .exit_source
            .recv_timeout(TERMINATION_GRACE)
            .ok()
            .and_then(Result::ok)
    });
    waiter.signal(SIGKILL);
    status.or_else(|| {
        waiter
            .exit_source
            .recv_timeout(CLEANUP_GRACE)
            .ok()
            .and_then(Result::ok)
    })
}

/// Cold setup failures use the same pinned identity and bounded exit waiter.
fn terminate_process_tree(
    child: std::process::Child,
    identity: io::Result<OwnedFd>,
) -> Option<ExitStatus> {
    let mut waiter = GroupWaiter::new(child, identity, None);
    let status = terminate_process_group(&waiter, None);
    if waiter.finish(status.is_some()).is_some() {
        None
    } else {
        status
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn group_waiter_capture_failure_cleans_only_fresh_startup_child() {
        use std::os::unix::process::CommandExt;
        let child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let original = child_identity(&child).unwrap();
        let mut waiter =
            GroupWaiter::new(child, Err(io::Error::from_raw_os_error(libc::EMFILE)), None);
        assert_eq!(
            waiter
                .exit_source
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EMFILE)
        );
        assert!(!waiter.signal(SIGTERM));
        assert!(waiter.finish(true).is_none());
        assert_eq!(
            observe_child_exit(&original, waiter.group, true)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn group_waiter_retains_leader_and_cleans_owned_descendants() {
        use std::io::BufRead;
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        const MARKER: &str = "CAGENTS_EXECUTOR_GROUP_CHILD";
        const TEST: &str =
            "process::tests::group_waiter_retains_leader_and_cleans_owned_descendants";
        if std::env::var_os(MARKER).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
                .env(MARKER, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}; {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .contains("owned executor descendant cases completed")
            );
            return;
        }
        // Only this exact subprocess becomes a subreaper; parallel libtest
        // siblings retain their ordinary process-wide child ownership.
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
            0
        );
        struct Descendant {
            identity: OwnedFd,
            pid: libc::pid_t,
            reaped: bool,
        }
        impl Drop for Descendant {
            fn drop(&mut self) {
                if !self.reaped {
                    unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            self.identity.as_raw_fd(),
                            SIGKILL,
                            std::ptr::null::<libc::siginfo_t>(),
                            0,
                        );
                        libc::waitpid(self.pid, std::ptr::null_mut(), 0);
                    }
                }
            }
        }
        for lost in [false, true] {
            let script = r#"import os,signal,sys
r,w=os.pipe()
pid=os.fork()
if pid==0:
 os.close(r)
 if sys.argv[1]=='normal': signal.signal(signal.SIGTERM,signal.SIG_IGN)
 os.write(w,b'1'); os.close(w)
 while True: signal.pause()
os.close(w); os.read(r,1); os.close(r)
print(pid,flush=True)
sys.stdin.buffer.read(1)
sys.exit(7)
"#;
            let mut command = std::process::Command::new("/usr/bin/python3");
            command
                .args(["-c", script, if lost { "lost" } else { "normal" }])
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            let mut child = command.spawn().unwrap();
            let identity = child_identity(&child).unwrap();
            let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let pid: libc::pid_t = line.trim().parse().unwrap();
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
            assert!(raw >= 0);
            let mut descendant = Descendant {
                identity: unsafe { OwnedFd::from_raw_fd(raw) },
                pid,
                reaped: false,
            };
            child.stdin.take().unwrap().write_all(b"x").unwrap();
            if lost {
                assert_eq!(child.wait().unwrap().code(), Some(7));
            }
            let mut waiter = GroupWaiter::new(child, Ok(identity), None);
            let observed = waiter
                .exit_source
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            if lost {
                assert_eq!(observed.unwrap_err().raw_os_error(), Some(libc::ECHILD));
                assert!(terminate_process_group(&waiter, None).is_none());
                let mut poll = libc::pollfd {
                    fd: descendant.identity.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                assert_eq!(
                    unsafe { libc::poll(&mut poll, 1, 100) },
                    0,
                    "lost authority must send neither TERM nor KILL"
                );
                assert!(waiter.finish(true).is_some());
            } else {
                let observed = observed.unwrap();
                assert_eq!(observed.code(), Some(7));
                assert_eq!(
                    observe_child_exit(waiter.identity.as_ref().unwrap(), waiter.group, true)
                        .unwrap()
                        .unwrap()
                        .code(),
                    Some(7),
                    "exit event must retain the unreaped leader"
                );
                assert_eq!(
                    terminate_process_group(&waiter, Some(observed))
                        .unwrap()
                        .code(),
                    Some(7)
                );
                assert!(waiter.finish(true).is_none());
                assert_eq!(
                    observe_child_exit(waiter.identity.as_ref().unwrap(), waiter.group, true)
                        .unwrap_err()
                        .raw_os_error(),
                    Some(libc::ECHILD)
                );
                assert!(
                    !waiter.signal(SIGKILL),
                    "final reap disarms all later signals"
                );
                let mut poll = libc::pollfd {
                    fd: descendant.identity.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                assert_eq!(unsafe { libc::poll(&mut poll, 1, 1000) }, 1);
                let mut raw_status = 0;
                assert_eq!(unsafe { libc::waitpid(pid, &mut raw_status, 0) }, pid);
                descendant.reaped = true;
                assert_eq!(ExitStatus::from_raw(raw_status).signal(), Some(SIGKILL));
            }
        }
        println!("owned executor descendant cases completed");
    }

    #[test]
    #[ignore = "requires an owned user/PID namespace and namespace-local PID allocation control"]
    fn group_waiter_refuses_a_kernel_reused_original_pid() {
        use std::os::unix::process::CommandExt;
        const MARKER: &str = "CAGENTS_EXECUTOR_REUSE_CHILD";
        const NAMESPACE: &str = "CAGENTS_EXECUTOR_OUTER_NAMESPACE";
        const TEST: &str = "process::tests::group_waiter_refuses_a_kernel_reused_original_pid";
        if std::env::var_os(MARKER).is_none() {
            let output = std::process::Command::new("/usr/bin/unshare")
                .args([
                    "--user",
                    "--map-root-user",
                    "--pid",
                    "--fork",
                    "--mount-proc",
                ])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    TEST,
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(MARKER, "1")
                .env(NAMESPACE, fs::read_link("/proc/self/ns/pid").unwrap())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}; {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .contains("original executor pidfd rejected recycled group")
            );
            return;
        }
        assert_eq!(std::process::id(), 1);
        assert_ne!(
            fs::read_link("/proc/self/ns/pid").unwrap().as_os_str(),
            std::env::var_os(NAMESPACE).unwrap()
        );
        fs::write("/proc/sys/kernel/ns_last_pid", b"199").unwrap();
        let mut original = std::process::Command::new("/bin/true")
            .process_group(0)
            .spawn()
            .unwrap();
        assert_eq!(original.id(), 200);
        let identity = child_identity(&original).unwrap();
        assert!(original.wait().unwrap().success());
        fs::write("/proc/sys/kernel/ns_last_pid", b"199").unwrap();
        let mut victim = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        assert_eq!(victim.id(), 200);
        assert_eq!(unsafe { libc::getpgid(200) }, 200);
        let victim_identity = child_identity(&victim).unwrap();
        struct Cleanup(OwnedFd);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.0.as_raw_fd(),
                        SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
        }
        let cleanup = Cleanup(victim_identity);
        let mut waiter = GroupWaiter::new(original, Ok(identity), None);
        assert_eq!(
            waiter
                .exit_source
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ECHILD)
        );
        assert!(terminate_process_group(&waiter, None).is_none());
        assert!(waiter.finish(true).is_some());
        let mut poll = libc::pollfd {
            fd: cleanup.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 100) }, 0);
        assert!(victim.try_wait().unwrap().is_none());
        println!("original executor pidfd rejected recycled group PID/PGID=200");
        drop(cleanup);
        victim.wait().unwrap();
    }

    #[test]
    fn failure_display_preserves_actionable_lines_and_formats_exit_codes() {
        let output = ProcessOutput {
            exit_code: Some(1),
            signal: None,
            success: false,
            stdout: b"hook stdout\n".to_vec(),
            stderr: b"\n lint: unused variable\npre-commit: aborting\n".to_vec(),
        };
        let failure = output.require_success(Path::new("git")).unwrap_err();
        let display = failure.to_string();
        assert!(display.contains("exit 1"));
        assert!(display.contains("lint: unused variable\npre-commit: aborting"));
        assert!(!display.contains("Some("));
        assert!(!display.contains("None"));
        assert!(failure.full_output().contains("hook stdout"));
        let mut stdout_only = failure.clone();
        stdout_only.stderr.clear();
        assert!(stdout_only.to_string().contains("hook stdout"));
        stdout_only.exit_code = None;
        stdout_only.message = "terminated by signal 9".into();
        assert!(!stdout_only.to_string().contains("None"));
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
    /// mutation is holding that repository's mutation lock while it
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
            // The mutation lane has no implicit deadline; only cancellation
            // can release this blocked child and its descendants.
            .without_timeout();

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
    fn cancellation_signals_coalesce_and_deregister_without_blocking() {
        let cancellation = CancellationToken::new();
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.try_send(()).unwrap();
        let registration = cancellation.register_signal(sender);
        // A full bounded channel must not block cancel or require a receiver.
        cancellation.cancel();
        assert!(cancellation.is_cancelled());
        receiver.try_recv().unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(registration);
        assert!(lock_wakers(&cancellation.0).is_empty());
    }

    #[test]
    fn cancellation_signals_wake_and_tolerate_disconnected_receivers() {
        let cancellation = CancellationToken::new();
        let (sender, receiver) = mpsc::sync_channel(1);
        let registration = cancellation.register_signal(sender);
        let (dead_sender, dead_receiver) = mpsc::sync_channel(1);
        drop(dead_receiver);
        let dead_registration = cancellation.register_signal(dead_sender);
        cancellation.cancel();
        receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(registration);
        drop(dead_registration);
        assert!(lock_wakers(&cancellation.0).is_empty());
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
}
