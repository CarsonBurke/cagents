//! Window-wide backend selection. Remote connections use the user's SSH trust
//! configuration, never a network listener or a copied credential store.
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
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
static REMOTE_HOME: OnceLock<PathBuf> = OnceLock::new();
static REMOTE_SHELL: OnceLock<String> = OnceLock::new();
static SELECTED: OnceLock<Device> = OnceLock::new();
static BRIDGE: Mutex<Option<PathBuf>> = Mutex::new(None);

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
    match fs::read(&path) {
        Ok(bytes) => {
            let config: Devices = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
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
    match selected() {
        None => Ok(slot.map(str::to_owned)),
        Some(device) => {
            let name = format!(
                "{}-{}",
                device.name,
                slot.unwrap_or(crate::instance::PRIMARY_WINDOW)
            );
            crate::instance::validate_window_slot(&name)?;
            Ok(Some(name))
        }
    }
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn ssh_command(device: &Device) -> Command {
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
        "--",
        &device.host,
    ]);
    command.arg(format!("{} daemon bridge", quote(&device.executable)));
    command
}
/// Each process owns its private local proxy; reconnecting a client establishes
/// a fresh SSH stream. No exported socket is trusted by UID alone across hosts.
pub fn socket_path() -> io::Result<Option<PathBuf>> {
    let Some(device) = selected() else {
        return Ok(None);
    };
    let mut bridge = BRIDGE.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(path) = bridge.as_ref() {
        return Ok(Some(path.clone()));
    }
    let runtime = crate::instance::runtime_directory()?.join(crate::instance::RUNTIME_SUBDIRECTORY);
    fs::create_dir_all(&runtime)?;
    let directory = runtime.join(format!("device-{}-{}", device.name, std::process::id()));
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let path = directory.join("daemon.sock");
    let listener = UnixListener::bind(&path)?;
    let device = device.clone();
    thread::Builder::new().name("cagents-device-bridge".into()).spawn(move || {
        for incoming in listener.incoming() {
            let Ok(stream) = incoming else { break; };

            let device = device.clone();
            let _ = thread::Builder::new().name("cagents-device-connection".into()).spawn(move || {
                if let Err(error) = forward(&device, stream) { tracing::warn!(device = device.name, %error, "remote daemon connection ended; verify SSH access and matching cagents installation"); }
            });
        }
    })?;
    *bridge = Some(path.clone());
    Ok(Some(path))
}
fn forward(device: &Device, stream: UnixStream) -> io::Result<()> {
    let mut child = ssh_command(device)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
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
    let upstream = thread::spawn(move || io::copy(&mut reader, &mut input));
    let result = io::copy(&mut output, &mut writer);
    let _ = writer.shutdown(std::net::Shutdown::Both);
    let _ = child.kill();
    let _ = child.wait();
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
    let path = crate::terminal::ensure_daemon_ready().map_err(io::Error::other)?;
    let stream = UnixStream::connect(path)?;
    let mut upstream = stream.try_clone()?;
    let input = thread::spawn(move || {
        let result = io::copy(&mut io::stdin().lock(), &mut upstream);
        let _ = upstream.shutdown(std::net::Shutdown::Write);
        result
    });
    let mut downstream = stream;
    let result = io::copy(&mut downstream, &mut io::stdout().lock());
    // stdin may stay blocked after a server disconnect. Process exit releases
    // it; do not join that thread until its copy has already finished.
    if input.is_finished() {
        let _ = input.join();
    }
    result.map(|_| ())
}
#[cfg(test)]
mod tests {
    use super::*;
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
        Ok(crate::device_service::Reply::Home { path, shell }) => {
            let _ = REMOTE_HOME.set(path);
            let _ = REMOTE_SHELL.set(shell);
            Ok(())
        }
        Ok(_) => Err(io::Error::other(
            "remote daemon returned an unexpected device identity",
        )),
        Err(error) => Err(io::Error::other(format!(
            "could not connect to device {} (SSH host {}): {error}; verify `ssh {} cagents daemon start` and install matching cagents executables",
            selected().unwrap().name,
            selected().unwrap().host,
            selected().unwrap().host
        ))),
    }
}
