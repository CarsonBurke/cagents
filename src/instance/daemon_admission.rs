//! Serialize direct daemon admission with window publication and reviewed handoff.
//! Capability bytes are trusted only inside an authenticated live slot lease.
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    os::unix::{fs::FileExt, process::CommandExt},
    thread,
    time::Instant,
};

pub(crate) const DAEMON_FENCE_ENV: &str = "CAGENTS_DAEMON_LAUNCH_FENCE_FD";
const DAEMON_FENCE_FD: i32 = 198;
const MAX_CAPABILITY_BYTES: usize = 512;

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProcessIdentity {
    pid: u32,
    started_ticks: u64,
    boot_id: crate::model::BootId,
    executable_device: u64,
    executable_inode: u64,
}

impl ProcessIdentity {
    fn read(pid: u32) -> io::Result<Self> {
        let root = Path::new("/proc").join(pid.to_string());
        let stat = fs::read_to_string(root.join("stat"))?;
        let started_ticks = stat
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(19))
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| io::Error::other("window process start identity is unavailable"))?;
        let executable = fs::metadata(root.join("exe"))?;
        Ok(Self {
            pid,
            started_ticks,
            boot_id: crate::daemon::read_boot_id()?,
            executable_device: executable.dev(),
            executable_inode: executable.ino(),
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WindowCapability {
    format: u8,
    protocol: u16,
    owner: ProcessIdentity,
}

pub(super) fn publish_capability(lease: &mut File) -> io::Result<()> {
    let metadata = lease.metadata()?;
    validate_private_file(&metadata)?;
    let capability = WindowCapability {
        format: 1,
        protocol: crate::terminal_ipc::PROTOCOL_VERSION,
        owner: ProcessIdentity::read(process::id())?,
    };
    let bytes = serde_json::to_vec(&capability).map_err(io::Error::other)?;
    if bytes.len() > MAX_CAPABILITY_BYTES {
        return Err(io::Error::other("window capability exceeds its limit"));
    }
    // The caller owns both the slot EX lease and shared publication fence.
    // Old code can leave bytes behind: lifetime identity makes those stale.
    lease.set_len(0)?;
    lease.write_all_at(&bytes, 0)?;
    lease.sync_all()
}

fn validate_private_file(metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.is_file()
        || metadata.uid() != current_uid()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(io::Error::other("invalid daemon admission lease"));
    }
    Ok(())
}

fn compatible_window(window: &VerifiedWindow) -> io::Result<()> {
    if !window.still_owns_lease()? {
        return Err(io::Error::other(
            "window ownership changed during daemon admission",
        ));
    }
    let before = window.lease.metadata()?;
    validate_private_file(&before)?;
    if before.len() == 0 || before.len() > MAX_CAPABILITY_BYTES as u64 {
        return Err(io::Error::other(
            "live window has no authenticated protocol capability; close it through a reviewed handoff",
        ));
    }
    let mut bytes = vec![0; before.len() as usize];
    window.lease.read_exact_at(&mut bytes, 0)?;
    let capability: WindowCapability = serde_json::from_slice(&bytes)
        .map_err(|_| io::Error::other("live window protocol capability is invalid"))?;
    let owner = ProcessIdentity::read(window.pid)?;
    let after = window.lease.metadata()?;
    if capability.format != 1
        || capability.owner != owner
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || !window.still_owns_lease()?
        || ProcessIdentity::read(window.pid)? != owner
    {
        return Err(io::Error::other(
            "live window protocol capability does not belong to its current owner",
        ));
    }
    if capability.protocol != crate::terminal_ipc::PROTOCOL_VERSION {
        return Err(io::Error::other(format!(
            "live window requires daemon protocol {}; this daemon provides {}; finish the reviewed window handoff first",
            capability.protocol,
            crate::terminal_ipc::PROTOCOL_VERSION,
        )));
    }
    Ok(())
}

fn fence_path() -> io::Result<PathBuf> {
    Ok(fence_path_in(&runtime_directory()?))
}

fn fence_path_in(runtime: &Path) -> PathBuf {
    runtime.join(format!("{APPLICATION_LOCK_FILE}.update"))
}

/// Beside the launch fence. A dot keeps it out of the window slot namespace.
fn declaration_path_in(runtime: &Path) -> PathBuf {
    runtime.join(format!("{APPLICATION_LOCK_FILE}.update.admission"))
}

/// How long an admission waits out starting windows' momentary probes of its
/// declaration before it proceeds undeclared.
const DECLARATION_PATIENCE: Duration = Duration::from_millis(250);
/// How often a starting window checks whether an admission has finished.
const ADMISSION_POLL: Duration = Duration::from_millis(50);
/// A starting window waits for an admission as long as the daemon may take to
/// start, plus this allowance for its listener to become ready.
const ADMISSION_WAIT_ALLOWANCE: Duration = Duration::from_secs(10);

/// The exclusive launch fence, held to admit a daemon or to verify windows
/// rather than to install an update.
///
/// The fence alone cannot say why it is held, and a starting window used to
/// read every exclusive holder as an update. Each admission therefore also
/// holds a shared lock on a declaration file for as long as it holds the
/// fence. A window that finds the fence held takes that declaration
/// exclusively: if it can, no admission holds the fence, so the holder is an
/// update; if it cannot, an admission does, and the window waits for it.
/// Holders that predate the declaration still read as updates.
pub(crate) struct AdmissionFence {
    // Fields drop in order: the fence is released before its declaration, so
    // a window can never find this admission's fence undeclared.
    _fence: OwnedLock,
    _declaration: Option<OwnedLock>,
}

impl AdmissionFence {
    pub(crate) fn acquire() -> io::Result<Self> {
        Self::acquire_in(&runtime_directory()?)
    }

    pub(super) fn acquire_in(runtime: &Path) -> io::Result<Self> {
        // Declare before taking the fence: a window must never see it held by
        // this admission without the declaration.
        let declaration = declare_admission(runtime);
        let fence = owned_lock(&fence_path_in(runtime), libc::LOCK_EX | libc::LOCK_NB)?;
        Ok(Self {
            _fence: fence,
            _declaration: declaration,
        })
    }
}

/// Starting windows hold the declaration exclusively only for the moment it
/// takes them to check the fence. A declaration that is still unavailable
/// after that, or cannot be opened at all, is not worth refusing the admission
/// over: undeclared, it reads as an update, which is what every holder looked
/// like before declarations existed.
fn declare_admission(runtime: &Path) -> Option<OwnedLock> {
    let deadline = Instant::now() + DECLARATION_PATIENCE;
    loop {
        match owned_lock(&declaration_path_in(runtime), libc::LOCK_SH | libc::LOCK_NB) {
            Ok(declaration) => return Some(declaration),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    tracing::warn!("admission proceeds undeclared: a window probe outlasted it");
                    return None;
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => {
                tracing::warn!(%error, "admission proceeds undeclared");
                return None;
            }
        }
    }
}

/// Who holds the launch fence, as a starting window sees it.
enum FenceHolder {
    /// Nobody exclusive: this shared hold now admits the window.
    Nobody(OwnedLock),
    /// A starting daemon, or a check of the running windows.
    Admission,
    /// An update, or a holder too old to declare an admission.
    Update,
}

fn fence_holder(runtime: &Path) -> io::Result<FenceHolder> {
    let fence = fence_path_in(runtime);
    match owned_lock(&fence, libc::LOCK_SH | libc::LOCK_NB) {
        Ok(lock) => return Ok(FenceHolder::Nobody(lock)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
        Err(error) => return Err(error),
    }
    // Every admission keeps its declaration for as long as it holds the
    // fence. While this probe excludes them all, a held fence is an update.
    let _probe = match owned_lock(&declaration_path_in(runtime), libc::LOCK_EX | libc::LOCK_NB) {
        Ok(probe) => probe,
        // An admission, or another starting window's probe: look again.
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return Ok(FenceHolder::Admission);
        }
        // Evidence that cannot be read never turns an update into a wait.
        Err(_) => return Ok(FenceHolder::Update),
    };
    match owned_lock(&fence, libc::LOCK_SH | libc::LOCK_NB) {
        Ok(lock) => Ok(FenceHolder::Nobody(lock)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(FenceHolder::Update),
        Err(error) => Err(error),
    }
}

/// The daemon did not finish its admission within a starting window's wait.
#[derive(Debug, Error)]
pub struct DaemonStillStarting {
    waited: Duration,
}

impl std::fmt::Display for DaemonStillStarting {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the cagents daemon is still starting")?;
        // A caller that does not wait has nothing worth counting.
        if self.waited >= Duration::from_secs(1) {
            write!(formatter, " after {} seconds", self.waited.as_secs())?;
        }
        formatter.write_str(
            "; try again in a moment, or run `cagents daemon status` to see what it is doing",
        )
    }
}

/// How long a starting window waits for a daemon admission.
pub(crate) fn admission_wait_timeout() -> Duration {
    // A daemon restoring an upgrade checkpoint may take as long as its
    // inventory allows. An unreadable checkpoint fails that daemon anyway.
    crate::upgrade::handoff::startup_timeout().unwrap_or(Duration::from_secs(5))
        + ADMISSION_WAIT_ALLOWANCE
}

/// Take the launch fence shared, waiting while an admission holds it.
///
/// An update still ends the attempt at once with [`UpdateInProgress`]; an
/// admission that outlasts `timeout` ends it with [`DaemonStillStarting`].
/// `waiting` hears each check that finds an admission still holding the
/// fence, with the time waited so far.
pub(super) fn shared_launch_fence(
    runtime: &Path,
    timeout: Duration,
    waiting: &mut dyn FnMut(Duration),
) -> io::Result<OwnedLock> {
    let started = Instant::now();
    loop {
        match fence_holder(runtime)? {
            FenceHolder::Nobody(lock) => return Ok(lock),
            FenceHolder::Update => {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, UpdateInProgress));
            }
            FenceHolder::Admission => {}
        }
        let waited = started.elapsed();
        if waited >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                DaemonStillStarting { waited },
            ));
        }
        waiting(waited);
        thread::sleep(ADMISSION_POLL.min(timeout - waited));
    }
}

