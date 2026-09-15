//! Daemon-owned, non-PTY provider actor. Detaching never stops its transport.
use super::{
    Command, Event, Item, ItemContent, Snapshot,
    protocol::{Adapter, ProcessSpec, Step, TransportSpec},
};
use crate::agent::ActivityState;
use std::{
    collections::{BTreeMap, VecDeque},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command as Process, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
mod store;
mod websocket;
use store::Store;

const FRAME_LIMIT: usize = 32 * 1024 * 1024;
const STATE_LIMIT: usize = 1024 * 1024;
const QUEUE_LIMIT: usize = 64 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const PRESENT_INTERVAL: Duration = Duration::from_millis(50);
/// Frames parsed per poll iteration: bounded so a provider flood cannot starve
/// the stdin writer or queued UI commands.
const FRAMES_PER_POLL: usize = 256;
/// Queued stdin bytes above which new commands wait for the provider to read.
const QUEUE_BACKPRESSURE: usize = QUEUE_LIMIT / 2;
type CommandRequest = (Command, crossbeam_channel::Sender<Result<(), String>>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Update {
    Presentation,
    Observed {
        generation: u64,
    },
    Activity {
        state: ActivityState,
        generation: u64,
    },
}

#[derive(Clone)]
pub struct Handle(Arc<Inner>);
struct Inner {
    commands: crossbeam_channel::Sender<CommandRequest>,
    wake: Mutex<UnixStream>,
    state: Arc<Mutex<Snapshot>>,
    confirmed_session: Arc<Mutex<Option<String>>>,
    visible: Arc<AtomicBool>,
    observe_next: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    completed: Arc<AtomicBool>,
    draining: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    store: Arc<Mutex<Store>>,
    notify: Arc<dyn Fn(Update) + Send + Sync>,
    execution_environment: crate::terminal_ipc::ChatExecutionEnvironment,
}
impl Handle {
    pub(crate) fn start(
        mut spec: ProcessSpec,
        adapter: Box<dyn Adapter>,
        initial: Step,
        store: Store,
        session_id: Option<String>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let child = match &spec.transport {
            TransportSpec::JsonLines => {
                let mut process = Process::new(&spec.program);
                process
                    .args(&spec.arguments)
                    .current_dir(&spec.working_directory)
                    .env_clear()
                    .envs(&spec.environment)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .process_group(0);
                Some(ChildGuard(
                    process
                        .spawn()
                        .map_err(|e| format!("Cannot start {}: {e}", spec.program))?,
                    false,
                ))
            }
            TransportSpec::UnixWebSocket { .. } => None,
        };
        let (wake, reader) = UnixStream::pair().map_err(|e| e.to_string())?;
        nonblocking(&wake)?;
        nonblocking(&reader)?;
        let (commands, incoming) = crossbeam_channel::bounded(64);
        let state = Arc::new(Mutex::new(Snapshot {
            session_id,
            ..Snapshot::default()
        }));
        let visible = Arc::new(AtomicBool::new(false));
        let observe_next = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let draining = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let store = Arc::new(Mutex::new(store));
        let confirmed_session = Arc::new(Mutex::new(None));
        let notify: Arc<dyn Fn(Update) + Send + Sync> = Arc::new(notify);
        let inner = Arc::new(Inner {
            commands,
            wake: Mutex::new(wake),
            state: state.clone(),
            confirmed_session: confirmed_session.clone(),
            visible: visible.clone(),
            observe_next: observe_next.clone(),
            stop: stop.clone(),
            completed: completed.clone(),
            draining: draining.clone(),
            failure: failure.clone(),
            worker: Mutex::new(None),
            store: store.clone(),
            notify: notify.clone(),
            execution_environment: crate::terminal_ipc::ChatExecutionEnvironment(std::mem::take(
                &mut spec.environment,
            )),
        });
        let worker = thread::Builder::new()
            .name("cagents-chat".into())
            .spawn(move || {
                let mut actor = Actor {
                    child,
                    adapter,
                    state,
                    confirmed_session,
                    store,
                    visible,
                    observe_next,
                    stop,
                    incoming,
                    wake: reader,
                    notify,
                    output: Vec::new(),
                    pending: VecDeque::new(),
                    offset: 0,
                    queued: 0,
                    stderr: VecDeque::new(),
                    indices: BTreeMap::new(),
                    retained: 0,
                    dirty: false,
                    activity: ActivityState::Idle,
                    last_present: Instant::now(),
                    first_frame: false,
                    started: Instant::now(),
                    write_started: None,
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match spec
                    .transport
                {
                    TransportSpec::JsonLines => actor.run(initial),
                    TransportSpec::UnixWebSocket {
                        socket,
                        device,
                        inode,
                    } => actor.run_websocket(initial, &socket, device, inode),
                }))
                .unwrap_or_else(|_| {
                    Err("Native chat worker panicked; exact provider state is unconfirmed".into())
                });
                // The actor has stopped answering commands, yet teardown below can
                // take seconds. Reject commands now so a caller is told the provider
                // exited instead of waiting out an acknowledgement nobody will send.
                draining.store(true, Ordering::Release);
                if let Err(error) = result {
                    *failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error.clone());
                    let stderr =
                        String::from_utf8_lossy(&actor.stderr.make_contiguous()).into_owned();
                    let message = if stderr.is_empty() {
                        error
                    } else {
                        format!("{error}\nProvider stderr (last 16 KiB): {stderr}")
                    };
                    actor.apply(Event::Error {
                        message,
                        fatal: true,
                    });
                }
                let code = actor.child.as_mut().and_then(|child| {
                    let code = child.0.try_wait().ok().flatten().and_then(|s| s.code());
                    child.terminate();
                    code
                });
                actor.apply(Event::Exited { code });
                actor.publish_activity();
                completed.store(true, Ordering::Release);
                actor.observe_update();
                if actor.visible.load(Ordering::Acquire) {
                    (actor.notify)(Update::Presentation);
                }
            })
            .map_err(|e| e.to_string())?;
        *inner.worker.lock().unwrap_or_else(|p| p.into_inner()) = Some(worker);
        Ok(Self(inner))
    }
    pub fn command(&self, command: Command) -> Result<(), String> {
        // `completed` is published only once teardown finished; `draining` the moment
        // the actor stopped consuming its command channel.
        let exiting = self.is_completed() || self.0.draining.load(Ordering::Acquire);
        if exiting && matches!(command, Command::LoadEarlier | Command::Refresh) {
            let cached = page_command(&self.0.store, &command)?;
            if command == Command::LoadEarlier && !cached {
                return Err("Earlier cached history is exhausted; the exited provider must be explicitly resumed to load more".into());
            }
            self.0
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .generation += 1;
            if self.0.visible.load(Ordering::Acquire) {
                (self.0.notify)(Update::Presentation);
            }
            return Ok(());
        }
        if exiting {
            return Err("Provider process exited; explicitly recover the exact session".into());
        }
        if self.0.stop.load(Ordering::Acquire) {
            return Err("Chat is closing".into());
        }
        let (reply, response) = crossbeam_channel::bounded(1);
        self.0
            .commands
            .try_send((command, reply))
            .map_err(|e| format!("Chat command queue unavailable: {e}"))?;
        self.wake();
        response
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| {
                "Provider command acknowledgement timed out; do not retry blindly".to_string()
            })?
    }
    fn wake(&self) {
        let _ = self
            .0
            .wake
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .write(&[1]);
    }
    pub fn set_visible(&self, visible: bool) {
        self.0.visible.store(visible, Ordering::Release);
        self.wake();
    }
    /// Rearm provider observation and return its current semantic boundary.
    /// Lock only resident metadata; never hydrate or clone the transcript.
    pub(crate) fn observe_next_update(&self) -> u64 {
        let state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        self.0.observe_next.store(true, Ordering::Release);
        state.generation
    }
    pub fn snapshot(&self, after: Option<u64>) -> Option<Snapshot> {
        let mut store = self.0.store.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if after.is_some_and(|generation| generation >= state.generation) && !store.dirty {
            return None;
        }
        if store.dirty {
            match store.window() {
                Ok(window) => {
                    state.items = window.items;
                    state.has_earlier = window.has_earlier;
                    state.has_later = window.has_later;
                }
                Err(error) => {
                    state.error = Some(format!("Cannot page owned transcript cache: {error}"))
                }
            }
        }
        Some(state.clone())
    }
    pub fn session_id(&self) -> Option<String> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session_id
            .clone()
    }
    /// Identity received from the provider, not the speculative launch selector.
    pub(crate) fn confirmed_session_id(&self) -> Option<String> {
        self.0
            .confirmed_session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub(crate) fn execution_environment(&self) -> crate::terminal_ipc::ChatExecutionEnvironment {
        self.0.execution_environment.clone()
    }
    pub(crate) fn execution_environment_matches(
        &self,
        environment: &BTreeMap<String, String>,
    ) -> bool {
        &self.0.execution_environment.0 == environment
    }
    pub fn is_completed(&self) -> bool {
        self.0.completed.load(Ordering::Acquire)
    }
    /// The recorded failure, without rebuilding the transcript window. Restore polls
    /// this; it must never pay for a multi-megabyte snapshot.
    pub fn failure(&self) -> Option<String> {
        if let Some(error) = self
            .0
            .failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            return Some(error);
        }
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .error
            .clone()
    }
    pub fn owns_work(&self) -> bool {
        !self.is_completed()
            || self
                .0
                .failure
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_some()
    }
    #[cfg(test)]
    pub(crate) fn resource_lifetime(&self) -> impl Fn() -> bool + use<> {
        let inner = Arc::downgrade(&self.0);
        move || inner.upgrade().is_some()
    }
    pub fn close(&self) -> Result<(), String> {
        self.0.stop.store(true, Ordering::Release);
        self.wake();
        if let Some(worker) = self
            .0
            .worker
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            && worker.join().is_err()
        {
            *self.0.failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(
                "Native chat worker did not shut down cleanly; provider state is unconfirmed"
                    .into(),
            );
        }
        match self
            .0
            .failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Validate local launch and adapter initialization before stopping an existing CLI.
pub(crate) fn preflight(spec: &ProcessSpec, initial: &mut Step) -> Result<Store, String> {
    use std::os::unix::ffi::OsStrExt;
    if !spec.working_directory.is_dir() {
        return Err("Provider working directory does not exist".into());
    }
    if spec.program.is_empty()
        || spec.program.contains('\0')
        || spec.arguments.iter().any(|arg| arg.contains('\0'))
        || spec
            .environment
            .iter()
            .any(|(key, value)| key.is_empty() || key.contains(['\0', '=']) || value.contains('\0'))
    {
        return Err(
            "Provider launch contains invalid executable, argument, or environment data".into(),
        );
    }
    if matches!(spec.transport, TransportSpec::JsonLines) {
        let paths = if spec.program.contains('/') {
            vec![spec.working_directory.join(&spec.program)]
        } else {
            let path = spec
                .environment
                .get("PATH")
                .cloned()
                .unwrap_or_else(|| "/usr/bin:/bin".into());
            std::env::split_paths(&path)
                .map(|directory| spec.working_directory.join(directory).join(&spec.program))
                .collect()
        };
        if !paths.iter().any(|path| {
            path.is_file()
                && std::ffi::CString::new(path.as_os_str().as_bytes())
                    .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0)
        }) {
            return Err(format!(
                "Provider executable {} is not executable in the preserved launch environment",
                spec.program
            ));
        }
    }
    let mut queued = 0usize;
    for frame in &initial.writes {
        let mut count = ByteCounter(0);
        serde_json::to_writer(&mut count, frame).map_err(|error| error.to_string())?;
        if count.0 > FRAME_LIMIT {
            return Err("Provider initialization frame exceeds 32 MiB".into());
        }
        queued = queued.saturating_add(count.0 + 1);
    }
    if queued > QUEUE_LIMIT {
        return Err("Provider initialization exceeds stdin queue capacity".into());
    }
    let mut store = Store::new()?;
    let mut metadata = Vec::new();
    for event in initial.events.drain(..) {
        if let Event::Error {
            message,
            fatal: true,
        } = &event
        {
            return Err(message.clone());
        }
        metadata.extend(store.apply(event)?);
    }
    let mut count = ByteCounter(0);
    serde_json::to_writer(&mut count, &metadata).map_err(|error| error.to_string())?;
    if count.0 > STATE_LIMIT {
        return Err(
            "Provider initial request/model metadata exceeds IPC presentation capacity".into(),
        );
    }
    initial.events = metadata;
    Ok(store)
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self
            .wake
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .write(&[1]);
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }
}
struct ChildGuard(Child, bool);
impl ChildGuard {
    fn terminate(&mut self) {
        if self.1 {
            return;
        }
        self.1 = true;
        // The child was started in its own process group; never signal a caller's group.
        let pid = self.0.id() as i32;
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut exited = false;
        while Instant::now() < deadline {
            if self.0.try_wait().ok().flatten().is_some() {
                exited = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        // Reaping the group leader releases its PID, so an unconditional escalation
        // could SIGKILL an unrelated group that recycled the number. Escalate only
        // while the leader is unreaped, or while the group provably still has members.
        if !exited || unsafe { libc::kill(-pid, 0) } == 0 {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        let _ = self.0.wait();
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}
fn nonblocking(fd: &impl AsRawFd) -> Result<(), String> {
    let raw = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}
struct Actor {
    child: Option<ChildGuard>,
    adapter: Box<dyn Adapter>,
    state: Arc<Mutex<Snapshot>>,
    confirmed_session: Arc<Mutex<Option<String>>>,
    store: Arc<Mutex<Store>>,
    visible: Arc<AtomicBool>,
    observe_next: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    incoming: crossbeam_channel::Receiver<CommandRequest>,
    wake: UnixStream,
    notify: Arc<dyn Fn(Update) + Send + Sync>,
    output: Vec<u8>,
    pending: VecDeque<Vec<u8>>,
    offset: usize,
    queued: usize,
    stderr: VecDeque<u8>,
    indices: BTreeMap<String, usize>,
    retained: usize,
    dirty: bool,
    activity: ActivityState,
    last_present: Instant,
    first_frame: bool,
    started: Instant,
    write_started: Option<Instant>,
}
impl Actor {
    fn run(&mut self, initial: Step) -> Result<(), String> {
        let mut stdin = self
            .child
            .as_mut()
            .ok_or("Provider process unavailable")?
            .0
            .stdin
            .take()
            .ok_or("Provider stdin unavailable")?;
        let mut stdout = self
            .child
            .as_mut()
            .ok_or("Provider process unavailable")?
            .0
            .stdout
            .take()
            .ok_or("Provider stdout unavailable")?;
        let mut stderr = self
            .child
            .as_mut()
            .ok_or("Provider process unavailable")?
            .0
            .stderr
            .take()
            .ok_or("Provider stderr unavailable")?;
        nonblocking(&stdin)?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        self.step(initial)?;
        let mut stdout_eof = false;
        let mut stderr_eof = false;
        let mut closing = None;
        let mut partial_started = None;
        let mut interrupt_sent = false;
        let mut interrupt_error = String::new();
        loop {
            if self.stop.load(Ordering::Acquire) {
                if closing.is_none() {
                    closing = Some(Instant::now());
                }
                let busy = self.state.lock().unwrap_or_else(|p| p.into_inner()).busy;
                if busy && !interrupt_sent && self.pending.is_empty() {
                    match self.adapter.command(Command::Interrupt) {
                        Ok(step) => {
                            self.step(step)?;
                            interrupt_sent = true;
                        }
                        // A just-submitted turn may not have its provider ID yet.
                        // Continue draining until it can be interrupted or the deadline expires.
                        Err(error) => interrupt_error = error,
                    }
                }
                if self.pending.is_empty()
                    && !self.state.lock().unwrap_or_else(|p| p.into_inner()).busy
                {
                    return Ok(());
                }
                if closing.is_some_and(|at: Instant| at.elapsed() > IO_TIMEOUT) {
                    return Err(format!(
                        "Provider interruption was not confirmed within 30 seconds ({interrupt_error}); backend work may still be active. Inspect exact provider session before recovery"
                    ));
                }
            }
            self.drain_commands(closing.is_some())?;
            // Parse every frame that already arrived. Gating this on an empty stdin
            // queue froze streaming for a whole turn against any provider that reads
            // stdin only between turns, then killed the session by blaming its
            // framing; a stalled write has its own `write_started` deadline below.
            let mut frames_ready = false;
            let mut parsed = 0usize;
            loop {
                let Some(newline) = self.output.iter().position(|byte| *byte == b'\n') else {
                    break;
                };
                if newline == 0 {
                    self.output.drain(..1);
                    continue;
                }
                if parsed == FRAMES_PER_POLL {
                    frames_ready = true;
                    break;
                }
                let frame = serde_json::from_slice(&self.output[..newline]).map_err(|error| {
                    format!("Unsupported provider stdout (expected JSON-lines): {error}")
                })?;
                self.output.drain(..=newline);
                partial_started = None;
                parsed += 1;
                self.first_frame = true;
                let step = self.adapter.receive(frame)?;
                self.provider_step(step)?;
            }
            if self.output.len() > FRAME_LIMIT {
                return Err("Provider stdout frame exceeds 32 MiB".into());
            }
            // Only a genuinely incomplete line is on the framing deadline.
            if frames_ready || self.output.is_empty() {
                partial_started = None;
            } else {
                let since = partial_started.get_or_insert_with(Instant::now);
                if since.elapsed() > IO_TIMEOUT {
                    return Err(
                        "Provider JSON frame remained incomplete or blocked for 30 seconds".into(),
                    );
                }
            }
            if stdout_eof && self.pending.is_empty() {
                if !self.output.is_empty() {
                    return Err("Provider ended with an incomplete JSON line".into());
                }
                if let Some(status) = self
                    .child
                    .as_mut()
                    .ok_or("Provider process unavailable")?
                    .0
                    .try_wait()
                    .map_err(|error| error.to_string())?
                {
                    if status.success() {
                        return Ok(());
                    }
                    return Err(format!("Provider exited with {status}"));
                }
                return Err("Provider closed its protocol stream; process terminated to avoid an unobservable writer".into());
            }
            if !self.first_frame && self.started.elapsed() > IO_TIMEOUT {
                return Err(
                    "Provider initialization timed out without a JSON protocol frame".into(),
                );
            }
            if self
                .write_started
                .is_some_and(|at| at.elapsed() > IO_TIMEOUT)
            {
                return Err("Provider stdin stalled for 30 seconds".into());
            }
            if self.dirty
                && self.visible.load(Ordering::Acquire)
                && self.last_present.elapsed() >= PRESENT_INTERVAL
            {
                self.dirty = false;
                self.last_present = Instant::now();
                (self.notify)(Update::Presentation);
            }
            let mut fds = [
                libc::pollfd {
                    fd: self.wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if stdout_eof || self.output.len() > FRAME_LIMIT {
                        -1
                    } else {
                        stdout.as_raw_fd()
                    },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if stderr_eof { -1 } else { stderr.as_raw_fd() },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if self.pending.is_empty() {
                        -1
                    } else {
                        stdin.as_raw_fd()
                    },
                    events: libc::POLLOUT,
                    revents: 0,
                },
            ];
            let timeout = if frames_ready
                || (self.queued <= QUEUE_BACKPRESSURE && !self.incoming.is_empty())
            {
                0
            } else if self.dirty && self.visible.load(Ordering::Acquire) {
                50
            } else if !self.first_frame
                || !self.output.is_empty()
                || self.write_started.is_some()
                || closing.is_some()
            {
                250
            } else {
                -1
            };
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) } < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error.to_string());
                }
            }
            let mut buffer = [0u8; 8192];
            if fds[0].revents != 0 {
                while self.wake.read(&mut buffer).is_ok_and(|n| n != 0) {}
            }
            if fds[3].revents != 0 {
                if let Some(line) = self.pending.front() {
                    match stdin.write(&line[self.offset..]) {
                        Ok(0) => return Err("Provider closed stdin".into()),
                        Ok(n) => {
                            self.offset += n;
                            self.queued -= n;
                            if self.offset == line.len() {
                                self.pending.pop_front();
                                self.offset = 0;
                                self.write_started = if self.pending.is_empty() {
                                    None
                                } else {
                                    Some(Instant::now())
                                };
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => return Err(format!("Provider stdin: {e}")),
                    }
                }
            }
            if fds[1].revents != 0 {
                let available = buffer.len().min(FRAME_LIMIT + 1 - self.output.len());
                match stdout.read(&mut buffer[..available]) {
                    Ok(0) => stdout_eof = true,
                    Ok(n) => {
                        reserve_frame(&mut self.output, n)?;
                        self.output.extend_from_slice(&buffer[..n]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(format!("Provider stdout: {e}")),
                }
            }
            if fds[2].revents != 0 {
                match stderr.read(&mut buffer) {
                    Ok(0) => stderr_eof = true,
                    Ok(n) => {
                        for byte in &buffer[..n] {
                            if self.stderr.len() == 16384 {
                                self.stderr.pop_front();
                            }
                            self.stderr.push_back(*byte);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(format!("Provider stderr: {e}")),
                }
            }
        }
    }
    fn drain_commands(&mut self, closing: bool) -> Result<(), String> {
        for _ in 0..64 {
            // Backpressure applies to queued bytes, not to the mere existence of a
            // queued write: a provider that reads stdin only between turns keeps
            // `pending` non-empty for the whole turn, and refusing to drain there left
            // UI commands unacknowledged until the caller's 10 second timeout.
            if self.queued > QUEUE_BACKPRESSURE {
                break;
            }
            let Ok((command, reply)) = self.incoming.try_recv() else {
                break;
            };
            if closing {
                let _ = reply.send(Err("Chat is closing".into()));
                continue;
            }
            if matches!(command, Command::LoadEarlier | Command::Refresh) {
                match page_command(&self.store, &command) {
                    Ok(cached) => {
                        self.touch();
                        if cached {
                            let _ = reply.send(Ok(()));
                            continue;
                        }
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                }
            }
            let starts_turn = matches!(&command, Command::Prompt { .. });
            match self.adapter.command(command) {
                Ok(step) => {
                    let result = self.step(step);
                    let _ = reply.send(result.clone());
                    result?;
                    if starts_turn {
                        self.apply(Event::Status {
                            busy: true,
                            detail: "Waiting for provider turn".into(),
                        });
                        self.publish_activity();
                    }
                }
                Err(message) => {
                    self.apply(Event::Error {
                        message: message.clone(),
                        fatal: false,
                    });
                    let _ = reply.send(Err(message));
                }
            }
        }
        Ok(())
    }
    fn observe_update(&self) {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let generation = state.generation;
        let observed = self.observe_next.swap(false, Ordering::AcqRel);
        // Host acknowledgement takes host -> state. Never notify under state.
        drop(state);
        if observed {
            (self.notify)(Update::Observed { generation });
        }
    }
    fn provider_step(&mut self, step: Step) -> Result<(), String> {
        let observed = !step.events.is_empty();
        let identity = step.events.iter().rev().find_map(|event| match event {
            Event::Session { id, .. } => Some(Some(id.clone())),
            Event::Reset { .. } => Some(None),
            _ => None,
        });
        self.step(step)?;
        if let Some(identity) = identity {
            *self
                .confirmed_session
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = identity;
        }
        if observed {
            self.observe_update();
        }
        Ok(())
    }
    fn step(&mut self, step: Step) -> Result<(), String> {
        for value in step.writes {
            let mut frame = BoundedFrame(Vec::new());
            serde_json::to_writer(&mut frame, &value)
                .map_err(|error| format!("Provider JSON request: {error}"))?;
            let mut bytes = frame.0;
            if self.child.is_some() {
                reserve_frame(&mut bytes, 1)?;
                bytes.push(b'\n');
            }
            if self.queued + bytes.len() > QUEUE_LIMIT {
                return Err("Provider stdin queue exceeds 64 MiB; no frames discarded".into());
            }
            self.queued += bytes.len();
            self.pending.push_back(bytes);
            if self.write_started.is_none() {
                self.write_started = Some(Instant::now());
            }
        }
        for event in step.events {
            let fatal = match &event {
                Event::Error {
                    message,
                    fatal: true,
                } => Some(message.clone()),
                _ => None,
            };
            let events = self
                .store
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .apply(event)?;
            if events.is_empty() {
                self.touch();
            } else {
                for event in events {
                    self.apply(event);
                }
            }
            if self.retained > STATE_LIMIT {
                return Err(
                    "Provider request/model metadata exceeds native IPC presentation capacity"
                        .into(),
                );
            }
            if let Some(message) = fatal {
                return Err(message);
            }
        }
        self.publish_activity();
        Ok(())
    }
    fn apply(&mut self, event: Event) {
        if matches!(&event, Event::Reset { .. }) {
            *self
                .confirmed_session
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = None;
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        apply_event(&mut state, &mut self.indices, event);
        // Only metadata is resident independently of the disk-backed window.
        let items = std::mem::take(&mut state.items);
        let mut counter = ByteCounter(0);
        let _ = serde_json::to_writer(&mut counter, &*state);
        state.items = items;
        self.retained = counter.0;
        state.generation = state.generation.saturating_add(1);
        self.dirty = true;
    }
    fn publish_activity(&mut self) {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let activity = state.activity();
        let generation = state.generation;
        drop(state);
        if activity != self.activity {
            self.activity = activity;
            (self.notify)(Update::Activity {
                state: activity,
                generation,
            });
        }
    }
    fn touch(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation += 1;
        self.dirty = true;
    }
}
fn page_command(store: &Mutex<Store>, command: &Command) -> Result<bool, String> {
    let mut store = store.lock().unwrap_or_else(|p| p.into_inner());
    match command {
        Command::LoadEarlier => store.earlier(),
        Command::Refresh => {
            store.latest();
            Ok(false)
        }
        _ => Ok(false),
    }
}
fn reserve_frame(bytes: &mut Vec<u8>, additional: usize) -> Result<(), String> {
    let required = bytes
        .len()
        .checked_add(additional)
        .ok_or("Provider frame length overflow")?;
    if required > FRAME_LIMIT + 1 {
        return Err("Provider frame exceeds 32 MiB".into());
    }
    if required > bytes.capacity() {
        let capacity = required.next_power_of_two().min(FRAME_LIMIT + 1);
        bytes
            .try_reserve_exact(capacity - bytes.len())
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}
struct BoundedFrame(Vec<u8>);
impl Write for BoundedFrame {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > FRAME_LIMIT.saturating_sub(self.0.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Provider request exceeds 32 MiB",
            ));
        }
        reserve_frame(&mut self.0, bytes.len()).map_err(std::io::Error::other)?;
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct ByteCounter(usize);
impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn apply_event(state: &mut Snapshot, indices: &mut BTreeMap<String, usize>, event: Event) {
    match event {
        Event::ItemsRemoved { ids } => {
            let removed: std::collections::BTreeSet<_> = ids.into_iter().collect();
            state.items.retain(|item| !removed.contains(&item.id));
            indices.clear();
            indices.extend(
                state
                    .items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| (item.id.clone(), index)),
            );
        }
        Event::Reset { reason } => {
            state.items.clear();
            indices.clear();
            state.requests.clear();
            state.usage = Default::default();
            state.session_id = None;
            state.title = None;
            state.has_earlier = false;
            state.detail = reason;
            state.busy = false;
            state.waiting = false;
            state.error = None;
        }
        Event::Session { id, title } => {
            state.session_id = Some(id);
            state.title = title;
        }
        Event::Capabilities(value) => state.capabilities = value,
        Event::Models {
            models,
            selected,
            reasoning,
        } => {
            if !models.is_empty() {
                state.models = models;
            }
            if selected.is_some() {
                state.selected_model = selected;
                state.reasoning = reasoning;
            } else if reasoning.is_some() {
                state.reasoning = reasoning;
            }
        }
        Event::Commands(value) => state.commands = value,
        Event::Item(mut item) => {
            if let Some(&index) = indices.get(&item.id) {
                if let (
                    ItemContent::Tool { name, input, .. },
                    ItemContent::Tool {
                        name: old_name,
                        input: old_input,
                        ..
                    },
                ) = (&mut item.content, &state.items[index].content)
                {
                    if name.is_empty() {
                        name.clone_from(old_name);
                    }
                    if input.is_empty() {
                        input.clone_from(old_input);
                    }
                }
                state.items[index] = item;
            } else {
                indices.insert(item.id.clone(), state.items.len());
                state.items.push(item);
            }
        }
        Event::History { items, has_earlier } => {
            let mut older = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            for item in items {
                if !indices.contains_key(&item.id) && seen.insert(item.id.clone()) {
                    older.push(item);
                }
            }
            older.append(&mut state.items);
            state.items = older;
            indices.clear();
            indices.extend(
                state
                    .items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| (item.id.clone(), index)),
            );
            state.has_earlier = has_earlier;
        }
        Event::TextDelta {
            id,
            role,
            thinking,
            delta,
        } => {
            if let Some(&index) = indices.get(&id) {
                if let ItemContent::Text { text, .. } = &mut state.items[index].content {
                    text.push_str(&delta);
                } else {
                    state.error = Some(format!(
                        "Provider reused non-text item ID {id} for a text delta"
                    ));
                }
            } else {
                indices.insert(id.clone(), state.items.len());
                state.items.push(Item {
                    id,
                    role,
                    content: ItemContent::Text {
                        text: delta,
                        thinking,
                    },
                });
            }
        }
        Event::Request(request) => {
            if let Some(old) = state.requests.iter_mut().find(|r| r.id == request.id) {
                *old = request;
            } else {
                state.requests.push(request);
            }
        }
        Event::RequestResolved { id } => state.requests.retain(|request| request.id != id),
        Event::Waiting(waiting) => state.waiting = waiting,
        Event::Status { busy, detail } => {
            state.busy = busy;
            state.detail = detail;
        }
        Event::Usage(value) => {
            if value.input_tokens != 0 {
                state.usage.input_tokens = value.input_tokens;
            }
            if value.output_tokens != 0 {
                state.usage.output_tokens = value.output_tokens;
            }
            if value.context_tokens != 0 {
                state.usage.context_tokens = value.context_tokens;
            }
            if value.context_limit != 0 {
                state.usage.context_limit = value.context_limit;
            }
            if value.cost_microusd.is_some() {
                state.usage.cost_microusd = value.cost_microusd;
            }
        }
        Event::Composer { text } => state.composer = Some(text),
        Event::OpenUrl { url, instructions } => state.open_url = Some((url, instructions)),
        Event::Error { message, fatal } => {
            state.error = Some(message);
            if fatal {
                state.busy = false;
            }
        }
        Event::Exited { .. } => {
            state.exited = true;
            state.waiting = false;
            state.busy = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Role;
    use super::*;
    use serde_json::{Value, json};

    fn text(id: &str, value: &str) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Text {
                text: value.into(),
                thinking: false,
            },
        }
    }
    #[test]
    fn outstanding_decisions_remain_waiting_after_streaming_stops() {
        let mut state = Snapshot::default();
        let mut indices = BTreeMap::new();
        apply_event(
            &mut state,
            &mut indices,
            Event::Status {
                busy: true,
                detail: String::new(),
            },
        );
        assert_eq!(state.activity(), ActivityState::Working);
        apply_event(
            &mut state,
            &mut indices,
            Event::Request(super::super::Request {
                id: "approval".into(),
                title: String::new(),
                description: String::new(),
                fields: Vec::new(),
                actions: Vec::new(),
            }),
        );
        assert_eq!(state.activity(), ActivityState::Waiting);
        apply_event(
            &mut state,
            &mut indices,
            Event::Status {
                busy: false,
                detail: String::new(),
            },
        );
        assert_eq!(state.activity(), ActivityState::Waiting);
        apply_event(
            &mut state,
            &mut indices,
            Event::RequestResolved {
                id: "approval".into(),
            },
        );
        assert_eq!(state.activity(), ActivityState::Idle);
        apply_event(&mut state, &mut indices, Event::Waiting(true));
        assert_eq!(state.activity(), ActivityState::Waiting);
        apply_event(&mut state, &mut indices, Event::Exited { code: None });
        assert_eq!(state.activity(), ActivityState::Unknown);
    }
    #[test]
    fn older_history_cannot_replace_a_live_stream_and_reset_invalidates_identity() {
        let mut state = Snapshot::default();
        let mut indices = BTreeMap::new();
        apply_event(
            &mut state,
            &mut indices,
            Event::Session {
                id: "exact-old".into(),
                title: None,
            },
        );
        apply_event(
            &mut state,
            &mut indices,
            Event::TextDelta {
                id: "live".into(),
                role: Role::Assistant,
                thinking: false,
                delta: "live".into(),
            },
        );
        apply_event(
            &mut state,
            &mut indices,
            Event::History {
                items: vec![
                    text("older", "history"),
                    text("live", "stale"),
                    text("older", "duplicate"),
                ],
                has_earlier: true,
            },
        );
        apply_event(
            &mut state,
            &mut indices,
            Event::TextDelta {
                id: "live".into(),
                role: Role::Assistant,
                thinking: false,
                delta: " update".into(),
            },
        );
        assert_eq!(
            state.items,
            vec![text("older", "history"), text("live", "live update")]
        );
        apply_event(
            &mut state,
            &mut indices,
            Event::Reset {
                reason: "upstream clear".into(),
            },
        );
        assert!(state.session_id.is_none());
        assert!(state.items.is_empty());
        apply_event(
            &mut state,
            &mut indices,
            Event::Item(text("live", "new conversation")),
        );
        assert_eq!(state.items, vec![text("live", "new conversation")]);
    }

    #[test]
    fn confirmed_default_reasoning_clears_override_but_catalog_refresh_does_not() {
        let mut state = Snapshot::default();
        let mut indices = BTreeMap::new();
        apply_event(
            &mut state,
            &mut indices,
            Event::Models {
                models: Vec::new(),
                selected: Some("model".into()),
                reasoning: Some("high".into()),
            },
        );
        apply_event(
            &mut state,
            &mut indices,
            Event::Models {
                models: Vec::new(),
                selected: None,
                reasoning: None,
            },
        );
        assert_eq!(state.reasoning.as_deref(), Some("high"));
        apply_event(
            &mut state,
            &mut indices,
            Event::Models {
                models: Vec::new(),
                selected: Some("model".into()),
                reasoning: None,
            },
        );
        assert_eq!(state.selected_model.as_deref(), Some("model"));
        assert!(state.reasoning.is_none());
    }

    struct Echo;
    impl Adapter for Echo {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                writes: vec![json!({"text": "hello"})],
                events: Vec::new(),
            })
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            if command != Command::Refresh {
                return Err("fixture only supports refresh".into());
            }
            Ok(Step {
                writes: vec![json!({"text": " world"})],
                events: Vec::new(),
            })
        }
        fn receive(&mut self, frame: Value) -> Result<Step, String> {
            Ok(Step {
                writes: Vec::new(),
                events: vec![Event::TextDelta {
                    id: "echo".into(),
                    role: Role::Assistant,
                    thinking: false,
                    delta: frame["text"].as_str().ok_or("missing echo text")?.into(),
                }],
            })
        }
    }
    fn until(handle: &Handle, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = handle.snapshot(None).unwrap();
            if predicate(&snapshot) {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "fixture timed out: {snapshot:?}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn native_child_receives_only_the_preserved_source_environment() {
        let environment = BTreeMap::from([
            ("HOME".into(), "/source-account".into()),
            ("ANTHROPIC_API_KEY".into(), "explicit-fixture-key".into()),
            ("LC_ALL".into(), "C.UTF-8".into()),
        ]);
        let spec = ProcessSpec {
            transport: TransportSpec::JsonLines,
            program: "/usr/bin/python3".into(),
            arguments: vec!["-c".into(), "import json,os,sys; sys.stdin.readline(); print(json.dumps({'text':json.dumps(dict(os.environ))}),flush=True); sys.stdin.read()".into()],
            working_directory: std::env::temp_dir(),
            environment: environment.clone(),
        };
        let mut adapter: Box<dyn Adapter> = Box::new(Echo);
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
        let snapshot = until(&handle, |snapshot| !snapshot.items.is_empty());
        let ItemContent::Text { text, .. } = &snapshot.items[0].content else {
            panic!("environment fixture did not return text");
        };
        assert_eq!(
            serde_json::from_str::<BTreeMap<String, String>>(text).unwrap(),
            environment
        );
        handle.close().unwrap();
    }
    #[test]
    fn real_json_lines_pipe_keeps_hidden_state_without_presentation() {
        let spec = ProcessSpec {
            transport: TransportSpec::JsonLines,
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let mut adapter: Box<dyn Adapter> = Box::new(Echo);
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let (notify, notified) = crossbeam_channel::bounded(8);
        let handle = Handle::start(spec, adapter, initial, store, None, move |update| {
            let _ = notify.try_send(update);
        })
        .unwrap();
        assert!(matches!(
            notified.recv_timeout(Duration::from_secs(2)).unwrap(),
            Update::Observed { .. }
        ));
        let generation = handle
            .0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation;
        handle.command(Command::Refresh).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while handle
            .0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation
            < generation + 2
        {
            assert!(Instant::now() < deadline, "hidden echo did not arrive");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(notified.try_recv().is_err(), "hidden deltas must coalesce");
        // Acknowledging an offscreen pane rearms activity, not presentation.
        handle.observe_next_update();
        handle.command(Command::Refresh).unwrap();
        assert!(matches!(
            notified.recv_timeout(Duration::from_secs(2)).unwrap(),
            Update::Observed { .. }
        ));
        while handle
            .0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation
            < generation + 4
        {
            assert!(
                Instant::now() < deadline,
                "acknowledged echo did not arrive"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            handle
                .0
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .items
                .is_empty()
        );
        assert!(
            handle
                .0
                .store
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .dirty
        );
        handle.set_visible(true);
        assert_eq!(
            notified.recv_timeout(Duration::from_secs(2)).unwrap(),
            Update::Presentation
        );
        assert_eq!(
            handle.snapshot(None).unwrap().items,
            vec![text("echo", "hello world world")]
        );
        handle.close().unwrap();
        assert!(handle.is_completed());
        assert_eq!(
            handle.snapshot(None).unwrap().items,
            vec![text("echo", "hello world world")]
        );
    }
    struct WebSocketFixture;
    impl Adapter for WebSocketFixture {
        fn initialize(&mut self) -> Result<Step, String> {
            Echo.initialize()
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            if command == Command::Interrupt {
                Ok(Step {
                    writes: vec![json!({"interrupt": true})],
                    events: Vec::new(),
                })
            } else {
                Echo.command(command)
            }
        }
        fn receive(&mut self, frame: Value) -> Result<Step, String> {
            let mut step = Echo.receive(frame.clone())?;
            step.events.push(Event::Status {
                busy: frame["busy"].as_bool().unwrap_or(false),
                detail: String::new(),
            });
            Ok(step)
        }
    }

    #[test]
    fn real_unix_websocket_preserves_hidden_history_pongs_and_interrupts_before_close() {
        use std::os::unix::{fs::MetadataExt, net::UnixListener};
        use tungstenite::Message;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("codex.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let spec = ProcessSpec {
            transport: TransportSpec::UnixWebSocket {
                socket: path,
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            // This executable does not exist: a socket connection must never spawn it.
            program: "/nonexistent/codex-websocket-fixture".into(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
        };
        let (pong, pong_received) = crossbeam_channel::bounded(1);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept_hdr(
                stream,
                |request: &tungstenite::handshake::server::Request, response| {
                    assert_eq!(request.uri().path(), "/");
                    assert_eq!(request.headers()["host"], "localhost");
                    Ok(response)
                },
            )
            .unwrap();
            let request = socket.read().unwrap();
            assert!(matches!(&request, Message::Text(_)));
            assert_eq!(
                serde_json::from_str::<Value>(request.to_text().unwrap()).unwrap(),
                json!({"text": "hello"})
            );
            socket
                .send(Message::Text(
                    json!({"text": "hello", "busy": true}).to_string().into(),
                ))
                .unwrap();
            socket
                .send(Message::Ping(b"exact-ping".to_vec().into()))
                .unwrap();
            assert_eq!(
                socket.read().unwrap(),
                Message::Pong(b"exact-ping".to_vec().into())
            );
            pong.send(()).unwrap();
            let request = socket.read().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(request.to_text().unwrap()).unwrap(),
                json!({"interrupt": true})
            );
            socket
                .send(Message::Text(
                    json!({"text": " stopped", "busy": false})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            assert!(matches!(socket.read().unwrap(), Message::Close(_)));
            // Match the real Codex server: it closes the connection without
            // echoing our close frame after confirming the turn is idle.
        });
        let mut adapter: Box<dyn Adapter> = Box::new(WebSocketFixture);
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let (notify, notified) = crossbeam_channel::bounded(8);
        let handle = Handle::start(spec, adapter, initial, store, None, move |update| {
            let _ = notify.try_send(update);
        })
        .unwrap();
        until(&handle, |snapshot| {
            snapshot.busy && snapshot.items == vec![text("echo", "hello")]
        });
        pong_received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            notified.recv_timeout(Duration::from_secs(2)).unwrap(),
            Update::Activity {
                state: ActivityState::Working,
                ..
            }
        ));
        assert!(matches!(
            notified.recv_timeout(Duration::from_secs(2)).unwrap(),
            Update::Observed { .. }
        ));
        assert!(notified.try_recv().is_err());
        let attached = handle.clone();
        attached.set_visible(false);
        drop(attached);
        assert!(!handle.is_completed());
        handle.close().unwrap();
        server.join().unwrap();
        let transitions: Vec<_> = notified
            .try_iter()
            .filter_map(|update| match update {
                Update::Activity { state, .. } => Some(state),
                Update::Presentation => panic!("hidden close must not request presentation"),
                Update::Observed { .. } => None,
            })
            .collect();
        assert_eq!(
            transitions,
            vec![ActivityState::Idle, ActivityState::Unknown]
        );
        let snapshot = handle.snapshot(None).unwrap();
        assert!(snapshot.exited);
        assert!(!snapshot.busy);
        assert_eq!(snapshot.items, vec![text("echo", "hello stopped")]);
    }

    #[test]
    fn unix_websocket_rejects_oversized_frame_before_receiving_its_payload() {
        use std::os::unix::{fs::MetadataExt, net::UnixListener};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("codex.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let spec = ProcessSpec {
            transport: TransportSpec::UnixWebSocket {
                socket: path,
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            program: "unused".into(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
        };
        let (release, released) = crossbeam_channel::bounded(1);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            socket.read().unwrap();
            // An unmasked final text frame with only its oversized length.
            socket.get_mut().write_all(&[0x81, 127]).unwrap();
            socket
                .get_mut()
                .write_all(&((FRAME_LIMIT + 1) as u64).to_be_bytes())
                .unwrap();
            // Keep the connection open: rejection cannot rely on EOF or timeout.
            released.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let mut adapter: Box<dyn Adapter> = Box::new(Echo);
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
        let snapshot = until(&handle, |snapshot| snapshot.exited);
        assert!(snapshot.error.is_some());
        assert!(snapshot.items.is_empty());
        assert!(handle.close().is_err());
        release.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn non_json_stdout_fails_visibly_instead_of_scraping_ansi() {
        let spec = ProcessSpec {
            transport: TransportSpec::JsonLines,
            program: "/usr/bin/printf".into(),
            arguments: vec!["not-json\\n".into()],
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let mut initial = Step::default();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, Box::new(Echo), initial, store, None, |_| {}).unwrap();
        let snapshot = until(&handle, |snapshot| snapshot.exited);
        assert!(
            snapshot
                .error
                .as_deref()
                .unwrap()
                .contains("Unsupported provider stdout")
        );
        assert!(handle.close().is_err());
    }
}
