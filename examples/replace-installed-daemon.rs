//! Reviewed maintenance path for replacing a same-protocol installed daemon.
//! Requires explicit process identity and hashes of a reviewed matched install.
//! Never run `cagentsd --version`: that executable starts a daemon.
//! Build with `--release --no-default-features`; debug builds use a separate
//! development namespace and must never perform this production maintenance.
//! Waiting expires without closing windows. A failed final recheck reopens
//! saved windows; a failed replacement start also reopens the matched target UI
//! so it can retry ordinary checkpoint recovery. No forced interruption or
//! old-daemon rollback occurs. An older host's parent-only atomic activity fence
//! is supplemented with full delegated-work snapshots immediately beforehand;
//! this cannot retrofit a stronger atomic activity fence into that old host.
use cagents::{
    control::ControlClient,
    instance::UiUpdateGuard,
    update::impact::{DaemonImpact, Policy},
};
use clap::Parser;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(about = "Replace one exact installed daemon after all non-durable work becomes idle")]
struct Args {
    #[arg(long)]
    old_pid: u32,
    /// Kernel start ticks from field 22 of /proc/PID/stat.
    #[arg(long)]
    old_start_ticks: u64,
    #[arg(long)]
    old_version_root: PathBuf,
    #[arg(long)]
    target_version_root: PathBuf,
    #[arg(long)]
    cli_sha256: String,
    #[arg(long)]
    ui_sha256: String,
    #[arg(long)]
    daemon_sha256: String,
    /// Maximum idle wait; UI windows remain open throughout this wait.
    #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..=1800))]
    wait_seconds: u64,
    /// Print one read-only impact preview as JSON, including blockers, and exit.
    #[arg(long)]
    preview_only: bool,
}

fn fail(message: impl Into<String>) -> Box<dyn std::error::Error> {
    io::Error::other(message.into()).into()
}
fn version(root: &Path) -> Result<String> {
    root.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .ok_or_else(|| fail("version root has no version name"))
}
fn root(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() || fs::canonicalize(path)? != path {
        return Err(fail(
            "version root must be an absolute canonical directory, never current or another symlink",
        ));
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(fail(
            "version root must be an owned directory without shared write permission",
        ));
    }
    if path
        .parent()
        .and_then(Path::file_name)
        .is_none_or(|s| s != "versions")
    {
        return Err(fail(
            "expected an immutable installation versions directory",
        ));
    }
    Ok(path.to_owned())
}
fn hash_file(path: &Path) -> Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(fail(
            "candidate binaries must be owned regular files without shared write permission",
        ));
    }
    let mut file = file;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn verify_target(root: &Path, pins: &[(&str, &str)]) -> Result<()> {
    let bin = root.join("bin");
    let metadata = fs::symlink_metadata(&bin)?;
    if !metadata.is_dir()
        || fs::canonicalize(&bin)? != bin
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(fail(
            "target bin must be a canonical owned directory without shared write permission",
        ));
    }
    for &(name, expected) in pins {
        if expected.len() != 64
            || !expected.bytes().all(|b| b.is_ascii_hexdigit())
            || hash_file(&root.join("bin").join(name))? != expected.to_ascii_lowercase()
        {
            return Err(fail(format!("reviewed hash does not match {name}")));
        }
    }
    let mut command = Command::new(root.join("bin/cagents"));
    command.arg("--version");
    let output = bounded(command, Duration::from_secs(10))?;
    if String::from_utf8(output)?.trim() != format!("cagents {}", version(root)?) {
        return Err(fail(
            "target CLI did not report the configured installation version",
        ));
    }
    let mut command = Command::new(root.join("bin/cagents"));
    command.args(["daemon", "protocol-version"]);
    if String::from_utf8(bounded(command, Duration::from_secs(10))?)?.trim()
        != cagents::terminal_ipc::PROTOCOL_VERSION.to_string()
    {
        return Err(fail(
            "target does not share this maintenance client's protocol",
        ));
    }
    Ok(())
}
fn start_ticks(pid: u32) -> Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    stat.rsplit_once(')')
        .and_then(|(_, tail)| tail.split_whitespace().nth(19))
        .ok_or_else(|| fail("invalid process stat"))?
        .parse()
        .map_err(Into::into)
}
fn verify_process(pid: u32, ticks: u64, executable: &Path) -> Result<()> {
    if start_ticks(pid)? != ticks
        || fs::read_link(format!("/proc/{pid}/exe"))? != executable
        || start_ticks(pid)? != ticks
    {
        return Err(fail(
            "the exact expected daemon process changed; no replacement authorized",
        ));
    }
    Ok(())
}
fn socket_pid(socket: &Path) -> Result<u32> {
    let stream = UnixStream::connect(socket)?;
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes only into this initialized live credentials buffer.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error().into());
    }
    if length as usize != std::mem::size_of::<libc::ucred>()
        || credentials.uid != unsafe { libc::getuid() }
        || credentials.pid <= 0
    {
        return Err(fail("daemon socket has invalid peer credentials"));
    }
    Ok(credentials.pid as u32)
}
fn verify_old(client: &mut ControlClient, socket: &Path, args: &Args) -> Result<()> {
    if socket_pid(socket)? != args.old_pid
        || client.daemon_version()? != version(&args.old_version_root)?
    {
        return Err(fail(
            "daemon socket or build changed; no replacement authorized",
        ));
    }
    verify_process(
        args.old_pid,
        args.old_start_ticks,
        &args.old_version_root.join("bin/cagentsd"),
    )
}

