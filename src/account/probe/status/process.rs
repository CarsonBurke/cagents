//! Original-child custody for the reserved status probe only.
//!
//! One worker is started before provider creation. It owns the original std
//! Child through the last group signal and final wait, independently of the
//! Tokio runtime and of any startup-result receiver.

use std::{
    fs::File,
    io,
    mem::ManuallyDrop,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    panic::{AssertUnwindSafe, catch_unwind},
    process::{Child, ExitStatus},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use tokio::{
    process::{ChildStdin, ChildStdout, Command},
    runtime::Handle,
    sync::oneshot,
};

use crate::{daemon::child_reaper, host::RecoveryStatusPermit};

const EXIT_GRACE: Duration = Duration::from_secs(2);
const CLEANUP_GRACE: Duration = Duration::from_secs(2);
const RETAINED: &str = "Account status cleanup could not be proved; its original process and account reservation were retained";

pub(super) struct Process {
    pub(super) stdin: Option<ChildStdin>,
    pub(super) stdout: Option<ChildStdout>,
    ticket: Ticket,
    finished: oneshot::Receiver<Result<ExitStatus, &'static str>>,
    result: Option<Result<ExitStatus, String>>,
}

impl Process {
    pub(super) async fn spawn(
        command: Command,
        permit: RecoveryStatusPermit,
    ) -> Result<Self, String> {
        let runtime = Handle::try_current()
            .map_err(|_| "Account status requires an active runtime".to_owned())?;
        let credit = child_reaper::reserve()
            .map_err(|_| "Account status process ownership is unavailable".to_owned())?;
        Self::spawn_owned(command, permit, runtime, credit, Hooks::default()).await
    }

    async fn spawn_owned(
        mut command: Command,
        permit: RecoveryStatusPermit,
        runtime: Handle,
        credit: child_reaper::Reservation,
        hooks: Hooks,
    ) -> Result<Self, String> {
        command.as_std_mut().process_group(0);
        // All custody storage, channels and the process-owned strong anchor
        // exist before the worker is started and before any provider exists.
        let shared = Arc::new(Shared {
            permit,
            control: Mutex::new(Request::Running),
            wake: Condvar::new(),
            custody: Mutex::new(Custody {
                child: None,
                pidfd: None,
                credit: Some(credit),
                fault: None,
                signalled: false,
            }),
            hooks,
        });
        shared.hooks.observe(&shared);
        let anchor = Anchor::new(shared.clone());
        let ticket = Ticket(shared.clone());
        let (started, incoming) = oneshot::channel();
        let (finished, completion) = oneshot::channel();
        let worker_shared = shared.clone();
        shared
            .hooks
            .at(Point::ThreadStart)
            .map_err(|_| "The account status cleanup worker could not start".to_owned())?;
        let _worker = thread::Builder::new()
            .name("account-status-owner".into())
            .spawn(move || {
                let _anchor = anchor;
                let mut reports = Reports {
                    started: Some(started),
                    finished: Some(finished),
                };
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    worker(&worker_shared, command, runtime, &mut reports)
                }));
                match outcome {
                    Ok(Ok(())) => {}
                    Ok(Err(reason)) => {
                        worker_shared.retain(reason);
                        reports.error(reason);
                    }
                    Err(_) => {
                        let owns_child = worker_shared
                            .custody
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .child
                            .is_some();
                        let reason = if owns_child {
                            "The account status owner panicked; original custody was retained"
                        } else {
                            "The account status owner panicked before provider creation"
                        };
                        worker_shared.retain(reason);
                        reports.error(reason);
                    }
                }
            })
            .map_err(|_| "The account status cleanup worker could not start".to_owned())?;
        match incoming.await {
            Ok(Ok(pipes)) => Ok(Self {
                stdin: pipes.stdin,
                stdout: pipes.stdout,
                ticket,
                finished: completion,
                result: None,
            }),
            Ok(Err(reason)) => Err(reason.to_owned()),
            Err(_) => Err(shared
                .reason()
                .unwrap_or("The account status owner stopped before startup")
                .to_owned()),
        }
    }

    /// Close protocol pipes, allow a short clean exit, then finish the owned
    /// process group and await its sole original-child wait. Repeated calls
    /// return the same result; an error never authorizes another numeric signal.
    pub(super) async fn finish(&mut self) -> Result<ExitStatus, String> {
        if let Some(result) = &self.result {
            return result.clone();
        }
        drop(self.stdin.take());
        drop(self.stdout.take());
        self.ticket.0.request(Request::Finish);
        let result = match (&mut self.finished).await {
            Ok(result) => result.map_err(str::to_owned),
            Err(_) => Err(self.ticket.0.reason().unwrap_or(RETAINED).to_owned()),
        };
        self.result = Some(result.clone());
        result
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        drop(self.stdin.take());
        drop(self.stdout.take());
        // Ticket's drop only requests cleanup; it never waits for a child or
        // joins a worker. The independently anchored worker retains custody.
    }
}

