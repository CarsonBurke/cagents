//! Serialize direct daemon admission with window publication and reviewed handoff.
//! Capability bytes are trusted only inside an authenticated live slot lease.
use super::*;
use serde::{Deserialize, Serialize};
use std::os::unix::{fs::FileExt, process::CommandExt};

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

/// Own locks explicitly unlock at scope exit; inherited descriptors only close.
/// In particular, a child must never unlock its parent's shared description.
pub(crate) enum LaunchFence {
    Owned(OwnedLock),
    Inherited { file: File, descriptor: i32 },
}
impl LaunchFence {
    fn file(&self) -> &File {
        match self {
            Self::Owned(lock) => lock,
            Self::Inherited { file, .. } => file,
        }
    }
    pub(crate) fn recovery() -> io::Result<Self> {
        if let Some((file, descriptor)) = inherited_fence()? {
            Ok(Self::Inherited { file, descriptor })
        } else {
            recovery_launch_fence().map(Self::Owned)
        }
    }
    pub(crate) fn inherit(&self, command: &mut process::Command) -> io::Result<()> {
        let inherited = match self {
            Self::Inherited { descriptor, .. } => Some(*descriptor),
            Self::Owned(_) => None,
        };
        forward_fence(command, self.file().try_clone()?, inherited)
    }
}

fn fence_path() -> io::Result<PathBuf> {
    Ok(runtime_directory()?.join(format!("{APPLICATION_LOCK_FILE}.update")))
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

pub(crate) fn has_inherited_daemon_fence() -> io::Result<bool> {
    Ok(inherited_fence()?.is_some())
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
    _launch: LaunchFence,
    _legacy: OwnedLock,
}
impl DaemonAdmissionGuard {
    pub(crate) fn acquire() -> io::Result<Self> {
        let launch = if let Some((file, descriptor)) = inherited_fence()? {
            upgrade_fence(&file)?;
            // The clone keeps the description alive. Close this process's raw
            // reserved FD too, so daemon lifetime cannot extend the fence.
            unsafe {
                libc::close(descriptor);
            }
            LaunchFence::Inherited { file, descriptor }
        } else {
            LaunchFence::Owned(owned_lock(&fence_path()?, libc::LOCK_EX | libc::LOCK_NB)?)
        };
        let runtime = runtime_directory()?;
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
            "daemon launch fence upgrade failed; recovery cancelled",
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
        publish_capability(&mut lease.0).unwrap();
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
        let borrowed = LaunchFence::Inherited {
            file: parent.try_clone().unwrap(),
            descriptor: DAEMON_FENCE_FD,
        };
        upgrade_fence(borrowed.file()).unwrap();
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
