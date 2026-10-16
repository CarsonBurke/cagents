//! Window-wide backend selection. Remote connections use the user's SSH trust
//! configuration, never a network listener or a copied credential store.
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, OnceLock, TryLockError},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub name: String,
    /// An SSH config Host alias, optionally user@host. Options are forbidden.
    pub host: String,
    /// Absolute path or shell command name of the matching remote CLI.
    #[serde(default = "default_executable")]
    pub executable: String,
}
fn default_executable() -> String {
    "cagents".into()
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Devices {
    #[serde(default)]
    devices: Vec<Device>,
}
/// A selection captures a validated profile rather than consulting mutable
/// configuration while sessions are live. The current CLI/window still owns
/// exactly one context; this does not introduce per-pane device selection.
struct ConnectionContext {
    device: Device,
    environment: OnceLock<
        std::collections::BTreeMap<
            crate::agent::Provider,
            std::collections::BTreeMap<String, String>,
        >,
    >,
    home: OnceLock<PathBuf>,
    shell: OnceLock<String>,
    shell_error: OnceLock<Option<String>>,
    bridge: Mutex<Option<Bridge>>,
    closed: std::sync::atomic::AtomicBool,
}
impl ConnectionContext {
    fn new(device: Device) -> Self {
        Self {
            device,
            environment: OnceLock::new(),
            home: OnceLock::new(),
            shell: OnceLock::new(),
            shell_error: OnceLock::new(),
            bridge: Mutex::new(None),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }
    fn close(&self) {
        // Serialize closure with bridge initialization. Existing consumers may
        // retain their streams, but no caller can reopen a closed context.
        let mut bridge = self.bridge.lock().unwrap_or_else(|p| p.into_inner());
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        drop(bridge.take());
    }
}
impl Drop for ConnectionContext {
    fn drop(&mut self) {
        self.close();
    }
}
static SELECTED: OnceLock<ConnectionContext> = OnceLock::new();
/// sshd admits `MaxSessions` (10 by default) streams per connection; a refused
/// one reconnects in full, so each master carries fewer and the next takes over.
const STREAMS_PER_MASTER: usize = 8;
/// Bridges the gaps between one process's connections, and bounds how long an
/// authenticated transport outlives a process that died without `Cleanup`.
const MASTER_PERSIST_SECONDS: u32 = 60;
/// Exceeds `ConnectTimeout`. A stream still connected then without a master
/// either cannot multiplex here or is authenticating unusually slowly, so later
/// streams stop waiting on one for `MULTIPLEX_RETRY`.
const MASTER_START_TIMEOUT: Duration = Duration::from_secs(20);
const MULTIPLEX_RETRY: Duration = Duration::from_secs(300);
/// `ssh -O exit` answers a healthy master in milliseconds. One that does not
/// answer would hold process exit for `ConnectTimeout` and then log in afresh.
const MASTER_EXIT_TIMEOUT: Duration = Duration::from_secs(2);

pub fn selected() -> Option<&'static Device> {
    SELECTED.get().map(|context| &context.device)
}
pub fn is_remote() -> bool {
    selected().is_some()
}
pub fn configuration_path() -> io::Result<PathBuf> {
    crate::instance::project_dirs()
        .map(|dirs| dirs.config_dir().join("devices.json"))
        .ok_or_else(|| io::Error::other("configuration directory unavailable"))
}
pub fn list() -> io::Result<Vec<Device>> {
    list_from_path(&configuration_path()?)
}

fn list_from_path(path: &Path) -> io::Result<Vec<Device>> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    // A configuration source must be a regular file. Open nonblocking before
    // checking the descriptor so a FIFO cannot wait for a writer; ordinary
    // symlinks to regular configurations retain their existing behavior.
    let bytes = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .and_then(|file| {
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "devices.json must be a regular file",
                ));
            }
            let mut bytes = Vec::new();
            file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
            Ok(bytes)
        });
    match bytes {
        Ok(bytes) => {
            if bytes.len() > 64 * 1024 {
                return Err(io::Error::other("devices.json exceeds 64 KiB"));
            }
            let config: Devices = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            if config.devices.len() > 64 {
                return Err(io::Error::other("configure at most 64 devices"));
            }
            for device in &config.devices {
                validate(device)?;
            }
            let mut names = std::collections::HashSet::new();
            if config
                .devices
                .iter()
                .any(|device| !names.insert(&device.name))
            {
                return Err(io::Error::other("duplicate device name"));
            }
            Ok(config.devices)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}
fn validate(device: &Device) -> io::Result<()> {
    if device.name == "local"
        || device.name.is_empty()
        || device.name.len() > 16
        || !device
            .name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(io::Error::other(
            "device names must be 1–16 ASCII letters, digits, hyphens or underscores; local is reserved",
        ));
    }
    if device.host.is_empty()
        || device.host.starts_with('-')
        || device
            .host
            .bytes()
            .any(|c| c.is_ascii_whitespace() || c.is_ascii_control())
        || device.executable.is_empty()
        || device.executable.bytes().any(|c| c.is_ascii_control())
    {
        return Err(io::Error::other("invalid SSH host or remote executable"));
    }
    Ok(())
}
pub fn select(name: Option<&str>) -> io::Result<()> {
    let Some(name) = name.filter(|name| *name != "local") else {
        return Ok(());
    };
    let configuration = configuration_path()?;
    let device = list()?
        .into_iter()
        .find(|device| device.name == name)
        .ok_or_else(|| {
            io::Error::other(format!(
                "unknown device {name:?}; configure {}",
                configuration.display()
            ))
        })?;
    SELECTED
        .set(ConnectionContext::new(device))
        .map_err(|_| io::Error::other("device is already selected"))
}
/// A namespace independent of both the local primary window and other devices.
pub fn window_slot(slot: Option<&str>) -> io::Result<Option<String>> {
    if let Some(slot) = slot {
        crate::instance::validate_window_slot(slot)?;
    }
    match selected() {
        None => {
            if slot.is_some_and(|slot| slot.starts_with("device-")) {
                return Err(io::Error::other(
                    "window names beginning device- are reserved for device windows",
                ));
            }
            Ok(slot.map(str::to_owned))
        }
        Some(device) => Ok(Some(scoped_window_slot(
            &device.name,
            slot.unwrap_or(crate::instance::PRIMARY_WINDOW),
        ))),
    }
}
pub(crate) fn scoped_window_slot(device: &str, slot: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{device}\0{slot}"));
    let mut suffix = String::new();
    for byte in &digest[..8] {
        use std::fmt::Write;
        let _ = write!(suffix, "{byte:02x}");
    }
    format!("device-{suffix}")
}
pub struct Cleanup;
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(context) = SELECTED.get() {
            context.close();
        }
    }
}

