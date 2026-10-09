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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
