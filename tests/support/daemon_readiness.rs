//! Readiness of an explicitly owned fixture daemon, without probing its lease.
use super::private_cli_capture;
use cagents::process::ProcessFailure;
use serde_json::Value;
use std::{
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::SocketAddr,
    },
    path::Path,
    process::{Child, Output},
    thread,
    time::{Duration, Instant},
};

pub fn serving_status(response: &Value) -> bool {
    response["ok"] == true
        && response["data"]["running"] == true
        && response["data"]["daemon_version"]
            .as_str()
            .is_some_and(|version| !version.is_empty())
}

pub fn wait(root: &Path, child: &mut Child, deadline: Instant) -> Result<(), String> {
    // Commands clear their environment, so use the matched binaries' default
    // namespace rather than an instance override inherited by the test harness.
    let socket = root
        .join("run")
        .join(cagents::instance::RUNTIME_SUBDIRECTORY)
        .join(cagents::terminal_ipc::SOCKET_FILE_NAME);
    wait_with(child, &socket, deadline, |deadline| {
        let spec = private_cli_capture::private_cli(root)
            .env("TERM", "xterm-256color")
            .args(["daemon", "status"]);
        private_cli_capture::capture(spec, deadline)
    })
    .map_err(|error| {
        let log = fs::read(root.join("daemon.log")).unwrap_or_default();
        format!(
            "{error}; daemon log: {}",
            String::from_utf8_lossy(&log[..log.len().min(32 * 1024)])
        )
    })
}

pub(crate) fn wait_with(
    child: &mut Child,
    socket: &Path,
    deadline: Instant,
    mut status: impl FnMut(Instant) -> Result<Output, ProcessFailure>,
) -> Result<(), String> {
    let mut readiness = Readiness::new(deadline);
    loop {
        if readiness.poll(child, socket, &mut status)? {
            return Ok(());
        }
        thread::sleep(
            Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

pub(crate) struct Readiness {
    deadline: Instant,
    last_status: String,
}

impl Readiness {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            last_status: "no status query issued before a connectable listener".into(),
        }
    }

    fn check_child(&self, child: &mut Child) -> Result<(), String> {
        if let Some(exit) = child.try_wait().map_err(|error| error.to_string())? {
            return Err(format!(
                "owned daemon exited: {exit}; last readiness: {}",
                self.last_status
            ));
        }
        Ok(())
    }

    fn check_deadline(&self) -> Result<(), String> {
        if Instant::now() >= self.deadline {
            return Err(format!(
                "readiness deadline expired; last readiness: {}",
                self.last_status
            ));
        }
        Ok(())
    }

    pub(crate) fn poll(
        &mut self,
        child: &mut Child,
        socket: &Path,
        status: &mut impl FnMut(Instant) -> Result<Output, ProcessFailure>,
    ) -> Result<bool, String> {
        self.check_child(child)?;
        self.check_deadline()?;
        // A stale socket can remain after SIGKILL. Only an actual nonblocking
        // connection permits Status: its missing-socket fallback probes the
        // inventory flock and could compete with the daemon's acquisition.
        if !listener_connectable(socket)
            .map_err(|error| format!("cannot probe fixture listener: {error}"))?
        {
            return Ok(false);
        }
        self.check_deadline()?;
        let output = match status(self.deadline) {
            Ok(output) => output,
            Err(error) => {
                self.last_status = format!("readiness query failed: {error:?}");
                self.check_child(child)?;
                return Err(self.last_status.clone());
            }
        };
        let response = serde_json::from_slice::<Value>(&output.stdout);
        self.last_status = format!(
            "exit={:?}, stdout={}, stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(4096)]),
            String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(4096)])
        );
        self.check_child(child)?;
        self.check_deadline()?;
        Ok(output.status.success() && response.is_ok_and(|response| serving_status(&response)))
    }
}

fn listener_connectable(path: &Path) -> io::Result<bool> {
    let address = SocketAddr::from_pathname(path)?;
    let bytes = address
        .as_pathname()
        .expect("pathname address")
        .as_os_str()
        .as_encoded_bytes();
    let mut raw: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    raw.sun_family = libc::AF_UNIX as _;
    for (target, byte) in raw.sun_path.iter_mut().zip(bytes) {
        *target = *byte as _;
    }
    // SAFETY: socket returns a new nonblocking, close-on-exec descriptor.
    let descriptor = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: transfer the new descriptor exactly once into an owning handle.
    let socket = unsafe { OwnedFd::from_raw_fd(descriptor) };
    // SAFETY: raw holds the validated pathname and remains live for this call.
    let result = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&raw as *const libc::sockaddr_un).cast(),
            (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as _,
        )
    };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::Interrupted
    ) || matches!(
        error.raw_os_error(),
        Some(libc::EINPROGRESS) | Some(libc::EALREADY)
    ) {
        Ok(false)
    } else {
        Err(error)
    }
}