struct Pipes {
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
}

struct Reports {
    started: Option<oneshot::Sender<Result<Pipes, &'static str>>>,
    finished: Option<oneshot::Sender<Result<ExitStatus, &'static str>>>,
}
impl Reports {
    fn error(&mut self, reason: &'static str) {
        if let Some(started) = self.started.take() {
            let _ = started.send(Err(reason));
        }
        if let Some(finished) = self.finished.take() {
            let _ = finished.send(Err(reason));
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Request {
    Running,
    Finish,
    Cancel,
}

struct Ticket(Arc<Shared>);
impl Drop for Ticket {
    fn drop(&mut self) {
        self.0.request(Request::Cancel);
    }
}

struct Shared {
    permit: RecoveryStatusPermit,
    control: Mutex<Request>,
    wake: Condvar,
    custody: Mutex<Custody>,
    hooks: Hooks,
}

struct Custody {
    child: Option<Child>,
    pidfd: Option<File>,
    credit: Option<child_reaper::Reservation>,
    fault: Option<&'static str>,
    signalled: bool,
}

impl Shared {
    fn request(&self, request: Request) {
        let mut current = self.control.lock().unwrap_or_else(|p| p.into_inner());
        if request == Request::Cancel || *current == Request::Running {
            *current = request;
        }
        self.wake.notify_all();
    }

    fn cancelled(&self) -> bool {
        *self.control.lock().unwrap_or_else(|p| p.into_inner()) == Request::Cancel
    }

    fn retry_delay(&self, delay: Duration) -> bool {
        let current = self.control.lock().unwrap_or_else(|p| p.into_inner());
        let (current, _) = self
            .wake
            .wait_timeout_while(current, delay, |value| *value == Request::Running)
            .unwrap_or_else(|p| p.into_inner());
        *current != Request::Cancel
    }

    fn wait_request(&self) -> Request {
        let mut request = self.control.lock().unwrap_or_else(|p| p.into_inner());
        while *request == Request::Running {
            request = self.wake.wait(request).unwrap_or_else(|p| p.into_inner());
        }
        *request
    }

    fn reason(&self) -> Option<&'static str> {
        self.custody.lock().unwrap_or_else(|p| p.into_inner()).fault
    }

    fn retain(&self, reason: &'static str) {
        let mut custody = self.custody.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(child) = &custody.child {
            let pid = child.id();
            if custody.fault.is_none() {
                custody.fault = Some(reason);
                if let Some(credit) = &custody.credit {
                    credit.quarantine();
                }
                tracing::error!(pid, %reason, "Account status original child, permit and cleanup credit remain retained");
                crate::daemon::wake();
            }
        }
    }
}

/// A strong anchor is installed before child creation. An unresolved tuple
/// deliberately survives worker panic and every caller/receiver being dropped.
/// The existing reaper credit bounds these fault cells and fences daemon exit.
/// There is no automatic repair or cleanup-success claim for a retained cell.
struct Anchor(ManuallyDrop<Arc<Shared>>);
impl Anchor {
    fn new(shared: Arc<Shared>) -> Self {
        Self(ManuallyDrop::new(shared))
    }
}
impl Drop for Anchor {
    fn drop(&mut self) {
        if self
            .0
            .custody
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .child
            .is_some()
        {
            self.0.retain(RETAINED);
            self.0.hooks.record(Point::Retained);
            // Keep this one process-owned strong reference. Its original
            // Child, optional original pidfd, permit and credit stay together.
        } else {
            // SAFETY: this anchor owns exactly this Arc, and releases it once
            // only after no original child remains to require custody.
            unsafe { ManuallyDrop::drop(&mut self.0) };
        }
    }
}

fn worker(
    shared: &Arc<Shared>,
    mut command: Command,
    runtime: Handle,
    reports: &mut Reports,
) -> Result<(), &'static str> {
    let mut delay = Duration::from_millis(10);
    loop {
        shared
            .hooks
            .at(Point::BeforeSpawn)
            .map_err(|_| "Account status startup was refused")?;
        if shared.cancelled() {
            reports.error("Account status startup was cancelled before provider creation");
            return Ok(());
        }
        let lease = shared
            .permit
            .acquire(command.as_std())
            .map_err(|_| "The account status reservation changed before provider creation")?;
        shared.hooks.record(Point::LeaseAcquired);
        let spawned = if shared.hooks.executable_busy() {
            Err(io::Error::from_raw_os_error(libc::ETXTBSY))
        } else {
            command.as_std_mut().spawn()
        };
        drop(lease);
        match spawned {
            Ok(child) => {
                shared
                    .custody
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .child = Some(child);
                shared.hooks.record(Point::LeaseReleased);
                break;
            }
            Err(error)
                if error.kind() == io::ErrorKind::ExecutableFileBusy
                    && delay <= Duration::from_millis(160) =>
            {
                shared.hooks.record(Point::LeaseReleased);
                shared
                    .hooks
                    .at(Point::RetryDelay)
                    .map_err(|_| "Account status retry was refused")?;
                if !shared.retry_delay(delay) {
                    reports.error("Account status startup was cancelled before provider creation");
                    return Ok(());
                }
                delay *= 2;
            }
            Err(_) => {
                shared.hooks.record(Point::LeaseReleased);
                return Err("The account status process could not be started");
            }
        }
    }
    shared
        .hooks
        .at(Point::OriginalStored)
        .map_err(|_| RETAINED)?;
    shared.hooks.at(Point::PidfdCapture).map_err(
        |_| "The account status original pidfd could not be captured; custody was retained",
    )?;
    {
        let mut custody = shared.custody.lock().unwrap_or_else(|p| p.into_inner());
        let child = custody
            .child
            .as_ref()
            .expect("worker owns the original child");
        let pidfd = capture_pidfd(child).map_err(
            |_| "The account status original pidfd could not be captured; custody was retained",
        )?;
        custody.pidfd = Some(pidfd);
        shared.hooks.at(Point::PidfdValidation).map_err(
            |_| "The account status original pidfd could not be verified; custody was retained",
        )?;
        validate_pidfd(
            custody
                .child
                .as_ref()
                .expect("original child remains owned"),
            custody.pidfd.as_ref().expect("original pidfd was stored"),
        )
        .map_err(
            |_| "The account status original pidfd could not be verified; custody was retained",
        )?;
    }
    shared.hooks.at(Point::PidfdStored).map_err(|_| RETAINED)?;
    if shared.cancelled() {
        return cleanup(shared, Request::Cancel, reports);
    }
    let pipes = (|| {
        shared
            .hooks
            .at(Point::PipeConversion)
            .map_err(|_| "The account status pipes could not be registered")?;
        let (stdin, stdout) = {
            let mut custody = shared.custody.lock().unwrap_or_else(|p| p.into_inner());
            let child = custody
                .child
                .as_mut()
                .expect("worker owns the original child");
            (child.stdin.take(), child.stdout.take())
        };
        let _runtime = runtime.enter();
        Ok::<_, &'static str>(Pipes {
            stdin: stdin
                .map(ChildStdin::from_std)
                .transpose()
                .map_err(|_| "The account status input could not be registered")?,
            stdout: stdout
                .map(ChildStdout::from_std)
                .transpose()
                .map_err(|_| "The account status output could not be registered")?,
        })
    })();
    match pipes {
        Err(reason) => {
            if let Some(started) = reports.started.take() {
                let _ = started.send(Err(reason));
            }
            return cleanup(shared, Request::Cancel, reports);
        }
        Ok(pipes) => {
            if shared.hooks.at(Point::Handoff).is_err() {
                drop(pipes);
                if let Some(started) = reports.started.take() {
                    let _ = started.send(Err("The account status startup handoff failed"));
                }
                return cleanup(shared, Request::Cancel, reports);
            }
            if reports
                .started
                .take()
                .expect("one startup report")
                .send(Ok(pipes))
                .is_err()
            {
                return cleanup(shared, Request::Cancel, reports);
            }
        }
    }
    cleanup(shared, shared.wait_request(), reports)
}

