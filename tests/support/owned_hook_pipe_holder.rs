use std::{
    fs,
    io::{Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::Value;

pub(super) struct Holder {
    child: Child,
    control: Option<ChildStdin>,
    output: Option<ChildStdout>,
    cookie: String,
    holding: Option<Value>,
    completion: Option<Result<(), String>>,
    rescue_used: bool,
    #[cfg(target_os = "linux")]
    pidfd: Option<std::os::fd::OwnedFd>,
}

impl Holder {
    pub(super) fn start(root: &Path, hook: &Path) -> Result<Self, String> {
        use std::os::unix::fs::PermissionsExt;

        let cookie = uuid::Uuid::new_v4().to_string();
        let address = root.join("hook-writers.sock");
        let script = include_str!("owned_hook_pipe_holder.py")
            .replace(
                "__ADDRESS_JSON__",
                &serde_json::to_string(&address).map_err(|error| error.to_string())?,
            )
            .replace("__COOKIE_JSON__", &format!("{cookie:?}"));
        let source = root.join("owned-hook-holder.py");
        fs::write(&source, &script).map_err(|error| error.to_string())?;
        fs::write(hook, script).map_err(|error| error.to_string())?;
        fs::set_permissions(hook, fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
        let mut child = Command::new("/usr/bin/python3")
            .args(["-I", "-B", "-u"])
            .arg(source)
            .arg("--holder")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| error.to_string())?;
        // Establish the guard before any fallible operation after spawn.
        let mut holder = Self {
            control: child.stdin.take(),
            output: child.stdout.take(),
            child,
            cookie,
            holding: None,
            completion: None,
            rescue_used: false,
            #[cfg(target_os = "linux")]
            pidfd: None,
        };
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd;
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, holder.child.id(), 0) };
            if fd < 0 {
                return Err(format!(
                    "original holder pidfd: {}",
                    std::io::Error::last_os_error()
                ));
            }
            holder.pidfd = Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) });
        }
        let ready = holder.receipt(Duration::from_secs(5))?;
        if ready["state"] != "ready" {
            return Err(format!("holder did not become ready: {ready}"));
        }
        holder.alive()?;
        Ok(holder)
    }

    fn receipt(&mut self, budget: Duration) -> Result<Value, String> {
        let output = self.output.as_mut().ok_or("missing original status pipe")?;
        let deadline = Instant::now() + budget;
        let mut line = Vec::new();
        while line.len() < 4096 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("original holder status deadline expired".into());
            }
            let mut descriptor = libc::pollfd {
                fd: output.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe {
                libc::poll(
                    &mut descriptor,
                    1,
                    remaining.as_millis().max(1).min(i32::MAX as u128) as i32,
                )
            };
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.to_string());
            }
            if result == 0 {
                return Err("original holder status deadline expired".into());
            }
            let mut byte = [0];
            output
                .read_exact(&mut byte)
                .map_err(|error| error.to_string())?;
            if byte[0] == b'\n' {
                let value: Value =
                    serde_json::from_slice(&line).map_err(|error| error.to_string())?;
                if value["cookie"] != self.cookie {
                    return Err("wrong original holder receipt cookie".into());
                }
                return Ok(value);
            }
            line.push(byte[0]);
        }
        Err("oversized original holder receipt".into())
    }

    fn alive(&mut self) -> Result<(), String> {
        if self
            .child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err("original holder exited before the product oracle".into());
        }
        Ok(())
    }

    pub(super) fn holding(&mut self) -> Result<(), String> {
        let receipt = self.receipt(Duration::from_secs(5))?;
        self.alive()?;
        if receipt["state"] != "holding"
            || receipt["writers"]
                .as_array()
                .is_none_or(|writers| writers.len() != 2)
            || receipt["holder_group"] == receipt["hook_group"]
        {
            return Err(format!("actual hook writer handoff missing: {receipt}"));
        }
        self.holding = Some(receipt);
        Ok(())
    }

    pub(super) fn snapshot(&mut self) -> Result<(), String> {
        self.alive()?;
        writeln!(
            self.control
                .as_mut()
                .ok_or("missing original control pipe")?,
            "snapshot {}",
            self.cookie
        )
        .map_err(|error| error.to_string())?;
        let receipt = self.receipt(Duration::from_secs(5))?;
        self.alive()?;
        if self.holding.as_ref() != Some(&receipt) {
            return Err("original holder no longer owns the same two capture writers".into());
        }
        Ok(())
    }

    fn exited(&mut self, budget: Duration) -> Result<bool, String> {
        #[cfg(target_os = "linux")]
        if let Some(fd) = &self.pidfd {
            let deadline = Instant::now() + budget;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let mut descriptor = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let result = unsafe {
                    libc::poll(
                        &mut descriptor,
                        1,
                        remaining.as_millis().min(i32::MAX as u128) as i32,
                    )
                };
                if result >= 0 {
                    if descriptor.revents & libc::POLLNVAL != 0 {
                        return Err("original holder pidfd became invalid".into());
                    }
                    return Ok(result > 0 && descriptor.revents & libc::POLLIN != 0);
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error.to_string());
                }
                if Instant::now() >= deadline {
                    return Ok(false);
                }
            }
        }
        // Also used if pidfd capture itself failed: cooperative EOF still has
        // to settle the original unreaped Child; no numeric signal fallback.
        let deadline = Instant::now() + budget;
        loop {
            if self
                .child
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn finish(&mut self) -> Result<(), String> {
        if let Some(completion) = &self.completion {
            return completion.clone();
        }
        // EOF is observed while accepting, receiving rights and holding them.
        drop(self.control.take());
        if !self.exited(Duration::from_secs(5))? {
            #[cfg(target_os = "linux")]
            {
                let fd = self
                    .pidfd
                    .as_ref()
                    .ok_or("holder did not settle; no original pidfd")?;
                let result = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        libc::SIGKILL,
                        0,
                        0,
                    )
                };
                if result < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                {
                    return Err(std::io::Error::last_os_error().to_string());
                }
            }
            #[cfg(not(target_os = "linux"))]
            self.child.kill().map_err(|error| error.to_string())?;
            self.rescue_used = true;
            if !self.exited(Duration::from_secs(5))? {
                return Err("original holder cleanup is incomplete after owned rescue".into());
            }
        }
        let status = self.child.wait().map_err(|error| error.to_string())?;
        let completion = if self.rescue_used || !status.success() {
            Err(format!(
                "holder cleanup cannot qualify a pass: rescue={}, {status}",
                self.rescue_used
            ))
        } else {
            Ok(())
        };
        self.completion = Some(completion.clone());
        completion
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            // The explicit success path requires finish() to succeed. An
            // unwind keeps its original failure; this is no cleanup proof.
            let _ = writeln!(
                std::io::stderr().lock(),
                "owned hook holder cleanup incomplete: {error}"
            );
            // A bounded finish failure must not discard the original Child,
            // pidfd, status pipe or private fixture while it could still run.
            // This retained wait is intentionally allowed to HOLD forever;
            // late settlement cannot change the already recorded failure.
            if self.completion.is_none() {
                match self.child.wait() {
                    Ok(status) => {
                        self.completion = Some(Err(format!(
                            "original cleanup failed before retained wait: {error}; {status}"
                        )));
                    }
                    Err(wait_error) => {
                        let _ = writeln!(
                            std::io::stderr().lock(),
                            "original hook holder wait HOLD: {wait_error}"
                        );
                        loop {
                            std::thread::park();
                        }
                    }
                }
            }
        }
    }
}
