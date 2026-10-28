//! Read-only, capability-free projection for a window waiting on an update.
//! Status is evidence, not admission: every reopen still uses normal UI fences.
mod window;
pub use window::{Request, run};

use super::{UpdateJob, UpdatePhase, impact::Disposition};
use std::{
    fs,
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockState {
    Held,
    Free,
    Unknown,
}
impl From<io::Result<bool>> for LockState {
    fn from(value: io::Result<bool>) -> Self {
        match value {
            Ok(true) => Self::Held,
            Ok(false) => Self::Free,
            Err(_) => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
struct Job {
    id: String,
    version: Option<String>,
    phase: UpdatePhase,
    waiting: usize,
    blocked: usize,
    startup_delayed: bool,
    daemon_timeout: bool,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub installation_lock: LockState,
    pub helper_lock: LockState,
    job: Option<Job>,
}

impl Snapshot {
    pub fn unavailable() -> Self {
        Self {
            installation_lock: LockState::Unknown,
            helper_lock: LockState::Unknown,
            job: None,
        }
    }
    pub fn target(&self) -> Option<&str> {
        self.job.as_ref()?.version.as_deref()
    }
    pub fn can_open(&self) -> bool {
        self.installation_lock == LockState::Free
    }
    pub fn finished(&self) -> bool {
        self.can_open()
            && self.helper_lock == LockState::Free
            && self
                .job
                .as_ref()
                .is_some_and(|job| job.phase == UpdatePhase::Complete)
    }
    pub fn heading(&self) -> &'static str {
        if self.finished() {
            return "The update has completed";
        }
        if self.helper_lock == LockState::Free && self.can_open() {
            return match self.job.as_ref().map(|job| &job.phase) {
                Some(UpdatePhase::Failed) => "The update did not complete",
                Some(UpdatePhase::Cancelled) => "The update was cancelled",
                Some(_) => "Update completion is unconfirmed",
                None => "Update status is unavailable",
            };
        }
        if self.helper_lock != LockState::Held {
            return "Waiting for the update owner";
        }
        match self.job.as_ref().map(|job| &job.phase) {
            Some(UpdatePhase::Starting) => "Preparing the update",
            Some(UpdatePhase::Downloading) => "Downloading the update",
            Some(UpdatePhase::Verifying) => "Verifying the update",
            Some(UpdatePhase::AwaitingConfirmation) => "The update needs your approval",
            Some(UpdatePhase::WaitingForIdle) => "Waiting for conversations to finish",
            Some(UpdatePhase::ClosingWindows) => "Saving and closing your windows",
            Some(UpdatePhase::Installing)
                if self.job.as_ref().is_some_and(|job| job.startup_delayed) =>
            {
                "Waiting for saved sessions to restore"
            }
            Some(UpdatePhase::Installing) => "Installing cagents",
            Some(UpdatePhase::Complete) => "Finishing the update",
            Some(UpdatePhase::Failed) => "Waiting for update recovery to finish",
            Some(UpdatePhase::Cancelled) => "Finishing update cancellation",
            _ => "Checking update progress",
        }
    }
    pub fn explanation(&self) -> String {
        if self.finished() {
            return "Open your workspace to continue.".into();
        }
        if self.helper_lock == LockState::Free && self.can_open() {
            return match self.job.as_ref() {
                Some(job) if job.phase == UpdatePhase::Failed && job.daemon_timeout =>
                    "Startup took longer than allowed. Open cagents to review recovery, or check diagnostics before trying again.".into(),
                Some(job) if !job.phase.is_terminal() =>
                    "The update lease is no longer held. Its last recorded phase is not proof that the update completed. You can safely try opening cagents through the normal startup checks.".into(),
                _ => "You can try opening cagents through its normal startup checks. This does not restart the updater or discard saved work.".into(),
            };
        }
        let Some(job) = &self.job else {
            return "Update progress could not be verified. Keep this window open to check again, or try focusing an existing cagents window.".into();
        };
        if self.helper_lock != LockState::Held {
            return "The last update record could not be matched to current helper ownership. The installation is still locked; cagents will keep checking without starting another updater.".into();
        }
        if job.phase == UpdatePhase::Installing && job.startup_delayed {
            return "Restoring saved sessions is taking longer than expected. The update is waiting for startup to finish while keeping saved work protected. Progress keeps updating here; Technical details lets you check diagnostics.".into();
        }
        if job.blocked > 0 {
            return "An existing window needs attention before the update can continue. Open it to review unsaved work or a pending decision.".into();
        }
        if job.waiting > 0 {
            return format!(
                "{} conversation{} still need{} to become idle. You can keep working in an existing window; no conversation will be interrupted from here.",
                job.waiting,
                if job.waiting == 1 { "" } else { "s" },
                if job.waiting == 1 { "s" } else { "" }
            );
        }
        match job.phase {
            UpdatePhase::AwaitingConfirmation => "Open an existing window to review the update and approve its current impact. This window cannot approve changes on your behalf.".into(),
            UpdatePhase::ClosingWindows => "The update is waiting for windows to save and close. Drafts and their existing save guards remain in place.".into(),
            UpdatePhase::Installing => "Keep this window open. It follows the update and opens your requested workspace once the same operation has completed.".into(),
            _ => "Progress updates automatically. No additional updater or daemon will be started while the installation is locked.".into(),
        }
    }
    pub fn technical_summary(&self) -> String {
        format!(
            "Installation: {}. Helper: {}. Recorded phase: {}.",
            lock_label(self.installation_lock),
            lock_label(self.helper_lock),
            self.job
                .as_ref()
                .map(|job| phase_label(&job.phase))
                .unwrap_or("unavailable")
        )
    }
}
fn lock_label(lock: LockState) -> &'static str {
    match lock {
        LockState::Held => "locked",
        LockState::Free => "released",
        LockState::Unknown => "unverified",
    }
}
fn phase_label(phase: &UpdatePhase) -> &'static str {
    match phase {
        UpdatePhase::Starting => "preparing",
        UpdatePhase::Downloading => "downloading",
        UpdatePhase::Verifying => "verifying",
        UpdatePhase::AwaitingConfirmation => "awaiting approval",
        UpdatePhase::WaitingForIdle => "waiting for idle",
        UpdatePhase::ClosingWindows => "saving windows",
        UpdatePhase::Installing => "installing",
        UpdatePhase::Complete => "complete",
        UpdatePhase::Failed => "failed",
        UpdatePhase::Cancelled => "cancelled",
    }
}

/// Only a transition of the SAME observed leased operation can auto-open.
/// A stale Complete record, missing helper, or unverified lock never can.
#[derive(Default)]
pub struct Follow {
    active: Option<String>,
    operation_changed: bool,
    auto_opened: bool,
}
impl Follow {
    fn mark_opened(&mut self) {
        self.auto_opened = true;
    }
    pub fn observe(&mut self, snapshot: &Snapshot) -> bool {
        self.observe_when_ready(snapshot, true)
    }
    fn observe_when_ready(&mut self, snapshot: &Snapshot, ready: bool) -> bool {
        // Focus and diagnostics may defer reopening, but cannot hide an
        // observed operation change or the first leased operation.
        if self.operation_changed {
            return false;
        }
        if let Some(job) = &snapshot.job {
            if self.active.as_deref().is_some_and(|id| id != job.id) {
                // The waiting window follows its first observed leased job.
                // A replacement permanently requires explicit Open, even if
                // the original job later reappears in retained status.
                self.operation_changed = true;
                return false;
            }
            if self.active.is_none()
                && snapshot.helper_lock == LockState::Held
                && !job.phase.is_terminal()
            {
                self.active = Some(job.id.clone());
            }
            if ready
                && snapshot.finished()
                && self.active.as_deref() == Some(&job.id)
                && !self.auto_opened
            {
                self.auto_opened = true;
                return true;
            }
        }
        false
    }
}

pub fn observe() -> Snapshot {
    let installation_lock = crate::instance::update_fence_held().into();
    let Ok(executable) = std::env::current_exe() else {
        return Snapshot {
            installation_lock,
            ..Snapshot::unavailable()
        };
    };
    observe_installation(&executable, installation_lock)
}
fn observe_installation(executable: &Path, installation_lock: LockState) -> Snapshot {
    let mut snapshot = Snapshot {
        installation_lock,
        ..Snapshot::unavailable()
    };
    let Ok(layout) = super::locate(executable) else {
        return snapshot;
    };
    let state = layout.prefix.join(".update");
    let Ok(metadata) = fs::symlink_metadata(&state) else {
        return snapshot;
    };
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return snapshot;
    }
    snapshot.helper_lock = crate::instance::private_lock_is_held(&state.join("lock")).into();
    if let Ok(job) = read_job(&state.join("status.json")) {
        let valid_id = uuid::Uuid::parse_str(&job.id)
            .ok()
            .is_some_and(|id| id.to_string() == job.id);
        if valid_id {
            let daemon = job
                .active_confirmation()
                .and_then(|confirmation| confirmation.preview.daemon.as_ref());
            snapshot.job = Some(Job {
                id: job.id.clone(),
                version: (job.version.len() <= 128)
                    .then(|| semver::Version::parse(&job.version).ok())
                    .flatten()
                    .map(|version| version.to_string()),
                phase: job.phase.clone(),
                waiting: daemon.map_or(0, |daemon| {
                    daemon
                        .sessions
                        .iter()
                        .filter(|session| session.disposition == Disposition::WaitsForIdle)
                        .count()
                }),
                blocked: daemon.map_or(0, |daemon| {
                    daemon.blockers.len()
                        + daemon
                            .sessions
                            .iter()
                            .filter(|session| session.disposition == Disposition::Blocked)
                            .count()
                }),
                startup_delayed: job.phase == UpdatePhase::Installing
                    && job.message == super::installed::WAITING_FOR_STARTUP,
                daemon_timeout: job.message.contains("daemon did not answer within")
                    || job.message.contains("daemon startup timed out")
                    || job
                        .message
                        .contains(crate::install_guard::DAEMON_START_BUDGET_EXCEEDED),
            });
        }
    }
    snapshot
}
fn read_job(path: &Path) -> Result<UpdateJob, ()> {
    const LIMIT: u64 = 4 * 1024 * 1024;
    let safe = |m: &fs::Metadata| {
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && m.nlink() == 1
            && m.len() <= LIMIT
    };
    let named = fs::symlink_metadata(path).map_err(|_| ())?;
    if !safe(&named) {
        return Err(());
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| ())?;
    let first = file.metadata().map_err(|_| ())?;
    let identity = |m: &fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    if !safe(&first) || identity(&first) != identity(&named) {
        return Err(());
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    let current = fs::symlink_metadata(path).map_err(|_| ())?;
    if bytes.len() as u64 > LIMIT
        || !safe(&current)
        || identity(&first) != identity(&file.metadata().map_err(|_| ())?)
        || identity(&first) != identity(&current)
    {
        return Err(());
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}

#[cfg(test)]
mod tests;