/// A shutdown wakes every descriptor waiter without an idle polling timer.
struct Cancellation {
    flag: std::sync::atomic::AtomicBool,
    reader: UnixStream,
    writer: UnixStream,
}
impl Cancellation {
    fn new() -> io::Result<Self> {
        let (reader, writer) = UnixStream::pair()?;
        Ok(Self {
            flag: std::sync::atomic::AtomicBool::new(false),
            reader,
            writer,
        })
    }
    fn is_cancelled(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::Acquire)
    }
    fn cancel(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::Release);
        let _ = self.writer.shutdown(std::net::Shutdown::Both);
    }
}
/// Workers retain their transport resources, never the context/Bridge owner.
/// This prevents a cycle from making cancellation depend on process exit.
struct Transport {
    device: Device,
    directory: PathBuf,
    pool: Arc<Mutex<Vec<Master>>>,
    paused: Mutex<Option<Instant>>,
    last_error: Mutex<Option<String>>,
    cancelled: Cancellation,
}
impl Drop for Transport {
    fn drop(&mut self) {
        // Only streams of this transport can own these private masters.
        // One overall deadline bounds cleanup independently of pool size.
        let deadline = Instant::now() + MASTER_EXIT_TIMEOUT;
        let masters = self.pool.lock().unwrap_or_else(|p| p.into_inner()).len();
        for control in (0..masters).filter_map(|index| control_path(&self.directory, index)) {
            if Instant::now() < deadline
                && Path::new(&control).exists()
                && let Ok(mut exit) = ssh(Some(&control))
                    .args(["-O", "exit", "--", &self.device.host])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
            {
                while matches!(exit.try_wait(), Ok(None)) {
                    if Instant::now() >= deadline {
                        let _ = exit.kill();
                        let _ = exit.wait();
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
            let _ = fs::remove_file(control);
        }
        let _ = fs::remove_file(self.directory.join("daemon.sock"));
        let _ = fs::remove_dir(&self.directory);
    }
}
struct Bridge {
    transport: Arc<Transport>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.transport.cancelled.cancel();
        if let Some(worker) = self.worker.take() {
            join_bounded(worker, MASTER_EXIT_TIMEOUT);
        }
    }
}
fn join_bounded(worker: thread::JoinHandle<()>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !worker.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    if worker.is_finished() {
        let _ = worker.join();
    }
    // An OS operation that remains stuck cannot block the UI indefinitely.
    // Its worker still owns cancelled resources until it completes; another
    // context never reuses that private directory or master pool.
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
/// Streams share an authenticated transport through OpenSSH multiplexing.
/// These options deliberately override the user's own ControlMaster settings:
/// a master cagents starts carries its keepalives, is counted against
/// `STREAMS_PER_MASTER`, and is one `Cleanup` may stop. Masters are private to
/// this process rather than shared between cagents processes, so closing the
/// window or finishing a command leaves no usable transport behind.
fn ssh(control: Option<&str>) -> Command {
    let mut command = Command::new("ssh");
    command.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=15",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
    ]);
    if let Some(control) = control {
        command.args([
            "-o",
            "ControlMaster=auto",
            "-o",
            &format!("ControlPath={control}"),
            "-o",
            &format!("ControlPersist={MASTER_PERSIST_SECONDS}"),
        ]);
    }
    command
}
fn bridge_command(device: &Device, control: Option<&str>) -> Command {
    let mut command = ssh(control);
    command.args(["--", &device.host]);
    command.arg(format!("{} daemon bridge", quote(&device.executable)));
    command
}
/// The control socket of one master, or `None` where SSH could not use it:
/// OpenSSH binds a 17-byte temporary suffix within the socket address limit and
/// reads the option as a configuration line with its own quoting and `%` tokens.
fn control_path(directory: &Path, master: usize) -> Option<String> {
    let path = directory.join(format!("ssh-{master}")).to_str()?.to_owned();
    // SAFETY: sockaddr_un is plain data for which all-zero bytes are valid.
    let limit = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
        .sun_path
        .len();
    (path.len() + 17 < limit
        && path
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c)))
    .then_some(path)
}
#[derive(Default)]
struct Master {
    streams: usize,
    /// Held while the first stream authenticates, so a burst shares its handshake.
    starting: Arc<Mutex<()>>,
}
/// One stream's place on the least-numbered master with room for it.
struct Slot {
    index: usize,
    pool: Arc<Mutex<Vec<Master>>>,
}
impl Slot {
    fn acquire(pool: &Arc<Mutex<Vec<Master>>>) -> (Self, Arc<Mutex<()>>) {
        let mut masters = pool.lock().unwrap_or_else(|poison| poison.into_inner());
        let index = masters
            .iter()
            .position(|master| master.streams < STREAMS_PER_MASTER)
            .unwrap_or_else(|| {
                masters.push(Master::default());
                masters.len() - 1
            });
        masters[index].streams += 1;
        (
            Self {
                index,
                pool: pool.clone(),
            },
            masters[index].starting.clone(),
        )
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.pool.lock().unwrap_or_else(|p| p.into_inner())[self.index].streams -= 1;
    }
}
/// Each process owns its private local proxy; reconnecting a client establishes
/// a fresh SSH stream. No exported socket is trusted by UID alone across hosts:
/// the control sockets beside the proxy stay on this host in the same private
/// directory, and grant only the SSH access their owner already has.
pub fn socket_path() -> io::Result<Option<PathBuf>> {
    SELECTED
        .get()
        .map(ConnectionContext::socket_path)
        .transpose()
}
impl ConnectionContext {
    fn socket_path(&self) -> io::Result<PathBuf> {
        let mut bridge = self.bridge.lock().unwrap_or_else(|p| p.into_inner());
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(io::Error::other("device connection is closed"));
        }
        if let Some(bridge) = bridge.as_ref() {
            return Ok(bridge.transport.directory.join("daemon.sock"));
        }
        let runtime =
            crate::instance::runtime_directory()?.join(crate::instance::RUNTIME_SUBDIRECTORY);
        crate::secure_file::prepare_private_directory(&runtime)?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let directory = runtime.join(format!("device-{}", &token[..12]));
        let cancelled = Cancellation::new()?;
        fs::create_dir(&directory)?;
        let transport = Arc::new(Transport {
            device: self.device.clone(),
            directory,
            pool: Arc::new(Mutex::new(Vec::new())),
            paused: Mutex::new(None),
            last_error: Mutex::new(None),
            cancelled,
        });
        fs::set_permissions(&transport.directory, fs::Permissions::from_mode(0o700))?;
        let path = transport.directory.join("daemon.sock");
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        let owned = transport.clone();
        let worker = thread::Builder::new()
            .name("cagents-device-bridge".into())
            .spawn(move || run_bridge(listener, owned))?;
        *bridge = Some(Bridge {
            transport,
            worker: Some(worker),
        });
        Ok(path)
    }
}
fn run_bridge(listener: UnixListener, transport: Arc<Transport>) {
    let mut workers = Vec::new();
    while !transport.cancelled.is_cancelled() {
        // Reap completed connections during normal operation, not just on close.
        let mut index = 0;
        while index < workers.len() {
            if thread::JoinHandle::is_finished(&workers[index]) {
                let _ = workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let owned = transport.clone();
                if let Ok(worker) = thread::Builder::new().name("cagents-device-connection".into())
                    .spawn(move || {
                        if let Err(error) = forward(&owned, stream)
                            && !owned.cancelled.is_cancelled()
                        {
                            tracing::warn!(device = owned.device.name, %error, "remote daemon connection ended; verify SSH access and matching cagents installation");
                        }
                    }) { workers.push(worker); }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if wait_transport_io(&listener, libc::POLLIN, &transport.cancelled, None).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    transport.cancelled.cancel();
    drop(listener);
    let deadline = Instant::now() + MASTER_EXIT_TIMEOUT;
    for worker in workers {
        join_bounded(worker, deadline.saturating_duration_since(Instant::now()));
    }
}
/// Spawn one stream. When its master is not listening this stream becomes it
/// (`ControlMaster=auto`, which also reconnects alone past a stale, unresponsive
/// or full master); later streams wait for that socket instead of racing it.
/// Killing a stream never stops the master, which OpenSSH detaches. Connecting
/// to the control socket and hanging up is how liveness is read without a
/// process per stream; the master discards such a client.
fn spawn_stream(
    transport: &Arc<Transport>,
    control: Option<&str>,
    starting: &Mutex<()>,
    stream: UnixStream,
) -> io::Result<Forwarding> {
    let device = &transport.device;
    let (_starting, waited) = if control.is_some() {
        match starting.try_lock() {
            Ok(guard) => (Some(guard), false),
            Err(TryLockError::Poisoned(poison)) => (Some(poison.into_inner()), false),
            Err(TryLockError::WouldBlock) => {
                (Some(lock_starting(starting, &transport.cancelled)?), true)
            }
        }
    } else {
        (None, false)
    };
    if transport.cancelled.is_cancelled() {
        return Err(io::Error::other("device connection is closed"));
    }
    let listening = || control.is_some_and(|control| UnixStream::connect(control).is_ok());
    let cold = control.is_some() && !listening();
    let child = bridge_command(device, control)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Forward immediately, even while the first stream coordinates startup.
    // SSH can successfully run the bridge without publishing a master socket;
    // waiting here before forwarding would expire the daemon's handshake.
    let mut forwarding = Forwarding::start(child, stream, Some(transport.clone()))?;
    // A stream that waited through a failed start must not make the rest queue
    // behind its own attempt; they fail or connect in parallel as before.
    let deadline = Instant::now() + MASTER_START_TIMEOUT;
    while !transport.cancelled.is_cancelled()
        && cold
        && !waited
        && !listening()
        && !forwarding
            .received_output
            .load(std::sync::atomic::Ordering::Acquire)
        && !forwarding
            .downstream
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished)
        && matches!(forwarding.child.try_wait(), Ok(None))
    {
        if Instant::now() >= deadline {
            tracing::warn!(
                device = device.name,
                "SSH did not share a connection in time; connecting separately for now"
            );
            *transport
                .paused
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) =
                Some(Instant::now() + MULTIPLEX_RETRY);
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    if cold
        && !listening()
        && forwarding
            .received_output
            .load(std::sync::atomic::Ordering::Acquire)
    {
        // Successful remote output proves authentication and remote startup
        // finished. A master would already have published its socket, so this
        // is a functional separate connection rather than a slow master.
        *transport
            .paused
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(Instant::now() + MULTIPLEX_RETRY);
    }
    Ok(forwarding)
}

/// Owns the transport and every pipe worker, including during partial setup.
/// Shutdown wakes local readers, reaps the child, and cancels diagnostics that
/// a detached SSH master might otherwise keep open after its stream ends.
struct Forwarding {
    child: std::process::Child,
    stream: UnixStream,
    upstream: Option<thread::JoinHandle<io::Result<u64>>>,
    downstream: Option<thread::JoinHandle<io::Result<u64>>>,
    diagnostics: Option<thread::JoinHandle<String>>,
    stop_diagnostics: Arc<Cancellation>,
    received_output: Arc<std::sync::atomic::AtomicBool>,
    settled: bool,
    transport: Option<Arc<Transport>>,
}
impl Forwarding {
    fn start(
        mut child: std::process::Child,
        stream: UnixStream,
        transport: Option<Arc<Transport>>,
    ) -> io::Result<Self> {
        let stop = match Cancellation::new() {
            Ok(stop) => Arc::new(stop),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let mut forwarding = Self {
            child,
            stream,
            upstream: None,
            downstream: None,
            diagnostics: None,
            stop_diagnostics: stop,
            received_output: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            settled: false,
            transport,
        };
        let mut input = forwarding
            .child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("SSH stdin unavailable"))?;
        let mut output = forwarding
            .child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("SSH stdout unavailable"))?;
        let stderr = forwarding
            .child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("SSH stderr unavailable"))?;
        // Pipe and socket copies must observe cancellation even when a peer
        // stops reading or a detached master retains a pipe descriptor.
        nonblocking(&input)?;
        nonblocking(&output)?;
        nonblocking(&stderr)?;
        forwarding.stream.set_nonblocking(true)?;
        let mut reader = forwarding.stream.try_clone()?;
        let mut writer = forwarding.stream.try_clone()?;
        let stop = forwarding.stop_diagnostics.clone();
        let transport = forwarding.transport.clone();
        forwarding.upstream = Some(
            thread::Builder::new()
                .name("cagents-device-input".into())
                .spawn(move || {
                    copy_transport(&mut reader, &mut input, &stop, transport.as_deref(), None)
                })?,
        );
        let received_output = forwarding.received_output.clone();
        let stop = forwarding.stop_diagnostics.clone();
        let transport = forwarding.transport.clone();
        forwarding.downstream = Some(
            thread::Builder::new()
                .name("cagents-device-output".into())
                .spawn(move || {
                    copy_transport(
                        &mut output,
                        &mut writer,
                        &stop,
                        transport.as_deref(),
                        Some(&received_output),
                    )
                })?,
        );
        let stop = forwarding.stop_diagnostics.clone();
        forwarding.diagnostics = Some(
            thread::Builder::new()
                .name("cagents-device-stderr".into())
                .spawn(move || capture_diagnostics(stderr, &stop))?,
        );
        Ok(forwarding)
    }

    fn finish(mut self, device: &Device) -> io::Result<()> {
        let result = self
            .downstream
            .take()
            .expect("forwarding output started")
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("SSH output worker panicked")));
        // EOF and copy failure must wake the input worker before waiting for
        // the child, whose remote command may itself be waiting for stdin EOF.
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if result.is_err() {
            let _ = self.child.kill();
        }
        self.stop_diagnostics.cancel();
        let deadline = Instant::now() + MASTER_EXIT_TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline
                || self
                    .transport
                    .as_ref()
                    .is_some_and(|transport| transport.cancelled.is_cancelled())
            {
                let _ = self.child.kill();
                break self.child.wait()?;
            }
            thread::sleep(Duration::from_millis(5));
        };
        self.settled = true;
        self.stop_diagnostics.cancel();
        let detail = self
            .diagnostics
            .take()
            .expect("forwarding diagnostics started")
            .join()
            .unwrap_or_default();
        if !status.success()
            && let Some(transport) = &self.transport
            && !transport.cancelled.is_cancelled()
        {
            *transport
                .last_error
                .lock()
                .unwrap_or_else(|p| p.into_inner()) =
                Some(format!("SSH to {} exited {status}: {detail}", device.host));
        }
        result.map(|_| ())
    }
}
impl Drop for Forwarding {
    fn drop(&mut self) {
        self.stop_diagnostics.cancel();
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if !self.settled {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.stop_diagnostics.cancel();
        if let Some(worker) = self.upstream.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.downstream.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.diagnostics.take() {
            let _ = worker.join();
        }
    }
}

fn nonblocking(fd: &impl std::os::fd::AsRawFd) -> io::Result<()> {
    // SAFETY: the caller owns a live descriptor; preserve its existing flags.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn wait_transport_io(
    fd: &impl std::os::fd::AsRawFd,
    events: libc::c_short,
    stop: &Cancellation,
    transport: Option<&Transport>,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let mut descriptors = vec![
        libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        },
        libc::pollfd {
            fd: stop.reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    if let Some(transport) = transport {
        descriptors.push(libc::pollfd {
            fd: transport.cancelled.reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
    }
    loop {
        if stop.is_cancelled()
            || transport.is_some_and(|transport| transport.cancelled.is_cancelled())
        {
            return Ok(());
        }
        // SAFETY: every descriptor stays owned for this poll; the cancellation
        // writer shuts down to wake all waiters, without consuming a token.
        if unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                -1,
            )
        } >= 0
        {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn copy_transport(
    reader: &mut (impl io::Read + std::os::fd::AsRawFd),
    writer: &mut (impl io::Write + std::os::fd::AsRawFd),
    stop: &Cancellation,
    transport: Option<&Transport>,
    received_output: Option<&std::sync::atomic::AtomicBool>,
) -> io::Result<u64> {
    use std::sync::atomic::Ordering;
    let cancelled = || {
        stop.is_cancelled() || transport.is_some_and(|transport| transport.cancelled.is_cancelled())
    };
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0;
    while !cancelled() {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_transport_io(reader, libc::POLLIN, stop, transport)?;
                continue;
            }
            Err(error) => return Err(error),
        };
        let mut offset = 0;
        while offset < count && !cancelled() {
            match writer.write(&buffer[offset..count]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(written) => offset += written,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    wait_transport_io(writer, libc::POLLOUT, stop, transport)?;
                }
                Err(error) => return Err(error),
            }
        }
        total += offset as u64;
        if offset < count {
            break;
        }
        writer.flush()?;
        if let Some(received_output) = received_output {
            received_output.store(true, Ordering::Release);
        }
    }
    Ok(total)
}

fn capture_diagnostics(mut stderr: std::process::ChildStderr, stop: &Cancellation) -> String {
    use std::io::Read;
    let mut captured = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stderr.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                let remaining = 4096usize.saturating_sub(captured.len());
                captured.extend_from_slice(&chunk[..count.min(remaining)]);
                // Drain an already available diagnostic prefix after child
                // exit, while bounding cancellation of a continuously busy
                // descriptor inherited by the persistent master.
                if stop.is_cancelled() && captured.len() == 4096 {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                if stop.is_cancelled() {
                    break;
                }
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if stop.is_cancelled() {
                    break;
                }
                if wait_transport_io(&stderr, libc::POLLIN, stop, None).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&captured).trim().to_owned()
}

fn lock_starting<'a>(
    starting: &'a Mutex<()>,
    cancelled: &Cancellation,
) -> io::Result<std::sync::MutexGuard<'a, ()>> {
    loop {
        if cancelled.is_cancelled() {
            return Err(io::Error::other("device connection is closed"));
        }
        match starting.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(p)) => return Ok(p.into_inner()),
            Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(5)),
        }
    }
}
fn forward(transport: &Arc<Transport>, stream: UnixStream) -> io::Result<()> {
    let (slot, starting) = Slot::acquire(&transport.pool);
    let paused = transport
        .paused
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .is_some_and(|until| Instant::now() < until);
    let control = control_path(&transport.directory, slot.index).filter(|_| !paused);
    spawn_stream(transport, control.as_deref(), &starting, stream)?.finish(&transport.device)
}
/// Runs on the selected host through SSH. stdout contains only framed protocol
/// bytes; diagnostics stay on stderr. EOF detaches the UI, leaving jobs alive.
pub fn serve_stdio() -> io::Result<()> {
    if is_remote() {
        return Err(io::Error::other(
            "a daemon bridge cannot target another device",
        ));
    }
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            // SSH sessions often lack the login session's runtime variable.
            // Re-exec with a child environment instead of mutating the process
            // environment while other library threads may be running.
            let directory = bridge_runtime_directory(std::path::Path::new("/run/user"), unsafe {
                libc::geteuid()
            })?;
            return Err(Command::new(std::env::current_exe()?)
                .args(std::env::args_os().skip(1))
                .env("XDG_RUNTIME_DIR", directory)
                .exec());
        }
        #[cfg(not(target_os = "linux"))]
        return Err(io::Error::other(
            "XDG_RUNTIME_DIR is missing; configure an owned private runtime directory for the remote daemon",
        ));
    }
    let path = crate::terminal::ensure_daemon_ready().map_err(io::Error::other)?;
    let stream = UnixStream::connect(path)?;
    let mut upstream = stream.try_clone()?;
    let input = thread::spawn(move || {
        let result = copy_flushed(&mut io::stdin().lock(), &mut upstream);
        let _ = upstream.shutdown(std::net::Shutdown::Write);
        result
    });
    let mut downstream = stream;
    let result = copy_flushed(&mut downstream, &mut io::stdout().lock());
    // stdin may stay blocked after a server disconnect. Process exit releases
    // it; do not join that thread until its copy has already finished.
    if input.is_finished() {
        let _ = input.join();
    }
    result.map(|_| ())
}
#[cfg(any(target_os = "linux", test))]
fn bridge_runtime_directory(base: &std::path::Path, uid: u32) -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let directory = base.join(uid.to_string());
    let metadata = fs::symlink_metadata(&directory).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "XDG_RUNTIME_DIR is missing and {} is unavailable: {error}",
                directory.display()
            ),
        )
    })?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "XDG_RUNTIME_DIR is missing and {} must be an owned private directory with mode 0700",
                directory.display()
            ),
        ));
    }
    Ok(directory)
}

