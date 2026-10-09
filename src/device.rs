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
static REMOTE_ENVIRONMENT: OnceLock<
    std::collections::BTreeMap<crate::agent::Provider, std::collections::BTreeMap<String, String>>,
> = OnceLock::new();
static REMOTE_HOME: OnceLock<PathBuf> = OnceLock::new();
static REMOTE_SHELL: OnceLock<String> = OnceLock::new();
static SELECTED: OnceLock<Device> = OnceLock::new();
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
static BRIDGE: Mutex<Option<PathBuf>> = Mutex::new(None);
static MASTERS: Mutex<Vec<Master>> = Mutex::new(Vec::new());
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
static MULTIPLEX_PAUSED: Mutex<Option<Instant>> = Mutex::new(None);
/// `ssh -O exit` answers a healthy master in milliseconds. One that does not
/// answer would hold process exit for `ConnectTimeout` and then log in afresh.
const MASTER_EXIT_TIMEOUT: Duration = Duration::from_secs(2);

pub fn selected() -> Option<&'static Device> {
    SELECTED.get()
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
        .set(device)
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
        if let Some(path) = BRIDGE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            let _ = fs::remove_file(&path);
            if let Some(directory) = path.parent() {
                let masters = MASTERS
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .len();
                for control in (0..masters).filter_map(|master| control_path(directory, master)) {
                    if let Some(device) = selected().filter(|_| Path::new(&control).exists())
                        && let Ok(mut exit) = ssh(Some(&control))
                            .args(["-O", "exit", "--", &device.host])
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()
                    {
                        let deadline = Instant::now() + MASTER_EXIT_TIMEOUT;
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
                let _ = fs::remove_dir(directory);
            }
        }
    }
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
struct Slot(usize);
impl Slot {
    fn acquire() -> (Self, Arc<Mutex<()>>) {
        let mut masters = MASTERS.lock().unwrap_or_else(|poison| poison.into_inner());
        let index = masters
            .iter()
            .position(|master| master.streams < STREAMS_PER_MASTER)
            .unwrap_or_else(|| {
                masters.push(Master::default());
                masters.len() - 1
            });
        masters[index].streams += 1;
        (Self(index), masters[index].starting.clone())
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        MASTERS.lock().unwrap_or_else(|poison| poison.into_inner())[self.0].streams -= 1;
    }
}
/// Each process owns its private local proxy; reconnecting a client establishes
/// a fresh SSH stream. No exported socket is trusted by UID alone across hosts:
/// the control sockets beside the proxy stay on this host in the same private
/// directory, and grant only the SSH access their owner already has.
pub fn socket_path() -> io::Result<Option<PathBuf>> {
    let Some(device) = selected() else {
        return Ok(None);
    };
    let mut bridge = BRIDGE.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(path) = bridge.as_ref() {
        return Ok(Some(path.clone()));
    }
    let runtime = crate::instance::runtime_directory()?.join(crate::instance::RUNTIME_SUBDIRECTORY);
    crate::secure_file::prepare_private_directory(&runtime)?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let directory = runtime.join(format!("device-{}", &token[..12]));
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let path = directory.join("daemon.sock");
    let listener = UnixListener::bind(&path)?;
    let device = device.clone();
    thread::Builder::new().name("cagents-device-bridge".into()).spawn(move || {
        for incoming in listener.incoming() {
            let Ok(stream) = incoming else { break; };

            let device = device.clone();
            let directory = directory.clone();
            let _ = thread::Builder::new().name("cagents-device-connection".into()).spawn(move || {
                if let Err(error) = forward(&device, &directory, stream) { tracing::warn!(device = device.name, %error, "remote daemon connection ended; verify SSH access and matching cagents installation"); }
            });
        }
    })?;
    *bridge = Some(path.clone());
    Ok(Some(path))
}
/// Spawn one stream. When its master is not listening this stream becomes it
/// (`ControlMaster=auto`, which also reconnects alone past a stale, unresponsive
/// or full master); later streams wait for that socket instead of racing it.
/// Killing a stream never stops the master, which OpenSSH detaches. Connecting
/// to the control socket and hanging up is how liveness is read without a
/// process per stream; the master discards such a client.
fn spawn_stream(
    device: &Device,
    control: Option<&str>,
    starting: &Mutex<()>,
    stream: UnixStream,
) -> io::Result<Forwarding> {
    let (_starting, waited) = if control.is_some() {
        match starting.try_lock() {
            Ok(guard) => (Some(guard), false),
            Err(TryLockError::Poisoned(poison)) => (Some(poison.into_inner()), false),
            Err(TryLockError::WouldBlock) => (
                Some(starting.lock().unwrap_or_else(|poison| poison.into_inner())),
                true,
            ),
        }
    } else {
        (None, false)
    };
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
    let mut forwarding = Forwarding::start(child, stream)?;
    // A stream that waited through a failed start must not make the rest queue
    // behind its own attempt; they fail or connect in parallel as before.
    let deadline = Instant::now() + MASTER_START_TIMEOUT;
    while cold
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
            *MULTIPLEX_PAUSED
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
        *MULTIPLEX_PAUSED
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
    stop_diagnostics: Arc<std::sync::atomic::AtomicBool>,
    received_output: Arc<std::sync::atomic::AtomicBool>,
    settled: bool,
}
impl Forwarding {
    fn start(child: std::process::Child, stream: UnixStream) -> io::Result<Self> {
        let mut forwarding = Self {
            child,
            stream,
            upstream: None,
            downstream: None,
            diagnostics: None,
            stop_diagnostics: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            received_output: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            settled: false,
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
        use std::os::fd::AsRawFd;
        // SAFETY: stderr owns this live descriptor, and flags preserves its
        // existing status flags while making cancellation reads nonblocking.
        let flags = unsafe { libc::fcntl(stderr.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(stderr.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut reader = forwarding.stream.try_clone()?;
        let mut writer = forwarding.stream.try_clone()?;
        forwarding.upstream = Some(
            thread::Builder::new()
                .name("cagents-device-input".into())
                .spawn(move || copy_flushed(&mut reader, &mut input))?,
        );
        let received_output = forwarding.received_output.clone();
        forwarding.downstream = Some(
            thread::Builder::new()
                .name("cagents-device-output".into())
                .spawn(move || {
                    copy_flushed_notifying(&mut output, &mut writer, Some(&received_output))
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
        let status = self.child.wait()?;
        self.settled = true;
        self.stop_diagnostics
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let detail = self
            .diagnostics
            .take()
            .expect("forwarding diagnostics started")
            .join()
            .unwrap_or_default();
        if !status.success() {
            *LAST_ERROR
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) =
                Some(format!("SSH to {} exited {status}: {detail}", device.host));
        }
        result.map(|_| ())
    }
}
impl Drop for Forwarding {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if !self.settled {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.stop_diagnostics
            .store(true, std::sync::atomic::Ordering::Relaxed);
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

fn capture_diagnostics(
    mut stderr: std::process::ChildStderr,
    stop: &std::sync::atomic::AtomicBool,
) -> String {
    use std::{io::Read, os::fd::AsRawFd, sync::atomic::Ordering};
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
                if stop.load(Ordering::Relaxed) && captured.len() == 4096 {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let mut descriptor = libc::pollfd {
                    fd: stderr.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: this one-element poll array and descriptor remain
                // valid for the call. The short poll bounds cancellation.
                let result = unsafe { libc::poll(&mut descriptor, 1, 100) };
                if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&captured).trim().to_owned()
}

fn forward(device: &Device, directory: &Path, stream: UnixStream) -> io::Result<()> {
    let (slot, starting) = Slot::acquire();
    let paused = MULTIPLEX_PAUSED
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .is_some_and(|until| Instant::now() < until);
    let control = control_path(directory, slot.0).filter(|_| !paused);
    spawn_stream(device, control.as_deref(), &starting, stream)?.finish(device)
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
    copy_flushed_notifying(reader, writer, None)
}
fn copy_flushed_notifying(
    reader: &mut impl io::Read,
    writer: &mut impl io::Write,
    received_output: Option<&std::sync::atomic::AtomicBool>,
) -> io::Result<u64> {
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
        if let Some(received_output) = received_output {
            received_output.store(true, std::sync::atomic::Ordering::Release);
        }
        total += count as u64;
    }
}
#[cfg(test)]
mod tests {
    use super::*;

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
                done_tx.send(forward(&device, &directory, server)).unwrap();
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
        let forwarding = Forwarding::start(child, server).unwrap();
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
        assert!(Forwarding::start(child, server).is_err());
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

        let mut slots = (0..=STREAMS_PER_MASTER)
            .map(|_| Slot::acquire().0)
            .collect::<Vec<_>>();
        assert_eq!(slots.pop().unwrap().0, 1);
        assert!(slots.iter().all(|slot| slot.0 == 0));
        slots.pop();
        assert_eq!(Slot::acquire().0.0, 0);
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
    REMOTE_HOME.get()
}
pub fn shell() -> Option<&'static str> {
    REMOTE_SHELL.get().map(String::as_str)
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
        }) => {
            let _ = REMOTE_HOME.set(path);
            let _ = REMOTE_ENVIRONMENT.set(environment);
            let _ = REMOTE_SHELL.set(shell);
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
            ) + &LAST_ERROR
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_ref()
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
        REMOTE_ENVIRONMENT
            .get()
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
