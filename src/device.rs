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
    sync::{
        Arc, Mutex, OnceLock, TryLockError,
        atomic::{AtomicBool, Ordering},
    },
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
/// Exceeds `ConnectTimeout`. A stream still connected then without a master has
/// an SSH that cannot multiplex here, and no later stream waits for one again.
const MASTER_START_TIMEOUT: Duration = Duration::from_secs(20);
static MULTIPLEX: AtomicBool = AtomicBool::new(true);

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
    let path = configuration_path()?;
    use std::io::Read;
    let bytes = fs::File::open(&path).and_then(|file| {
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
                    if let Some(device) = selected().filter(|_| Path::new(&control).exists()) {
                        let _ = ssh(Some(&control))
                            .args(["-O", "exit", "--", &device.host])
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status();
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
) -> io::Result<std::process::Child> {
    let spawn = || {
        bridge_command(device, control)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    };
    let Some(control) = control else {
        return spawn();
    };
    let (_starting, waited) = match starting.try_lock() {
        Ok(guard) => (guard, false),
        Err(TryLockError::Poisoned(poison)) => (poison.into_inner(), false),
        Err(TryLockError::WouldBlock) => (
            starting.lock().unwrap_or_else(|poison| poison.into_inner()),
            true,
        ),
    };
    let listening = || UnixStream::connect(control).is_ok();
    let cold = !listening();
    let mut child = spawn()?;
    // A stream that waited through a failed start must not make the rest queue
    // behind its own attempt; they fail or connect in parallel as before.
    let deadline = Instant::now() + MASTER_START_TIMEOUT;
    while cold && !waited && !listening() && matches!(child.try_wait(), Ok(None)) {
        if Instant::now() >= deadline {
            MULTIPLEX.store(false, Ordering::Relaxed);
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(child)
}
fn forward(device: &Device, directory: &Path, stream: UnixStream) -> io::Result<()> {
    let (slot, starting) = Slot::acquire();
    let control = control_path(directory, slot.0).filter(|_| MULTIPLEX.load(Ordering::Relaxed));
    let mut child = spawn_stream(device, control.as_deref(), &starting)?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("SSH stderr unavailable"))?;
    let diagnostics = thread::spawn(move || {
        use std::io::Read;
        let mut stderr = stderr;
        let mut captured = Vec::new();
        let mut chunk = [0u8; 1024];
        while let Ok(count) = stderr.read(&mut chunk) {
            if count == 0 {
                break;
            }
            let remaining = 4096usize.saturating_sub(captured.len());
            captured.extend_from_slice(&chunk[..count.min(remaining)]);
        }
        String::from_utf8_lossy(&captured).trim().to_owned()
    });
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("SSH stdin unavailable"))?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("SSH stdout unavailable"))?;
    let mut reader = stream.try_clone()?;
    let mut writer = stream;
    let upstream = thread::spawn(move || copy_flushed(&mut reader, &mut input));
    let result = copy_flushed(&mut output, &mut writer);
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let detail = diagnostics.join().unwrap_or_default();
    if !status.success() {
        *LAST_ERROR
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) =
            Some(format!("SSH to {} exited {status}: {detail}", device.host));
    }
    let _ = writer.shutdown(std::net::Shutdown::Both);
    let _ = upstream.join();
    result.map(|_| ())
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