fn copy_flushed(reader: &mut impl io::Read, writer: &mut impl io::Write) -> io::Result<u64> {
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0;
    loop {
        let count = match reader.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            return Ok(total);
        }
        writer.write_all(&buffer[..count])?;
        // stdout's LineWriter must deliver binary frames without waiting for
        // a newline or the next request from the peer.
        writer.flush()?;
        total += count as u64;
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_transport(device: Device, directory: PathBuf) -> Arc<Transport> {
        Arc::new(Transport {
            device,
            directory,
            pool: Arc::new(Mutex::new(Vec::new())),
            paused: Mutex::new(None),
            last_error: Mutex::new(None),
            cancelled: Cancellation::new().unwrap(),
        })
    }

    #[test]
    fn releasing_an_old_slot_only_changes_its_originating_pool() {
        let old = Arc::new(Mutex::new(Vec::new()));
        let new = Arc::new(Mutex::new(Vec::new()));
        let (old_slot, _) = Slot::acquire(&old);
        let (new_slot, _) = Slot::acquire(&new);
        drop(old_slot);
        assert_eq!(old.lock().unwrap()[0].streams, 0);
        assert_eq!(new.lock().unwrap()[0].streams, 1);
        drop(new_slot);
        assert_eq!(new.lock().unwrap()[0].streams, 0);
    }

    #[test]
    fn cancellation_interrupts_startup_coordination_without_unlocking_the_owner() {
        let lock = Arc::new(Mutex::new(()));
        let held = lock.lock().unwrap();
        let cancelled = Arc::new(Cancellation::new().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_lock = lock.clone();
        let worker_cancelled = cancelled.clone();
        let worker = thread::spawn(move || {
            tx.send(lock_starting(&worker_lock, &worker_cancelled).is_err())
                .unwrap();
        });
        cancelled.cancel();
        assert!(rx.recv_timeout(Duration::from_secs(1)).unwrap());
        assert!(lock.try_lock().is_err());
        drop(held);
        worker.join().unwrap();
    }

    #[test]
    fn bridge_close_releases_listener_without_reopening_a_closed_context() {
        let context = ConnectionContext::new(Device {
            name: "fixture".into(),
            host: "unused-fixture".into(),
            executable: "cagents".into(),
        });
        // No remote connection is made: exercise the real listener/cancellation
        // ownership with a private local socket and the production accept loop.
        let directory = tempfile::tempdir().unwrap();
        let transport = fixture_transport(context.device.clone(), directory.path().to_path_buf());
        let path = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let owned = transport.clone();
        let worker = thread::spawn(move || run_bridge(listener, owned));
        *context.bridge.lock().unwrap() = Some(Bridge {
            transport,
            worker: Some(worker),
        });
        assert_eq!(context.socket_path().unwrap(), path);
        let started = Instant::now();
        context.close();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!path.exists());
        assert!(context.socket_path().is_err());
    }

    #[test]
    fn closing_transport_reaps_only_its_owned_forwarding_child() {
        let directory = tempfile::tempdir().unwrap();
        let transport = fixture_transport(
            Device {
                name: "fixture".into(),
                host: "unused-fixture".into(),
                executable: "cagents".into(),
            },
            directory.path().to_path_buf(),
        );
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let (_client, server) = UnixStream::pair().unwrap();
        let forwarding = Forwarding::start(child, server, Some(transport.clone())).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let device = transport.device.clone();
        let worker = thread::spawn(move || {
            tx.send(forwarding.finish(&device)).unwrap();
        });
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        transport.cancelled.cancel();
        rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        worker.join().unwrap();
        // SAFETY: only querying our exact fixture child; no PID discovery/signal.
        assert_eq!(
            unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert!(
            transport.last_error.lock().unwrap().is_none(),
            "intentional closure is not a device connection error"
        );
    }

    #[test]
    fn cancellation_wakes_a_copy_blocked_on_peer_backpressure() {
        use std::io::Write;
        let (mut reader, mut input) = UnixStream::pair().unwrap();
        let (mut writer, _peer_not_reading) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        writer.set_nonblocking(true).unwrap();
        let padding = [0u8; 65536];
        loop {
            match writer.write(&padding) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("fill owned socket: {error}"),
            }
        }
        input.write_all(b"pending bytes").unwrap();
        let stop = Arc::new(Cancellation::new().unwrap());
        let copy_stop = stop.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            tx.send(copy_transport(
                &mut reader,
                &mut writer,
                &copy_stop,
                None,
                None,
            ))
            .unwrap();
        });
        // The socket remains full; successful completion requires cancellation,
        // independently of whether the worker has reached poll yet.
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        stop.cancel();
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap(), 0);
        worker.join().unwrap();
    }

    #[test]
    fn device_configuration_rejects_a_fifo_without_a_writer() {
        const CHILD: &str = "CAGENTS_TEST_DEVICE_CONFIG_FIFO";
        if let Some(path) = std::env::var_os(CHILD) {
            let path = PathBuf::from(path);
            let before = fs::symlink_metadata(&path).unwrap();
            assert_eq!(
                list_from_path(&path).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            let after = fs::symlink_metadata(&path).unwrap();
            assert!(after.file_type().is_fifo());
            assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
            fs::write(path.with_extension("verified"), b"FIFO refused").unwrap();
            return;
        }
        use std::os::unix::ffi::OsStrExt;
        // The parent retains the fixture even if a regressed child is killed.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devices.json");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a live NUL-terminated path in the owned fixture.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        // Bound a regression in open itself without leaving a blocked test
        // thread or changing this process's environment.
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "device::tests::device_configuration_rejects_a_fifo_without_a_writer",
                "--nocapture",
            ])
            .env(CHILD, &path)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "isolated FIFO reader failed: {status}");
                assert_eq!(
                    fs::read(path.with_extension("verified")).unwrap(),
                    b"FIFO refused"
                );
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("device configuration reader waited for a FIFO writer");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn device_configuration_preserves_regular_symlink_missing_and_bounds_contracts() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devices.json");
        assert!(list_from_path(&path).unwrap().is_empty());
        let document = br#"{"devices":[{"name":"fixture","host":"offline"}]}"#;
        fs::write(&path, document).unwrap();
        let loaded = list_from_path(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "fixture");
        assert_eq!(loaded[0].executable, "cagents");
        let link = directory.path().join("linked.json");
        symlink(&path, &link).unwrap();
        assert_eq!(list_from_path(&link).unwrap()[0].host, "offline");
        let mut exact = document.to_vec();
        exact.resize(64 * 1024, b' ');
        fs::write(&path, &exact).unwrap();
        assert_eq!(list_from_path(&link).unwrap().len(), 1);
        exact.push(b' ');
        fs::write(&path, &exact).unwrap();
        assert!(
            list_from_path(&path)
                .unwrap_err()
                .to_string()
                .contains("64 KiB")
        );
        fs::write(&path, br#"{"devices":[{"name":"fixture","host":"offline"},{"name":"fixture","host":"offline"}]}"#).unwrap();
        assert!(
            list_from_path(&path)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        fs::write(&path, br#"{"devices":[{"name":"local","host":"offline"}]}"#).unwrap();
        assert!(list_from_path(&path).is_err());
        assert_eq!(
            list_from_path(directory.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn bridge_runtime_fallback_requires_owned_private_real_directory() {
        use std::os::unix::fs::symlink;
        let base = tempfile::tempdir().unwrap();
        let uid = unsafe { libc::geteuid() };
        let directory = base.path().join(uid.to_string());
        assert!(bridge_runtime_directory(base.path(), uid).is_err());
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            bridge_runtime_directory(base.path(), uid).unwrap(),
            directory
        );
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(bridge_runtime_directory(base.path(), uid).is_err());
        fs::remove_dir(&directory).unwrap();
        symlink(base.path(), &directory).unwrap();
        assert!(bridge_runtime_directory(base.path(), uid).is_err());
        fs::remove_file(&directory).unwrap();
        fs::write(&directory, b"not a directory").unwrap();
        assert!(bridge_runtime_directory(base.path(), uid).is_err());
    }

    #[test]
    fn binary_bridge_flushes_a_response_before_reading_the_next_frame() {
        struct Writer {
            pending: Vec<u8>,
            visible: std::sync::Arc<Mutex<Vec<u8>>>,
        }
        impl io::Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.pending.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.visible.lock().unwrap().extend(self.pending.drain(..));
                Ok(())
            }
        }
        struct Reader {
            visible: std::sync::Arc<Mutex<Vec<u8>>>,
            sent: bool,
        }
        impl io::Read for Reader {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                if self.sent {
                    assert_eq!(*self.visible.lock().unwrap(), b"binary-response");
                    return Ok(0);
                }
                self.sent = true;
                bytes[..15].copy_from_slice(b"binary-response");
                Ok(15)
            }
        }
        let visible = std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut reader = Reader {
            visible: visible.clone(),
            sent: false,
        };
        let mut writer = Writer {
            visible,
            pending: Vec::new(),
        };
        assert_eq!(copy_flushed(&mut reader, &mut writer).unwrap(), 15);
    }
    #[test]
    fn device_windows_are_separate_and_bounded() {
        assert_ne!(
            scoped_window_slot("a-b", "c"),
            scoped_window_slot("a", "b-c")
        );
        assert_ne!(scoped_window_slot("fw16", "primary"), "fw16-primary");
        assert_eq!(
            scoped_window_slot("fw16", "primary"),
            scoped_window_slot("fw16", "primary")
        );
        crate::instance::validate_window_slot(&scoped_window_slot(
            "long-device-name",
            "a-very-long-but-valid-window-name",
        ))
        .unwrap();
    }
    #[test]
    fn ssh_bridge_forwards_before_an_unpublished_master_and_propagates_eof() {
        use std::io::{BufRead, Write};
        const CHILD: &str = "CAGENTS_TEST_UNPUBLISHED_SSH_MASTER";
        let Some(directory) = std::env::var_os(CHILD) else {
            let directory = tempfile::tempdir().unwrap();
            let bin = directory.path().join("bin");
            fs::create_dir(&bin).unwrap();
            let ssh = bin.join("ssh");
            fs::write(&ssh, b"#!/bin/sh\nIFS= read -r request || exit 1\nprintf 'reply:%s\\n' \"$request\"\ncat >/dev/null\n").unwrap();
            fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "device::tests::ssh_bridge_forwards_before_an_unpublished_master_and_propagates_eof", "--nocapture"])
                .env(CHILD, directory.path())
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .output().unwrap();
            assert!(
                output.status.success(),
                "isolated SSH bridge test failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        };
        let directory = PathBuf::from(directory);
        // A final-name collision is the real OpenSSH graceful fallback:
        // `ControlSocket ... already exists, disabling multiplexing`.
        fs::create_dir(directory.join("ssh-0")).unwrap();
        let mut connections = Vec::new();
        for _ in 0..2 {
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let directory = directory.clone();
            let handler = thread::spawn(move || {
                let device = Device {
                    name: "fixture".into(),
                    host: "fixture".into(),
                    executable: "cagents".into(),
                };
                let transport = fixture_transport(device, directory);
                done_tx.send(forward(&transport, server)).unwrap();
            });
            client.write_all(b"handshake\n").unwrap();
            let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
            let mut response = String::new();
            reader.read_line(&mut response).unwrap();
            assert_eq!(response, "reply:handshake\n");
            connections.push((client, reader, done_rx, handler));
        }
        // Keep the first separate stream alive while the second connects.
        // Healthy traffic must release startup coordination before its 20s
        // timeout, rather than serializing all later handshakes behind it.
        for (_, _, done, _) in &connections {
            assert!(matches!(
                done.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ));
        }
        for (client, mut reader, done, handler) in connections {
            client.shutdown(std::net::Shutdown::Write).unwrap();
            done.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
            assert_eq!(reader.read_line(&mut String::new()).unwrap(), 0);
            handler.join().unwrap();
        }
    }

    #[test]
    fn ssh_forwarding_drop_reaps_child_and_unblocks_pipe_workers() {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let (mut client, server) = UnixStream::pair().unwrap();
        let forwarding = Forwarding::start(child, server, None).unwrap();
        let start = Instant::now();
        drop(forwarding);
        assert!(start.elapsed() < Duration::from_secs(2));
        use std::io::Read;
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(client.read(&mut [0]).unwrap(), 0);
        // SAFETY: this exact child PID is queried without waiting or sending a
        // signal. ECHILD proves the guard has already reaped it.
        assert_eq!(
            unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn ssh_forwarding_setup_failure_reaps_child() {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let (_client, server) = UnixStream::pair().unwrap();
        assert!(Forwarding::start(child, server, None).is_err());
        // SAFETY: query only the child we created; no signal or blocking wait.
        assert_eq!(
            unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn streams_multiplex_over_a_private_master_and_spill_to_the_next() {
        let device = Device {
            name: "fw16".into(),
            host: "marvin@fw16".into(),
            executable: "/home/a b/cagents".into(),
        };
        let arguments = |control| {
            bridge_command(&device, control)
                .get_args()
                .map(|argument| argument.to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let plain = arguments(None);
        assert!(!plain.iter().any(|argument| argument.contains("Control")));
        assert_eq!(
            plain[plain.len() - 3..],
            ["--", "marvin@fw16", "'/home/a b/cagents' daemon bridge"]
        );
        let directory = Path::new("/run/user/1000/cagents/device-0123456789ab");
        let control = control_path(directory, 0).unwrap();
        assert_eq!(control, "/run/user/1000/cagents/device-0123456789ab/ssh-0");
        let multiplexed = arguments(Some(&control));
        // Options precede `--`, so the host can never be read as one.
        assert_eq!(
            multiplexed[multiplexed.len() - 3..],
            plain[plain.len() - 3..]
        );
        assert_eq!(
            multiplexed[plain.len() - 3..multiplexed.len() - 3],
            [
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=/run/user/1000/cagents/device-0123456789ab/ssh-0",
                "-o",
                "ControlPersist=60"
            ]
        );
        // Unusable control paths fall back to one connection per stream.
        let long = Path::new("/run").join("x".repeat(90));
        assert_eq!(control_path(&long, 0), None);
        assert_eq!(control_path(Path::new("/run/user/a b"), 0), None);
        assert_eq!(control_path(Path::new("/run/user/%h"), 0), None);

        let pool = Arc::new(Mutex::new(Vec::new()));
        let mut slots = (0..=STREAMS_PER_MASTER)
            .map(|_| Slot::acquire(&pool).0)
            .collect::<Vec<_>>();
        assert_eq!(slots.pop().unwrap().index, 1);
        assert!(slots.iter().all(|slot| slot.index == 0));
        slots.pop();
        assert_eq!(Slot::acquire(&pool).0.index, 0);
    }
    #[test]
    fn remote_command_quotes_executable_and_rejects_options() {
        assert_eq!(quote("/home/a'b/cagents"), "'/home/a'\\''b/cagents'");
        assert!(
            validate(&Device {
                name: "fw16".into(),
                host: "-oProxyCommand=evil".into(),
                executable: default_executable()
            })
            .is_err()
        );
        assert!(
            validate(&Device {
                name: "local".into(),
                host: "fw16".into(),
                executable: default_executable()
            })
            .is_err()
        );
        assert!(
            validate(&Device {
                name: "fw16".into(),
                host: "marvin@fw16".into(),
                executable: default_executable()
            })
            .is_ok()
        );
    }
}

pub fn home() -> Option<&'static PathBuf> {
    SELECTED.get().and_then(|context| context.home.get())
}
pub fn shell() -> Option<&'static str> {
    SELECTED
        .get()
        .and_then(|context| context.shell.get())
        .map(String::as_str)
}
pub(crate) fn shell_error() -> Option<&'static str> {
    SELECTED
        .get()
        .and_then(|context| context.shell_error.get())
        .and_then(|error| error.as_deref())
}
pub fn prepare() -> io::Result<()> {
    if !is_remote() {
        return Ok(());
    }
    match crate::device_service::request(crate::device_service::Operation::Home) {
        Ok(crate::device_service::Reply::Home {
            path,
            shell,
            environment,
            shell_error,
        }) => {
            let _ = SELECTED.get().unwrap().home.set(path);
            let _ = SELECTED.get().unwrap().environment.set(environment);
            let _ = SELECTED.get().unwrap().shell.set(shell);
            let _ = SELECTED.get().unwrap().shell_error.set(shell_error);
            Ok(())
        }
        Ok(_) => Err(io::Error::other(
            "remote daemon returned an unexpected device identity",
        )),
        Err(error) => Err(io::Error::other(
            format!(
                "could not connect to device {} (SSH host {}): {error}; verify `ssh {} cagents daemon start` and install matching cagents executables",
                selected().unwrap().name,
                selected().unwrap().host,
                selected().unwrap().host
            ) + &SELECTED
                .get()
                .unwrap()
                .bridge
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .and_then(|bridge| {
                    bridge
                        .transport
                        .last_error
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .clone()
                })
                .map(|error| format!("; {error}"))
                .unwrap_or_default(),
        )),
    }
}

#[cfg(any(feature = "ui", feature = "host"))]
pub(crate) fn environment(
    provider: crate::agent::Provider,
    persistable: bool,
) -> std::collections::BTreeMap<String, String> {
    if persistable {
        SELECTED
            .get()
            .and_then(|context| context.environment.get())
            .and_then(|environment| environment.get(&provider))
            .cloned()
            .unwrap_or_default()
    } else {
        Default::default()
    }
}

/// Save one local connection profile. Provider credentials remain exclusively
/// with their daemon host; this file contains only SSH routing information.
pub fn save(device: Device) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    validate(&device)?;
    let mut devices = list()?;
    devices.retain(|existing| existing.name != device.name);
    devices.push(device);
    devices.sort_by(|a, b| a.name.cmp(&b.name));
    let path = configuration_path()?;
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("configuration has no parent directory"))?;
    crate::secure_file::prepare_private_directory(directory)?;
    let temporary = directory.join(format!(".devices-{}.json", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(
            &serde_json::to_vec_pretty(&Devices { devices }).map_err(io::Error::other)?,
        )?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        fs::File::open(directory)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