fn cleanup(shared: &Shared, request: Request, reports: &mut Reports) -> Result<(), &'static str> {
    let mut custody = shared.custody.lock().unwrap_or_else(|p| p.into_inner());
    let child = custody
        .child
        .as_ref()
        .expect("worker owns the original child");
    let pid = child.id();
    let pidfd = custody.pidfd.as_ref().ok_or(RETAINED)?;
    validate_pidfd(child, pidfd).map_err(
        |_| "The account status original pidfd could not be verified; custody was retained",
    )?;
    if request == Request::Finish {
        let deadline = Instant::now() + EXIT_GRACE;
        while !observe_exit(pidfd, pid).map_err(
            |_| "The account status original exit could not be observed; custody was retained",
        )? {
            if shared.cancelled() || Instant::now() >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            poll_exit(pidfd, remaining.min(Duration::from_millis(20))).map_err(
                |_| "The account status original exit wait failed; custody was retained",
            )?;
        }
    }
    // Even an exited leader still anchors its process-group number. Prove
    // the original remains waitable, then signal descendants before any reap.
    observe_exit(pidfd, pid).map_err(
        |_| "The account status original exit could not be observed; custody was retained",
    )?;
    // SAFETY: this is the original still-unreaped child of this sole worker;
    // its validated original pidfd was observed immediately above.
    if unsafe { libc::killpg(pid as i32, libc::SIGKILL) } == -1
        && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
        return Err("The account status process group could not be stopped; custody was retained");
    }
    custody.signalled = true;
    shared.hooks.record(Point::GroupSignalled);
    let deadline = Instant::now() + shared.hooks.cleanup_grace();
    while !shared
        .hooks
        .cleanup_ready(custody.pidfd.as_ref().ok_or(RETAINED)?, pid)
        .map_err(
            |_| "The account status original exit could not be observed; custody was retained",
        )?
    {
        if Instant::now() >= deadline {
            return Err(
                "Account status cleanup exceeded its deadline; original custody was retained",
            );
        }
        poll_exit(
            custody.pidfd.as_ref().ok_or(RETAINED)?,
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(20)),
        )
        .map_err(|_| "The account status original exit wait failed; custody was retained")?;
    }
    shared
        .hooks
        .at(Point::OriginalWait)
        .map_err(|_| "The account status original wait failed; custody was retained")?;
    let status = loop {
        match custody
            .child
            .as_mut()
            .expect("original remains owned")
            .wait()
        {
            Ok(status) => break status,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("The account status original wait failed; custody was retained"),
        }
    };
    custody.child = None;
    drop(custody.pidfd.take());
    let credit = custody.credit.take();
    drop(custody);
    shared.hooks.record(Point::OriginalReaped);
    drop(credit);
    shared.hooks.record(Point::CreditReleased);
    if let Some(finished) = reports.finished.take() {
        let _ = finished.send(Ok(status));
    }
    Ok(())
}