/// Capture bounded output without blocking on inherited descendant pipes.
fn bounded(mut command: Command, timeout: Duration) -> Result<Vec<u8>> {
    let mut output = tempfile::tempfile_in("/var/tmp")?;
    let mut errors = tempfile::tempfile_in("/var/tmp")?;
    command
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(errors.try_clone()?)
        .process_group(0);
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            // SAFETY: our unreaped direct child owns this new process group.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            return Err(fail("bounded maintenance command timed out"));
        }
        thread::sleep(Duration::from_millis(25));
    };
    if output.metadata()?.len() > 64 * 1024 || errors.metadata()?.len() > 64 * 1024 {
        return Err(fail("maintenance command output exceeded its bound"));
    }
    use std::io::{Seek, SeekFrom};
    errors.seek(SeekFrom::Start(0))?;
    let mut stderr = String::new();
    errors.take(64 * 1024 + 1).read_to_string(&mut stderr)?;
    if !stderr.is_empty() {
        eprintln!("Maintenance command diagnostics: {stderr}");
    }
    if !status.success() {
        return Err(fail(format!("maintenance command failed: {status}")));
    }
    output.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    output.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn check_start_report(bytes: &[u8]) -> Result<()> {
    let report: serde_json::Value = serde_json::from_slice(bytes)?;
    if report["ok"] != true
        || report["command"] != "daemon.start"
        || report["data"]["running"] != true
    {
        return Err(fail("target CLI did not confirm a successful daemon start"));
    }
    if report["data"].get("upgrade_attention").is_some() {
        return Err(fail(
            "target reports retained conversations requiring upgrade attention; review daemon start diagnostics and retained-conversation evidence",
        ));
    }
    Ok(())
}