/// Read fdinfo on this exact description, not a PID-file claim or contention
/// on another descriptor. An inherited flock retains its original owner's PID.
fn validate_fence(file: &File) -> io::Result<()> {
    let actual = file.metadata()?;
    validate_private_file(&actual)?;
    let expected = fs::symlink_metadata(fence_path()?)?;
    if !expected.is_file() || actual.dev() != expected.dev() || actual.ino() != expected.ino() {
        return Err(io::Error::other(
            "daemon inherited a different launch fence",
        ));
    }
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    let owns = info
        .lines()
        .filter_map(|line| line.strip_prefix("lock:"))
        .any(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            fields.len() == 8
                && fields[1..3] == ["FLOCK", "ADVISORY"]
                && matches!(fields[3], "READ" | "WRITE")
                && fields[6..8] == ["0", "EOF"]
                && fields[5]
                    .rsplit(':')
                    .next()
                    .and_then(|inode| inode.parse::<u64>().ok())
                    == Some(actual.ino())
        });
    if !owns {
        return Err(io::Error::other(
            "daemon inherited an unheld launch fence description",
        ));
    }
    Ok(())
}

fn inherited_fence() -> io::Result<Option<(File, i32)>> {
    use std::os::fd::BorrowedFd;
    let descriptor = if let Some(value) = env::var_os(DAEMON_FENCE_ENV) {
        if value != "198" {
            return Err(io::Error::other("invalid daemon launch fence descriptor"));
        }
        DAEMON_FENCE_FD
    } else if env::var_os(INSTALL_GUARD_ENV).is_some() {
        validate_stage_guard()?;
        INSTALL_GUARD_FD
    } else {
        return Ok(None);
    };
    // SAFETY: F_GETFD checks existence before borrowing and cloning.
    if unsafe { libc::fcntl(descriptor, libc::F_GETFD) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let file = File::from(unsafe { BorrowedFd::borrow_raw(descriptor) }.try_clone_to_owned()?);
    validate_fence(&file)?;
    Ok(Some((file, descriptor)))
}

fn forward_fence(
    command: &mut process::Command,
    file: File,
    inherited: Option<i32>,
) -> io::Result<()> {
    let fd = file.as_raw_fd();
    // Do not overwrite an unrelated inherited resource in the reserved slot.
    if fd != DAEMON_FENCE_FD
        && inherited != Some(DAEMON_FENCE_FD)
        && unsafe { libc::fcntl(DAEMON_FENCE_FD, libc::F_GETFD) } != -1
    {
        return Err(io::Error::from_raw_os_error(libc::EBUSY));
    }
    command.env(DAEMON_FENCE_ENV, DAEMON_FENCE_FD.to_string());
    // Capture File itself so the source remains alive through spawn. This hook
    // follows descriptor sealing and restores only the authenticated authority.
    unsafe {
        command.pre_exec(move || {
            let fd = file.as_raw_fd();
            if fd != DAEMON_FENCE_FD && libc::dup2(fd, DAEMON_FENCE_FD) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(DAEMON_FENCE_FD, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

pub(crate) fn inherit_daemon_fence(command: &mut process::Command) -> io::Result<()> {
    if let Some((file, descriptor)) = inherited_fence()? {
        forward_fence(command, file, Some(descriptor))?;
    }
    Ok(())
}

/// Acquire before mutable daemon ownership/socket/store operations and keep
/// through listener readiness. No live window can publish midway through admit.
pub(crate) struct DaemonAdmissionGuard {
    _launch: DaemonLaunch,
    _legacy: OwnedLock,
}

enum DaemonLaunch {
    /// This daemon's own fence, declared as an admission while it is held.
    Owned(AdmissionFence),
    /// An installer's fence stays held after readiness for its remaining work.
    /// Only close the inherited description here: unlocking it would release
    /// the installer's fence too. It is never declared as a daemon admission.
    Inherited(File),
}

impl DaemonAdmissionGuard {
    pub(crate) fn acquire() -> io::Result<Self> {
        let runtime = runtime_directory()?;
        let launch = if let Some((file, descriptor)) = inherited_fence()? {
            upgrade_fence(&file)?;
            // The clone keeps the description alive. Close this process's raw
            // reserved FD too, so daemon lifetime cannot extend the fence.
            unsafe {
                libc::close(descriptor);
            }
            DaemonLaunch::Inherited(file)
        } else {
            DaemonLaunch::Owned(AdmissionFence::acquire_in(&runtime)?)
        };
        let legacy = admit_windows(&runtime)?;
        Ok(Self {
            _launch: launch,
            _legacy: legacy,
        })
    }
}

fn upgrade_fence(file: &File) -> io::Result<()> {
    // An SH→EX conversion is NOT atomic. On failure Linux can drop the
    // parent's SH lock: return immediately, before any startup mutation.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            "daemon launch fence upgrade failed; admission cancelled",
        ));
    }
    Ok(())
}

fn admit_windows(runtime: &Path) -> io::Result<OwnedLock> {
    let legacy = owned_lock(
        &runtime.join(APPLICATION_LOCK_FILE),
        libc::LOCK_SH | libc::LOCK_NB,
    )
    .map_err(|_| io::Error::other("an unknown legacy window owns this daemon runtime"))?;
    for window in verified_window_lease_owners_in(runtime, Path::new("/proc"))? {
        compatible_window(&window)?;
    }
    Ok(legacy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fence_wait(runtime: &Path, timeout: Duration) -> (io::Result<OwnedLock>, usize) {
        let mut checks = 0;
        let result = shared_launch_fence(runtime, timeout, &mut |_| checks += 1);
        (result, checks)
    }

    #[test]
    fn a_window_waits_out_a_starting_daemon_instead_of_reporting_an_update() {
        let runtime = tempfile::tempdir().unwrap();
        let mut admission = Some(AdmissionFence::acquire_in(runtime.path()).unwrap());
        assert!(matches!(
            fence_holder(runtime.path()).unwrap(),
            FenceHolder::Admission
        ));
        let mut checks = 0;
        let lock = shared_launch_fence(runtime.path(), Duration::from_secs(30), &mut |_| {
            checks += 1;
            // The daemon's listener becomes ready while the window waits.
            admission.take();
        })
        .expect("a finished admission admits the waiting window");
        assert_eq!(checks, 1, "the window waited for the admission once");
        // The window now holds the fence shared, as any starting window does.
        assert!(
            owned_lock(
                &fence_path_in(runtime.path()),
                libc::LOCK_EX | libc::LOCK_NB
            )
            .is_err()
        );
        drop(lock);
    }

    #[test]
    fn an_undeclared_exclusive_holder_is_still_an_update_at_once() {
        let runtime = tempfile::tempdir().unwrap();
        // The reviewed update coordinator, an installer's inherited guard and
        // admissions built before declarations all hold only the fence.
        let update = owned_lock(
            &fence_path_in(runtime.path()),
            libc::LOCK_EX | libc::LOCK_NB,
        )
        .unwrap();
        let (result, checks) = fence_wait(runtime.path(), Duration::from_secs(30));
        let error = result.err().expect("an update refuses the window");
        assert!(is_update_in_progress(&error));
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(checks, 0, "an update is reported without waiting");
        drop(update);
        assert!(fence_wait(runtime.path(), Duration::ZERO).0.is_ok());
    }

    #[test]
    fn windows_that_check_together_during_an_update_all_report_it() {
        let runtime = tempfile::tempdir().unwrap();
        let _update = owned_lock(
            &fence_path_in(runtime.path()),
            libc::LOCK_EX | libc::LOCK_NB,
        )
        .unwrap();
        // Another starting window is in the middle of its own check, so it
        // holds the declaration this window needs to rule out an admission.
        let mut other_window = Some(
            owned_lock(
                &declaration_path_in(runtime.path()),
                libc::LOCK_EX | libc::LOCK_NB,
            )
            .unwrap(),
        );
        let mut checks = 0;
        let result = shared_launch_fence(runtime.path(), Duration::from_secs(30), &mut |_| {
            checks += 1;
            other_window.take();
        });
        assert!(is_update_in_progress(&result.err().unwrap()));
        assert_eq!(checks, 1, "one more look, not the whole startup wait");
    }

    #[test]
    fn an_update_that_follows_an_admission_is_reported_as_an_update() {
        let runtime = tempfile::tempdir().unwrap();
        let mut admission = Some(AdmissionFence::acquire_in(runtime.path()).unwrap());
        let mut _update = None;
        let mut checks = 0;
        let result = shared_launch_fence(runtime.path(), Duration::from_secs(30), &mut |_| {
            checks += 1;
            // The daemon becomes ready, and an update takes the fence before
            // the waiting window looks again.
            admission.take();
            _update = Some(
                owned_lock(
                    &fence_path_in(runtime.path()),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
                .unwrap(),
            );
        });
        assert!(is_update_in_progress(&result.err().unwrap()));
        assert_eq!(checks, 1);
    }

    #[test]
    fn an_admission_that_outlasts_the_wait_ends_as_a_startup_failure_not_an_update() {
        let runtime = tempfile::tempdir().unwrap();
        let _admission = AdmissionFence::acquire_in(runtime.path()).unwrap();
        let (result, checks) = fence_wait(runtime.path(), Duration::from_millis(120));
        let error = result.err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(!is_update_in_progress(&error));
        assert!(
            error
                .get_ref()
                .is_some_and(|inner| inner.is::<DaemonStillStarting>()),
            "{error}"
        );
        assert!(error.to_string().contains("still starting"), "{error}");
        assert!(checks >= 1);
        // A caller that must not block, such as an opening editor, learns the
        // same thing at once.
        let (result, checks) = fence_wait(runtime.path(), Duration::ZERO);
        let error = result.err().unwrap();
        assert_eq!(checks, 0);
        assert!(!is_update_in_progress(&error));
        assert_eq!(
            error.to_string(),
            "the cagents daemon is still starting; try again in a moment, or run `cagents daemon status` to see what it is doing"
        );
    }

    #[test]
    fn a_daemon_that_dies_during_admission_releases_waiting_windows() {
        const HOLDER: &str = "CAGENTS_TEST_DAEMON_ADMISSION_HOLDER";
        if let Some(runtime) = env::var_os(HOLDER) {
            let runtime = PathBuf::from(runtime);
            let _admission = AdmissionFence::acquire_in(&runtime).unwrap();
            fs::write(runtime.join("holder-ready"), b"").unwrap();
            thread::sleep(Duration::from_secs(120));
            return;
        }
        /// A failed assertion must not leave the holder sleeping behind.
        struct Holder(process::Child);
        impl Drop for Holder {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let runtime = tempfile::tempdir().unwrap();
        let mut holder = Holder(
            process::Command::new(env::current_exe().unwrap())
                .args([
                    "--exact",
                    "instance::daemon_admission::tests::a_daemon_that_dies_during_admission_releases_waiting_windows",
                    "--nocapture",
                ])
                .env(HOLDER, runtime.path())
                .stdout(process::Stdio::null())
                .stderr(process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        while !runtime.path().join("holder-ready").exists() {
            if let Some(status) = holder.0.try_wait().unwrap() {
                panic!("admission holder exited before admitting: {status}");
            }
            assert!(Instant::now() < deadline, "admission holder did not start");
            thread::sleep(Duration::from_millis(10));
        }
        let mut checks = 0;
        let lock = shared_launch_fence(runtime.path(), Duration::from_secs(60), &mut |_| {
            checks += 1;
            if checks == 1 {
                holder.0.kill().unwrap();
                holder.0.wait().unwrap();
            }
        })
        .expect("a dead admission leaves neither a wait nor an update behind");
        assert_eq!(checks, 1);
        drop(lock);
    }

    #[test]
    fn production_guards_tell_daemon_startup_from_an_update() {
        const CHILD: &str = "CAGENTS_TEST_PRODUCTION_LAUNCH_GUARDS";
        if env::var_os(CHILD).is_none() {
            // The guards locate their runtime through the environment. Give
            // them a private one in a fresh process rather than mutating the
            // shared test runner's environment.
            let root = tempfile::tempdir().unwrap();
            let runtime = root.path().join("runtime");
            fs::create_dir(&runtime).unwrap();
            fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
            let output = process::Command::new(env::current_exe().unwrap())
                .args([
                    "--exact",
                    "instance::daemon_admission::tests::production_guards_tell_daemon_startup_from_an_update",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("XDG_RUNTIME_DIR", &runtime)
                .env("XDG_STATE_HOME", root.path().join("state"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env_remove(DAEMON_FENCE_ENV)
                .env_remove(INSTALL_GUARD_ENV)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated launch guard fixture failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }
        // A daemon started on its own holds the fence until it is ready.
        let mut daemon = Some(DaemonAdmissionGuard::acquire().unwrap());
        let mut checks = 0;
        let lease = WindowLease::acquire_waiting(false, None, &mut |_| {
            checks += 1;
            daemon.take();
        })
        .expect("a window opened during daemon startup opens once it is ready");
        assert_eq!(checks, 1);
        drop(lease);
        // A daemon admitted under an installer's or recovery launcher's fence
        // is not declared: that owner keeps the fence after the daemon is
        // ready, so a window opened meanwhile reports its update.
        let launcher = owned_lock(&fence_path().unwrap(), libc::LOCK_SH | libc::LOCK_NB).unwrap();
        // SAFETY: this fresh process runs only this test, so it owns FD198
        // and no other thread reads the environment while it changes.
        unsafe {
            assert_eq!(
                libc::dup2(launcher.as_raw_fd(), DAEMON_FENCE_FD),
                DAEMON_FENCE_FD
            );
            env::set_var(DAEMON_FENCE_ENV, DAEMON_FENCE_FD.to_string());
        }
        let daemon = DaemonAdmissionGuard::acquire().unwrap();
        // SAFETY: as above.
        unsafe { env::remove_var(DAEMON_FENCE_ENV) };
        let error = WindowLease::acquire_waiting(false, None, &mut |_| {
            panic!("a window must not wait out an inherited update fence")
        })
        .err()
        .unwrap();
        assert!(is_update_in_progress(&error), "{error}");
        drop(daemon);
        drop(launcher);
        // The reviewed update coordinator holds the same fence for an update.
        let update = UiUpdateGuard::acquire().unwrap();
        let error = WindowLease::acquire_waiting(true, None, &mut |_| {
            panic!("a window must not wait out an update")
        })
        .err()
        .unwrap();
        assert!(is_update_in_progress(&error), "{error}");
        drop(update);

        // Launches that wait together share one startup notice.
        #[cfg(feature = "ui")]
        {
            let sleeper = || process::Command::new("sleep").arg("30").spawn();
            show_startup_wait_notice(sleeper).unwrap();
            show_startup_wait_notice(|| panic!("a second launch shows no second notice")).unwrap();
            dismiss_startup_wait_notice();
            let mut shown = false;
            show_startup_wait_notice(|| {
                shown = true;
                sleeper()
            })
            .unwrap();
            assert!(shown, "a dismissed notice lets the next launch show one");
            dismiss_startup_wait_notice();
        }
    }

    #[test]
    fn a_window_probe_never_refuses_an_admission() {
        let runtime = tempfile::tempdir().unwrap();
        let declaration = declaration_path_in(runtime.path());
        // A starting window checks the fence while holding the declaration.
        let probe = owned_lock(&declaration, libc::LOCK_EX | libc::LOCK_NB).unwrap();
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            drop(probe);
        });
        let admission = AdmissionFence::acquire_in(runtime.path()).unwrap();
        release.join().unwrap();
        assert!(
            admission._declaration.is_some(),
            "the admission outwaited the probe"
        );
        drop(admission);

        // A probe that never ends costs only the declaration, not the
        // daemon's startup: the admission then reads as an update, as every
        // holder did before declarations.
        let stuck = owned_lock(&declaration, libc::LOCK_EX | libc::LOCK_NB).unwrap();
        let admission = AdmissionFence::acquire_in(runtime.path()).unwrap();
        assert!(admission._declaration.is_none());
        drop(stuck);
        assert!(matches!(
            fence_holder(runtime.path()).unwrap(),
            FenceHolder::Update
        ));
        drop(admission);
        assert!(matches!(
            fence_holder(runtime.path()).unwrap(),
            FenceHolder::Nobody(_)
        ));
    }

    #[test]
    fn concurrent_admissions_share_one_declaration() {
        let runtime = tempfile::tempdir().unwrap();
        let first = AdmissionFence::acquire_in(runtime.path()).unwrap();
        // The fence itself still admits one holder at a time, and a refused
        // admission withdraws its declaration with its error.
        assert_eq!(
            AdmissionFence::acquire_in(runtime.path())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(first);
        assert!(matches!(
            fence_holder(runtime.path()).unwrap(),
            FenceHolder::Nobody(_)
        ));
        // A declaration alone, as when an admission has declared but not yet
        // taken the fence, never delays a window: the fence is free.
        let _declared = declare_admission(runtime.path()).unwrap();
        assert!(matches!(
            fence_holder(runtime.path()).unwrap(),
            FenceHolder::Nobody(_)
        ));
    }

    fn replace_capability(lease: &File, capability: &WindowCapability) {
        let bytes = serde_json::to_vec(capability).unwrap();
        lease.set_len(0).unwrap();
        lease.write_all_at(&bytes, 0).unwrap();
    }

    #[test]
    fn live_capability_requires_current_owner_and_matching_protocol() {
        let runtime = tempfile::tempdir().unwrap();
        let mut lease = owned_lock(
            &runtime.path().join(slot_lock_name(PRIMARY_WINDOW)),
            libc::LOCK_EX | libc::LOCK_NB,
        )
        .unwrap();
        publish_capability(lease.file_mut()).unwrap();
        assert!(admit_windows(runtime.path()).is_ok());
        let mut capability = WindowCapability {
            format: 1,
            protocol: crate::terminal_ipc::PROTOCOL_VERSION + 1,
            owner: ProcessIdentity::read(process::id()).unwrap(),
        };
        replace_capability(&lease, &capability);
        assert!(
            admit_windows(runtime.path())
                .err()
                .unwrap()
                .to_string()
                .contains("requires daemon protocol")
        );
        capability.protocol = crate::terminal_ipc::PROTOCOL_VERSION;
        capability.owner.started_ticks += 1;
        replace_capability(&lease, &capability);
        assert!(
            admit_windows(runtime.path()).is_err(),
            "stale bytes must not authenticate a reused PID/lease"
        );
        capability.owner = ProcessIdentity::read(process::id()).unwrap();
        capability.owner.executable_inode += 1;
        replace_capability(&lease, &capability);
        assert!(
            admit_windows(runtime.path()).is_err(),
            "a different executable cannot reuse a capability"
        );
        lease.set_len(0).unwrap();
        assert!(
            admit_windows(runtime.path()).is_err(),
            "older live slot owners have unknown capability"
        );
        lease
            .write_all_at(&vec![b' '; MAX_CAPABILITY_BYTES + 1], 0)
            .unwrap();
        assert!(
            admit_windows(runtime.path()).is_err(),
            "capability reads are bounded"
        );
        drop(lease);
        assert!(
            admit_windows(runtime.path()).is_ok(),
            "stale unlocked bytes do not represent a window"
        );
    }

    #[test]
    fn legacy_window_blocks_admission_even_without_slot_metadata() {
        let runtime = tempfile::tempdir().unwrap();
        let legacy = owned_lock(
            &runtime.path().join(APPLICATION_LOCK_FILE),
            libc::LOCK_EX | libc::LOCK_NB,
        )
        .unwrap();
        assert!(admit_windows(runtime.path()).is_err());
        drop(legacy);
        assert!(admit_windows(runtime.path()).is_ok());
    }

    #[test]
    fn borrowed_fence_drop_never_unlocks_its_parent() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("launch");
        let parent = owned_lock(&path, libc::LOCK_SH | libc::LOCK_NB).unwrap();
        let borrowed = parent.try_clone().unwrap();
        upgrade_fence(&borrowed).unwrap();
        drop(borrowed);
        assert!(
            owned_lock(&path, libc::LOCK_SH | libc::LOCK_NB).is_err(),
            "the parent's description remains exclusive"
        );
        drop(parent);
        assert!(owned_lock(&path, libc::LOCK_EX | libc::LOCK_NB).is_ok());
    }

    #[test]
    fn descriptor_sealing_preserves_only_explicit_forwarded_authority() {
        const CHILD: &str = "CAGENTS_TEST_DAEMON_FENCE_FORWARDING";
        if env::var_os(CHILD).is_none() {
            // Parallel widget/socket fixtures legitimately allocate FD198.
            // Exercise reserved descriptors in a fresh sealed process rather
            // than assuming the shared test runner's descriptor table is empty.
            let mut fixture = process::Command::new(env::current_exe().unwrap());
            fixture
                .args([
                    "--exact",
                    "instance::daemon_admission::tests::descriptor_sealing_preserves_only_explicit_forwarded_authority",
                    "--nocapture",
                ])
                .env(CHILD, "1");
            // SAFETY: the hook only marks inherited descriptors CLOEXEC; no
            // authority from another parallel fixture belongs to this child.
            unsafe {
                fixture.pre_exec(crate::daemon::seal_inherited_descriptors);
            }
            let output = fixture.output().unwrap();
            assert!(
                output.status.success(),
                "isolated fence fixture failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("launch");
        let parent = owned_lock(&path, libc::LOCK_SH | libc::LOCK_NB).unwrap();
        let mut command = process::Command::new("/bin/sh");
        command.args(["-c", "cat /proc/self/fdinfo/198"]);
        // SAFETY: the production sealing hook only marks descriptors CLOEXEC.
        unsafe {
            command.pre_exec(crate::daemon::seal_inherited_descriptors);
        }
        forward_fence(&mut command, parent.try_clone().unwrap(), None).unwrap();
        let output = command.output().unwrap();
        assert!(output.status.success());
        let info = String::from_utf8(output.stdout).unwrap();
        assert!(
            info.lines().any(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                fields.len() >= 5
                    && fields[0] == "lock:"
                    && fields[2..5] == ["FLOCK", "ADVISORY", "READ"]
            }),
            "{info}"
        );
        drop(command);
        assert!(
            owned_lock(&path, libc::LOCK_EX | libc::LOCK_NB).is_err(),
            "child exit and command drop cannot unlock parent authority"
        );
        // The production refusal is intentional: forwarding must not replace
        // an unrelated descriptor merely because it uses the reserved number.
        use std::os::fd::FromRawFd;
        let unrelated = File::create(root.path().join("unrelated")).unwrap();
        // SAFETY: this sealed fixture owns its descriptor table and FD198 is
        // unused after the forwarded child exited. dup2 creates an owned copy.
        assert_eq!(
            unsafe { libc::dup2(unrelated.as_raw_fd(), DAEMON_FENCE_FD) },
            DAEMON_FENCE_FD,
        );
        // SAFETY: successful dup2 transferred ownership of this new descriptor.
        let reserved = unsafe { File::from_raw_fd(DAEMON_FENCE_FD) };
        let error = forward_fence(
            &mut process::Command::new("/bin/true"),
            parent.try_clone().unwrap(),
            None,
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBUSY));
        assert_eq!(
            reserved.metadata().unwrap().ino(),
            unrelated.metadata().unwrap().ino()
        );
    }

    #[test]
    fn failed_upgrade_cannot_be_treated_as_a_retained_shared_fence() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("launch");
        let parent = owned_lock(&path, libc::LOCK_SH | libc::LOCK_NB).unwrap();
        let competing = owned_lock(&path, libc::LOCK_SH | libc::LOCK_NB).unwrap();
        assert!(upgrade_fence(&parent).is_err());
        drop(competing);
        // Linux drops the original shared lock during a failed conversion.
        // Startup must stop on the error rather than perform protected writes.
        assert!(owned_lock(&path, libc::LOCK_EX | libc::LOCK_NB).is_ok());
    }
}