fn capture_pidfd(child: &Child) -> io::Result<File> {
    // SAFETY: this is our newly created, unreaped original child. No later
    // cleanup path reopens a pidfd from a remembered numeric identifier.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) } as i32;
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pidfd_open transferred this new owned descriptor.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn validate_pidfd(child: &Child, pidfd: &File) -> io::Result<()> {
    let descriptor = pidfd.as_raw_fd();
    if std::fs::read_link(format!("/proc/self/fd/{descriptor}"))?
        != std::path::Path::new("anon_inode:[pidfd]")
    {
        return Err(io::Error::other("The original child pidfd is unavailable"));
    }
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{descriptor}"))?;
    let actual = info.lines().find_map(|line| {
        line.strip_prefix("Pid:")
            .and_then(|pid| pid.trim().parse::<u32>().ok())
    });
    if actual != Some(child.id()) {
        return Err(io::Error::other(
            "The original child pidfd changed identity",
        ));
    }
    Ok(())
}

fn observe_exit(pidfd: &File, pid: u32) -> io::Result<bool> {
    loop {
        // SAFETY: siginfo_t is valid zero-initialized output for waitid.
        let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: the original owned pidfd and output remain live. WNOWAIT
        // leaves this exact child unreaped through the final group signal.
        if unsafe {
            libc::waitid(
                libc::P_PIDFD,
                pidfd.as_raw_fd() as libc::id_t,
                &mut information,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } == 0
        {
            // SAFETY: successful waitid initialized the siginfo union.
            let observed = unsafe { information.si_pid() };
            if observed == 0 {
                return Ok(false);
            }
            if observed as u32 == pid
                && matches!(
                    information.si_code,
                    libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
                )
            {
                return Ok(true);
            }
            return Err(io::Error::other(
                "The original child exit identity is unavailable",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn poll_exit(pidfd: &File, timeout: Duration) -> io::Result<()> {
    let mut poll = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: this one correctly sized pollfd and its original owned fd live
    // through the call. No process is identified by a reusable numeric PID.
    let result = unsafe {
        libc::poll(
            &mut poll,
            1,
            timeout.as_millis().min(i32::MAX as u128) as i32,
        )
    };
    if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
        return Err(io::Error::last_os_error());
    }
    if poll.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
        return Err(io::Error::other(
            "The original child exit descriptor failed",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Point {
    ThreadStart,
    BeforeSpawn,
    LeaseAcquired,
    LeaseReleased,
    RetryDelay,
    OriginalStored,
    PidfdCapture,
    PidfdValidation,
    PidfdStored,
    PipeConversion,
    Handoff,
    OriginalWait,
    GroupSignalled,
    OriginalReaped,
    CreditReleased,
    Retained,
}

#[derive(Clone, Default)]
struct Hooks {
    #[cfg(test)]
    test: Option<Arc<tests::TestHooks>>,
}
impl Hooks {
    fn at(&self, point: Point) -> io::Result<()> {
        self.record(point);
        #[cfg(test)]
        if let Some(test) = &self.test {
            return test.at(point);
        }
        Ok(())
    }
    fn record(&self, _point: Point) {
        #[cfg(test)]
        if let Some(test) = &self.test {
            test.record(_point);
        }
    }
    fn observe(&self, _shared: &Arc<Shared>) {
        #[cfg(test)]
        if let Some(test) = &self.test {
            test.observe(_shared);
        }
    }
    fn executable_busy(&self) -> bool {
        #[cfg(test)]
        if let Some(test) = &self.test {
            return test.executable_busy();
        }
        false
    }
    fn cleanup_grace(&self) -> Duration {
        #[cfg(test)]
        if self.test.as_ref().is_some_and(|test| test.cleanup_deadline) {
            return Duration::ZERO;
        }
        CLEANUP_GRACE
    }
    fn cleanup_ready(&self, pidfd: &File, pid: u32) -> io::Result<bool> {
        #[cfg(test)]
        if self.test.as_ref().is_some_and(|test| test.cleanup_deadline) {
            return Ok(false);
        }
        observe_exit(pidfd, pid)
    }
}

#[cfg(test)]
#[path = "process/tests.rs"]
mod tests;