fn lock_available(path: &Path) -> Result<bool> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(fail("unsafe daemon lease file"));
    }
    // SAFETY: flock acts on our owned descriptor; no process is signalled.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error.into())
    }
}
fn wait_stopped(socket: &Path, pid: u32, ticks: u64) -> Result<()> {
    let lock = socket.with_file_name("cagentsd.lock");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let exact_alive = start_ticks(pid).is_ok_and(|actual| actual == ticks);
        if !exact_alive && !socket.try_exists()? && lock_available(&lock)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(fail(
                "checkpointed daemon did not release its process, socket and lease within 30 seconds",
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}
fn log_impact(impact: &DaemonImpact) {
    eprintln!(
        "Observed {} sessions, {} blockers",
        impact.sessions.len(),
        impact.blockers.len()
    );
    for session in &impact.sessions {
        eprintln!(
            "  {}: {}: {}",
            session.id,
            session.disposition.label(),
            session.reason.escape_debug()
        );
    }
    for blocker in &impact.blockers {
        eprintln!("  blocker: {blocker}");
    }
}
/// Older same-protocol hosts classified activity by parent `busy` alone.
/// Attachment reads the full snapshot without submitting or replaying a prompt.
fn reconnect_only_working(client: &mut ControlClient, socket: &Path) -> Result<bool> {
    use cagents::terminal_ipc::{
        ClientEnvelope, ClientRequest, ServerEnvelope, ServerMessage, ServerResponse, read_frame,
        write_frame,
    };
    let sessions = client.list_chats()?;
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let proof = client.proof().clone();
    let mut request_id = 0;
    let mut request = |request: ClientRequest| -> Result<ServerResponse> {
        request_id += 1;
        write_frame(
            &mut stream,
            &ClientEnvelope {
                version: cagents::terminal_ipc::PROTOCOL_VERSION,
                request_id,
                proof: Some(proof.clone()),
                request,
            },
        )?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if Instant::now() >= deadline {
                return Err(fail("snapshot read deadline expired"));
            }
            let response: ServerEnvelope =
                read_frame(&mut stream)?.ok_or_else(|| fail("snapshot connection closed"))?;
            if response.proof != proof
                || response.version != cagents::terminal_ipc::PROTOCOL_VERSION
            {
                return Err(fail("snapshot host proof changed"));
            }
            if response.request_id == Some(request_id) {
                return match response.message {
                    ServerMessage::Response { response } => Ok(response),
                    _ => Err(fail("snapshot request rejected")),
                };
            }
        }
    };
    if !matches!(request(ClientRequest::Connect {
        expected_host: Some(proof.clone()),
        resume: vec![],
    })?, ServerResponse::Connected { proof: connected, .. } if connected == proof)
    {
        return Err(fail("snapshot handshake did not confirm the exact host"));
    }
    for session in sessions {
        if matches!(
            session.native_continuity,
            cagents::chat::NativeContinuity::Durable { .. }
        ) {
            continue;
        }
        match request(ClientRequest::ChatAttach { session })? {
            ServerResponse::ChatAttached { snapshot, .. } if snapshot.is_working() => {
                return Ok(true);
            }
            ServerResponse::ChatAttached { .. } => (),
            _ => return Err(fail("full native snapshot unavailable; cannot prove idle")),
        }
    }
    Ok(false)
}
fn ready_preview(client: &mut ControlClient, socket: &Path, args: &Args) -> Result<DaemonImpact> {
    let deadline = Instant::now() + Duration::from_secs(args.wait_seconds);
    let mut previous = None;
    loop {
        verify_old(client, socket, args)?;
        let impact = client.upgrade_impact(Policy::WhenIdle)?;
        let state = serde_json::to_string(&(&impact.sessions, &impact.blockers))?;
        if previous.as_ref() != Some(&state) {
            log_impact(&impact);
            previous = Some(state);
        }
        if impact.blocked() {
            return Err(fail(
                "resolve the preview blockers; no windows or daemon were stopped",
            ));
        }
        if impact.ready() && !reconnect_only_working(client, socket)? {
            return Ok(impact);
        }
        if Instant::now() >= deadline {
            return Err(fail("idle wait expired; no windows or daemon were stopped"));
        }
        thread::sleep(Duration::from_secs(1));
    }
}
fn main() -> Result<()> {
    let mut args = Args::parse();
    if cfg!(any(debug_assertions, feature = "development")) {
        return Err(fail(
            "production maintenance requires a release build without the development feature",
        ));
    }
    args.old_version_root = root(&args.old_version_root)?;
    args.target_version_root = root(&args.target_version_root)?;
    if args.old_version_root.parent() != args.target_version_root.parent()
        || args.old_version_root == args.target_version_root
    {
        return Err(fail(
            "replacement must be a distinct reviewed version of the same installation",
        ));
    }
    let pins = [
        ("cagents", args.cli_sha256.as_str()),
        ("cagents-ui", args.ui_sha256.as_str()),
        ("cagentsd", args.daemon_sha256.as_str()),
    ];
    verify_target(&args.target_version_root, &pins)?;
    let socket = cagents::terminal_ipc::daemon_socket_path()?;
    let mut client = ControlClient::connect(&socket)?;
    if args.preview_only {
        verify_old(&mut client, &socket, &args)?;
        let impact = client.upgrade_impact(Policy::WhenIdle)?;
        println!("{}", serde_json::to_string_pretty(&impact)?);
        return Ok(());
    }
    let proof = client.proof().clone();
    let approved = ready_preview(&mut client, &socket, &args)?;
    eprintln!("Idle preview ready; saving windows before final fenced recheck");
    let mut guard = UiUpdateGuard::acquire()?;
    // The target matched UI supports this daemon's protocol and carries the
    // persistence fix. If any window committed a close, Drop reopens that slot.
    guard.set_fallback_executable(args.target_version_root.join("bin/cagents-ui"));
    let windows = guard.prepare_and_close_windows(Duration::from_secs(45))?;
    let result = (|| -> Result<()> {
        verify_target(&args.target_version_root, &pins)?;
        verify_old(&mut client, &socket, &args)?;
        let current = client.upgrade_impact(Policy::WhenIdle)?;
        if client.proof() != &proof || !approved.preserves_consent(&current) || !current.ready() {
            return Err(fail(
                "approved owners or readiness changed after closing windows; no checkpoint requested",
            ));
        }
        if reconnect_only_working(&mut client, &socket)? {
            return Err(fail(
                "parent or delegated native work is still running; no checkpoint requested",
            ));
        }
        // The daemon rechecks under its control write gate and fences actors.
        // Any new busy turn or owner invalidates this request without teardown.
        client.prepare_confirmed_upgrade(current.clone(), false)?;
        drop(client);
        wait_stopped(&socket, args.old_pid, args.old_start_ticks)?;
        verify_target(&args.target_version_root, &pins)?;
        eprintln!("Exact old daemon checkpointed; starting reviewed matched target");
        let mut command = Command::new(args.target_version_root.join("bin/cagents"));
        command.args(["--json", "daemon", "start"]);
        let start_report = bounded(command, Duration::from_secs(30))?;
        let mut replacement = ControlClient::connect(&socket)?;
        let pid = socket_pid(&socket)?;
        if replacement.daemon_version()? != version(&args.target_version_root)?
            || replacement.proof() == &proof
            || fs::read_link(format!("/proc/{pid}/exe"))?
                != args.target_version_root.join("bin/cagentsd")
        {
            return Err(fail(
                "replacement daemon did not confirm the exact target build and new proof",
            ));
        }
        check_start_report(&start_report)?;
        let terminals = replacement.list()?;
        let chats = replacement.list_chats()?;
        for session in &current.sessions {
            let found = match session.kind {
                cagents::update::impact::SessionKind::Terminal => {
                    terminals.iter().any(|s| s.session.id == session.id)
                }
                cagents::update::impact::SessionKind::NativeChat => {
                    chats.iter().any(|s| s.id == session.id)
                }
            };
            if !found {
                return Err(fail(format!(
                    "retained {:?} {} is absent from replacement inventory; review restore evidence",
                    session.kind, session.id
                )));
            }
        }
        eprintln!("Replacement daemon verified; reopening saved window slots");
        Ok(())
    })();
    let reopened =
        guard.relaunch_windows(&windows, &args.target_version_root.join("bin/cagents-ui"));
    match (result, reopened) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Err(reopen)) => Err(fail(format!(
            "{error}; reopening saved windows also failed: {reopen}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_running_daemon_with_partial_restore_is_not_success() {
        assert!(
            check_start_report(br#"{"ok":true,"command":"daemon.start","data":{"running":true}}"#)
                .is_ok()
        );
        assert!(check_start_report(br#"{"ok":true,"command":"daemon.start","data":{"running":true,"upgrade_attention":{"unattached_conversations":[{"id":"lost"}]}}}"#).is_err());
        assert!(
            check_start_report(br#"{"ok":true,"command":"daemon.status","data":{"running":true}}"#)
                .is_err()
        );
        assert!(check_start_report(b"unexpected output").is_err());
    }
    #[test]
    fn pid_reuse_or_wrong_executable_cannot_authorize_replacement() {
        let pid = std::process::id();
        let ticks = start_ticks(pid).unwrap();
        let executable = fs::read_link(format!("/proc/{pid}/exe")).unwrap();
        verify_process(pid, ticks, &executable).unwrap();
        assert!(verify_process(pid, ticks + 1, &executable).is_err());
        assert!(verify_process(pid, ticks, Path::new("/different/cagentsd")).is_err());
    }
    #[test]
    fn lease_probe_does_not_release_an_existing_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lease");
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(!lock_available(&path).unwrap());
        drop(file);
        assert!(lock_available(&path).unwrap());
    }
    #[test]
    fn maintenance_timeout_is_bounded_and_success_output_is_captured() {
        let mut sleepy = Command::new("/bin/sleep");
        sleepy.arg("30");
        let start = Instant::now();
        assert!(bounded(sleepy, Duration::from_millis(30)).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        let mut echo = Command::new("/bin/echo");
        echo.arg("verified");
        assert_eq!(
            bounded(echo, Duration::from_secs(1)).unwrap(),
            b"verified\n"
        );
    }
}
