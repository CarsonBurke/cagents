//! Setup-only authority transferred to the existing execed terminal holder.
//! No daemon thread may call the dedicated subreaper's family cleanup.
use crate::model::{OperationId, TerminalId};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) const MAX_FILES: usize = 64;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Binding {
    pub(crate) creation: OperationId,
    pub(crate) run: OperationId,
    pub(crate) terminal: TerminalId,
    pub(crate) admin_path: PathBuf,
    pub(crate) admin: (u64, u64),
    pub(crate) lease: (u64, u64),
    pub(crate) protection: Vec<(u64, u64)>,
}
#[derive(Clone, Debug)]
pub(crate) struct Authority {
    pub(crate) binding: Binding,
    pub(crate) admin: Arc<File>,
    pub(crate) lease: Arc<File>,
    pub(crate) protection: Vec<Arc<File>>,
    delivery: Arc<std::sync::atomic::AtomicU8>,
}
impl Authority {
    pub(crate) fn new(
        binding: Binding,
        admin: Arc<File>,
        lease: Arc<File>,
        protection: Vec<Arc<File>>,
    ) -> Self {
        Self {
            binding,
            admin,
            lease,
            protection,
            delivery: Arc::new(std::sync::atomic::AtomicU8::new(0)),
        }
    }
    /// Exactly one worker may cross the authority-delivery boundary. A startup
    /// that was positively cancelled before this point can never launch later.
    pub(crate) fn claim_delivery(&self) -> Result<(), String> {
        self.delivery
            .compare_exchange(
                0,
                1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| {
                "Setup startup was cancelled before authority delivery; no command was sent".into()
            })
    }
    pub(crate) fn cancel_before_delivery(&self) -> Option<NoLaunch> {
        self.delivery
            .compare_exchange(
                0,
                2,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .ok()
            .map(|_| NoLaunch(self.binding.clone()))
    }
    /// Typed Spawn order only. Started/Attached preserve their original3slots.
    pub(crate) fn files(&self) -> impl Iterator<Item = &Arc<File>> {
        std::iter::once(&self.admin)
            .chain(std::iter::once(&self.lease))
            .chain(&self.protection)
    }
}
/// A positive no-authority-delivery proof. A generic timeout or free lease
/// cannot create this; the same worker's delivery CAS is permanently cancelled.
pub(crate) struct NoLaunch(Binding);
impl NoLaunch {
    pub(crate) fn binding(&self) -> &Binding {
        &self.0
    }
}

/// Actual transferred descriptors, retained until original family proof.
/// Only the freshly execed holder constructs this after validating the wire.
pub(crate) struct Execution {
    binding: Binding,
    _admin: File,
    _lease: File,
    _protection: Vec<File>,
    admin_root: crate::fs_safety::Root,
    completed: bool,
    root: Option<i32>,
    root_status: Option<libc::c_int>,
}
fn identity(file: &File) -> io::Result<(u64, u64)> {
    let m = file.metadata()?;
    Ok((m.dev(), m.ino()))
}
impl Binding {
    pub(crate) fn matches_source(&self, source: &crate::model::TerminalSession) -> bool {
        self.terminal == source.id
            && crate::upgrade::is_worktree_setup_recipe(&source.launch)
            && source
                .launch
                .environment
                .get("CAGENTS_WORKTREE_SETUP_CREATION_ID")
                == Some(&self.creation.to_string())
            && source.launch.environment.get("CAGENTS_WORKTREE_SETUP_ID")
                == Some(&self.run.to_string())
            && self.admin_path.is_absolute()
            && self.protection.len() + 2 <= MAX_FILES
    }
}
impl Execution {
    pub(crate) fn receive(
        binding: Binding,
        descriptors: Vec<OwnedFd>,
        recovery: Option<&super::Recovery>,
    ) -> Result<Self, String> {
        if descriptors.len() != binding.protection.len() + 2
            || descriptors.len() > MAX_FILES
            || binding.protection.is_empty()
        {
            return Err("Setup Spawn descriptor counts do not match its typed authority".into());
        }
        if !recovery.is_some_and(|recovery| binding.matches_source(&recovery.source)) {
            return Err("Setup requires original registered holder source authority".into());
        }
        let mut descriptors = descriptors.into_iter().map(File::from);
        let admin = descriptors.next().expect("checked admin slot");
        let lease = descriptors.next().expect("checked lease slot");
        let protection: Vec<_> = descriptors.collect();
        if identity(&admin).map_err(|e| e.to_string())? != binding.admin
            || !admin.metadata().map_err(|e| e.to_string())?.is_dir()
            || identity(&lease).map_err(|e| e.to_string())? != binding.lease
            || !lease.metadata().map_err(|e| e.to_string())?.is_file()
            || lease.metadata().map_err(|e| e.to_string())?.uid() != unsafe { libc::geteuid() }
            || lease.metadata().map_err(|e| e.to_string())?.mode() & 0o077 != 0
        {
            return Err("Setup Spawn original directory or lease identity changed".into());
        }
        // Same inherited open description must already hold the exclusive
        // original creation flock. This cannot establish cleanup of an old run.
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("Setup execution lease is unavailable".into());
        }
        for (file, expected) in protection.iter().zip(&binding.protection) {
            if identity(file).map_err(|e| e.to_string())? != *expected {
                return Err("Setup checkout protection identity changed".into());
            }
        }
        let admin_root = crate::fs_safety::Root::open(&PathBuf::from(format!(
            "/proc/self/fd/{}/.",
            admin.as_raw_fd()
        )))
        .map_err(|e| e.to_string())?;
        crate::repository::verify_reserved_setup_holder(&admin_root, &binding)?;
        // Sole reaper, checked before PTY/root creation. An inherited setting
        // is insufficient; failing this never starts repository shell effects.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        require_sole_subreaper().map_err(|e| e.to_string())?;
        Ok(Self {
            binding,
            _admin: admin,
            _lease: lease,
            _protection: protection,
            admin_root,
            completed: false,
            root: None,
            root_status: None,
        })
    }
    pub(crate) fn set_root(&mut self, pid: i32) {
        self.root = Some(pid);
    }
    pub(crate) fn binding(&self) -> &Binding {
        &self.binding
    }
    /// Root was already reaped on natural completion. Explicit release may
    /// still own it. A bounded failed pass retains every actual descriptor and
    /// retries in this exact small holder; it never reports Exited prematurely.
    pub(crate) fn finish(&mut self) -> Option<libc::c_int> {
        if self.completed {
            return self.root_status;
        }
        loop {
            match terminate_owned_family(
                Instant::now() + Duration::from_millis(250),
                self.root,
                &mut self.root_status,
            ) {
                Ok(Some(proof)) => {
                    match crate::repository::publish_setup_family_reaped(
                        &self.admin_root,
                        &self.binding,
                        &proof,
                    ) {
                        Ok(()) => {
                            self.completed = true;
                            return self.root_status;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "Setup family reaped; original disposition publication remains pending")
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "Original setup family cleanup remains pending")
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
/// Private field: root exit, PTY drain, a free lease or daemon process census
/// cannot mint this proof. Only this dedicated sole reaper's ECHILD can.
pub(crate) struct ReapedFamily(());
fn require_sole_subreaper() -> io::Result<()> {
    let mut subreaper = 0i32;
    if unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &raw mut subreaper, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if subreaper != 1
        || std::fs::read_dir("/proc/self/task")?
            .collect::<io::Result<Vec<_>>>()?
            .len()
            != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Setup cleanup requires its dedicated single-threaded subreaper",
        ));
    }
    Ok(())
}
fn direct_children() -> io::Result<std::collections::BTreeSet<i32>> {
    let raw = std::fs::read_to_string(format!("/proc/self/task/{}/children", std::process::id()))?;
    raw.split_whitespace()
        .map(|id| {
            id.parse()
                .map_err(|_| io::Error::other("Invalid owned child identity"))
        })
        .collect()
}
fn terminate_owned_family(
    deadline: Instant,
    root: Option<i32>,
    root_status: &mut Option<libc::c_int>,
) -> io::Result<Option<ReapedFamily>> {
    require_sole_subreaper()?;
    loop {
        for pid in direct_children()? {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if fd < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                return Err(error);
            }
            let descriptor = unsafe { File::from_raw_fd(fd as i32) };
            // Sole reaper retains exited children until wait below, preventing
            // PID reuse between the direct-child snapshot and pidfd capture.
            if unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    descriptor.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            } < 0
            {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        }
        loop {
            let mut status = 0;
            match unsafe { libc::waitpid(-1, &raw mut status, libc::WNOHANG) } {
                pid if pid > 0 => {
                    if Some(pid) == root {
                        *root_status = Some(status);
                    }
                    continue;
                }
                0 => break,
                _ => match io::Error::last_os_error().raw_os_error() {
                    Some(libc::ECHILD) => return Ok(Some(ReapedFamily(()))),
                    Some(libc::EINTR) => continue,
                    _ => return Err(io::Error::last_os_error()),
                },
            }
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl Drop for Execution {
    fn drop(&mut self) {
        // Also covers every fallible step after PTY spawn and before Started.
        // This runs only in the dedicated execed owner, never the daemon.
        let _ = self.finish();
    }
}
