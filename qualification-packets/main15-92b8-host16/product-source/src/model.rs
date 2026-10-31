use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ffi::{CStr, CString},
    fmt,
    mem::MaybeUninit,
    path::{Path, PathBuf},
    ptr,
    str::FromStr,
};

use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub const fn from_uuid(id: Uuid) -> Self {
                Self(id)
            }

            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }

        impl From<$name> for Uuid {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

id_type!(WorkspaceId);
id_type!(CardId);
id_type!(ProjectId);
id_type!(WorktreeId);
id_type!(ChannelId);
id_type!(TerminalId);
id_type!(BrowserId);
id_type!(OperationId);
id_type!(AgentId);
id_type!(RunId);
id_type!(MessageId);

macro_rules! evidence_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

evidence_type!(BootId);
evidence_type!(HostSessionId);
evidence_type!(HostIncarnation);

/// Durable card existence. Closing and closed cards remain durable so an
/// interrupted close cannot resurrect a card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Existence {
    Open,
    Closing { operation_id: OperationId },
    Closed { tombstone: Tombstone },
}

impl Existence {
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub operation_id: Option<OperationId>,
    pub closed_at_unix_ms: i64,
    pub reason: Option<String>,
}

/// Volatile viewport placement. This is deliberately not written to durable
/// storage; column order, width, and focus are the persisted layout intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exposure {
    Visible,
    Prefetch,
    Offscreen,
}

/// A typed runtime failure which is safe to present and persist in diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeError {
    pub code: String,
    pub message: String,
}

/// Volatile resource state. It is orthogonal to existence and exposure: an
/// open offscreen card can be hot, hibernated, hydrating, or failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Residency {
    Cold,
    Hydrating {
        epoch: u64,
    },
    Hot,
    Evicting {
        epoch: u64,
    },
    Hibernated,
    Failed {
        typed_error: RuntimeError,
        retryable: bool,
    },
}

impl Residency {
    pub fn is_renderable(&self) -> bool {
        matches!(self, Self::Hot)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardState {
    pub id: CardId,
    pub generation: u64,
    pub title: String,
    pub width_fraction: f32,
    pub existence: Existence,
    pub exposure: Exposure,
    pub residency: Residency,
    pub kind: CardKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CardKind {
    Terminal(TerminalSession),
    Chat(crate::chat::Session),
    Browser(BrowserSession),
    PullRequests(PullRequestView),
    Repository(RepositoryView),
    /// The window's settings pane. Its content is the application's global
    /// settings, so the card itself carries no payload.
    Settings,
    /// The window's account usage pane. Like settings it shows global state
    /// and carries no payload.
    Accounts,
    /// Daemon-owned schedules and their durable execution history.
    Automations,
    /// The daemon's Work board of every worktree and branch in progress.
    Work,
}

impl CardKind {
    /// Whether the card is one of the window's singleton utility panes, which
    /// show application state rather than a session, file or page, so a
    /// window holds at most one of each and they never hibernate.
    pub fn is_utility(&self) -> bool {
        matches!(
            self,
            Self::Settings | Self::Accounts | Self::Automations | Self::Work
        )
    }
}

/// An argv-based launch recipe. `program` is never interpreted by a shell and
/// every argument and environment entry remains a separate value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchRecipe {
    pub program: String,
    pub arguments: Vec<String>,
    pub working_directory: PathBuf,
    pub environment: BTreeMap<String, String>,
    /// Removed after every inherited and explicit environment layer. Names only;
    /// selected accounts must survive daemon restarts without saving credentials.
    #[serde(default)]
    pub unset_environment: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LoginShellError {
    #[error(
        "no usable login shell was found in the effective user's passwd entry, SHELL, or /bin/sh"
    )]
    Unavailable,
}

impl LaunchRecipe {
    pub fn shell(working_directory: PathBuf) -> Result<Self, LoginShellError> {
        Ok(Self {
            program: login_shell()?,
            arguments: Vec::new(),
            working_directory,
            environment: BTreeMap::new(),
            unset_environment: BTreeSet::new(),
        })
    }

    #[cfg(any(feature = "host", feature = "ui"))]
    /// Apply explicit removals and the equivalent policy for legacy managed homes.
    pub(crate) fn remove_unset_environment(&self, environment: &mut BTreeMap<String, String>) {
        let names = crate::account::launch_credential_overrides(self);
        environment.retain(|name, _| !names.contains(name));
    }
}

/// The shell a new terminal runs: the passwd entry's, else `SHELL`, else
/// `/bin/sh`.
pub(crate) fn login_shell() -> Result<String, LoginShellError> {
    choose_login_shell(
        passwd_shell(),
        std::env::var_os("SHELL").and_then(|shell| shell.into_string().ok()),
        is_valid_shell,
    )
}

fn choose_login_shell(
    passwd: Option<String>,
    environment: Option<String>,
    is_valid: impl Fn(&str) -> bool,
) -> Result<String, LoginShellError> {
    if let Some(shell) = passwd.filter(|shell| is_valid(shell)) {
        return Ok(shell);
    }
    if let Some(shell) = environment.filter(|shell| is_valid(shell)) {
        return Ok(shell);
    }
    if is_valid("/bin/sh") {
        return Ok("/bin/sh".to_owned());
    }
    Err(LoginShellError::Unavailable)
}

fn passwd_shell() -> Option<String> {
    const INITIAL_BUFFER_SIZE: usize = 1_024;
    const MAX_BUFFER_SIZE: usize = 1024 * 1024;

    // SAFETY: sysconf and geteuid have no pointer arguments or preconditions.
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let size = usize::try_from(suggested)
        .ok()
        .filter(|size| *size > 0)
        .unwrap_or(INITIAL_BUFFER_SIZE)
        .clamp(INITIAL_BUFFER_SIZE, MAX_BUFFER_SIZE);
    let mut buffer = vec![0_u8; size];

    loop {
        let mut entry = MaybeUninit::<libc::passwd>::zeroed();
        let mut result = ptr::null_mut();
        // SAFETY: entry and result are valid writable pointers, and buffer
        // describes exactly `buffer.len()` writable bytes for the call.
        let status = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer.len() < MAX_BUFFER_SIZE {
            buffer.resize(buffer.len().saturating_mul(2).min(MAX_BUFFER_SIZE), 0);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }

        // SAFETY: a successful getpwuid_r initialized `result` and keeps every
        // pointed-to string alive in `buffer` until this function returns.
        let shell = unsafe { (*result).pw_shell };
        if shell.is_null() {
            return None;
        }
        // SAFETY: passwd string fields are NUL-terminated by getpwuid_r.
        return unsafe { CStr::from_ptr(shell) }
            .to_str()
            .ok()
            .map(str::to_owned);
    }
}

fn is_valid_shell(candidate: &str) -> bool {
    let path = Path::new(candidate);
    if candidate.is_empty() || !path.is_absolute() {
        return false;
    }
    fs_metadata_is_executable(path)
}

fn fs_metadata_is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    let Some(bytes) = path.to_str().map(str::as_bytes) else {
        return false;
    };
    let Ok(path) = CString::new(bytes) else {
        return false;
    };
    // SAFETY: path is a live NUL-terminated string. AT_EACCESS asks Linux to
    // check execute permission using the process's effective credentials.
    unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::X_OK, libc::AT_EACCESS) == 0 }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalExit {
    pub code: Option<i32>,
    /// The signal that ended the process, as the host describes it. The PTY
    /// layer reports a name rather than a portable number, and the name is
    /// what the card has to show.
    pub signal: Option<String>,
    pub occurred_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSessionProof {
    pub host_session_id: HostSessionId,
    pub boot_id: BootId,
    pub host_incarnation: HostIncarnation,
}

impl HostSessionProof {
    pub fn is_complete(&self) -> bool {
        !self.host_session_id.as_str().is_empty()
            && !self.boot_id.as_str().is_empty()
            && !self.host_incarnation.as_str().is_empty()
    }
}

/// Durable terminal continuity evidence. A row marked `Live` is only attached
/// as live after all three proof fields match the current host proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalContinuity {
    Live {
        host_session_id: HostSessionId,
        boot_id: BootId,
        host_incarnation: HostIncarnation,
    },
    DetachedSameBoot {
        host_session_id: HostSessionId,
        boot_id: BootId,
        host_incarnation: HostIncarnation,
    },
    RelaunchAvailable {
        recipe: LaunchRecipe,
        last_exit: Option<TerminalExit>,
    },
    Exited {
        last_exit: TerminalExit,
    },
    Failed {
        typed_error: RuntimeError,
        retryable: bool,
    },
}

impl TerminalContinuity {
    pub fn live_proof(&self) -> Option<HostSessionProof> {
        match self {
            Self::Live {
                host_session_id,
                boot_id,
                host_incarnation,
            }
            | Self::DetachedSameBoot {
                host_session_id,
                boot_id,
                host_incarnation,
            } => Some(HostSessionProof {
                host_session_id: host_session_id.clone(),
                boot_id: boot_id.clone(),
                host_incarnation: host_incarnation.clone(),
            }),
            Self::RelaunchAvailable { .. } | Self::Exited { .. } | Self::Failed { .. } => None,
        }
    }

    pub fn can_resume_live(&self, proof: &HostSessionProof) -> bool {
        if !proof.is_complete() {
            return false;
        }
        matches!(
            self,
            Self::Live {
                host_session_id,
                boot_id,
                host_incarnation,
            }
            | Self::DetachedSameBoot {
                host_session_id,
                boot_id,
                host_incarnation,
            } if host_session_id == &proof.host_session_id
                && boot_id == &proof.boot_id
                && host_incarnation == &proof.host_incarnation
        )
    }
}

/// Display-only wall time; ordering is exclusively the current worker sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalOutputActivity {
    pub sequence: u64,
    pub last_output_at_unix_ms: i64,
}

/// Quiet means only that no PTY output has been observed for five seconds.
pub const TERMINAL_QUIET_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalActivityState {
    Starting,
    OutputActive,
    Quiet,
    Exited,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalActivityLifecycle {
    Starting,
    Live,
    Exited,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalActivityTimer {
    pub terminal_id: TerminalId,
    pub proof: HostSessionProof,
    pub sequence: u64,
    pub deadline: std::time::Duration,
}

/// Ephemeral, proof-fenced presentation state, never persisted or inferred from text.
/// Consumers schedule `timer()` only while mounted and use one monotonic clock origin.
#[derive(Debug, Clone)]
pub struct TerminalActivity {
    terminal_id: TerminalId,
    proof: HostSessionProof,
    revision: u64,
    output: Option<TerminalOutputActivity>,
    output_high_water: Option<TerminalOutputActivity>,
    observed_at: std::time::Duration,
    state: TerminalActivityState,
    timer: Option<TerminalActivityTimer>,
}

impl TerminalActivity {
    pub fn new(terminal_id: TerminalId, proof: HostSessionProof) -> Self {
        Self {
            terminal_id,
            proof,
            revision: 0,
            output: None,
            output_high_water: None,
            observed_at: std::time::Duration::ZERO,
            state: TerminalActivityState::Unknown,
            timer: None,
        }
    }

    pub fn state(&self) -> TerminalActivityState {
        self.state
    }

    pub fn timer(&self) -> Option<TerminalActivityTimer> {
        self.timer.clone()
    }

    pub fn observe(
        &mut self,
        proof: &HostSessionProof,
        revision: u64,
        lifecycle: TerminalActivityLifecycle,
        output: Option<TerminalOutputActivity>,
        now: std::time::Duration,
        unix_ms: i64,
    ) -> bool {
        if proof != &self.proof || !proof.is_complete() || revision < self.revision {
            return false;
        }
        if !matches!(
            lifecycle,
            TerminalActivityLifecycle::Exited | TerminalActivityLifecycle::Failed
        ) && output
            .zip(self.output_high_water)
            .is_some_and(|(new, old)| {
                new.sequence < old.sequence || new.sequence == old.sequence && new != old
            })
        {
            return false;
        }
        let previous = self.state;
        let previous_timer = self.timer.clone();
        self.revision = revision;
        let clock_reversed = now < self.observed_at;
        self.observed_at = self.observed_at.max(now);
        // Terminal lifecycle evidence cannot be undone by late live descriptors.
        if self.state == TerminalActivityState::Failed {
            return false;
        }
        if lifecycle == TerminalActivityLifecycle::Failed {
            self.state = TerminalActivityState::Failed;
            self.timer = None;
        } else if self.state == TerminalActivityState::Exited
            || lifecycle == TerminalActivityLifecycle::Exited
        {
            self.state = TerminalActivityState::Exited;
            self.timer = None;
        } else if clock_reversed {
            self.state = TerminalActivityState::Unknown;
            self.timer = None;
        } else if lifecycle == TerminalActivityLifecycle::Starting {
            self.state = TerminalActivityState::Starting;
            self.timer = None;
        } else if lifecycle != TerminalActivityLifecycle::Live || output.is_none() {
            self.state = TerminalActivityState::Unknown;
            self.timer = None;
        } else if let Some(output) = output {
            if output.sequence == 0 || output.last_output_at_unix_ms < 0 {
                self.state = TerminalActivityState::Unknown;
                self.timer = None;
            } else if self.output != Some(output)
                || self.timer.is_none() && self.state != TerminalActivityState::Quiet
            {
                // Wall time is used only to age an initial observation; never to
                // extend a previously armed deadline after a clock adjustment.
                let age = unix_ms
                    .checked_sub(output.last_output_at_unix_ms)
                    .and_then(|age| u64::try_from(age).ok());
                if let Some(age) = age {
                    let age = std::time::Duration::from_millis(age);
                    self.state = if age >= TERMINAL_QUIET_AFTER {
                        TerminalActivityState::Quiet
                    } else {
                        TerminalActivityState::OutputActive
                    };
                    self.timer = (age < TERMINAL_QUIET_AFTER).then(|| TerminalActivityTimer {
                        terminal_id: self.terminal_id,
                        proof: self.proof.clone(),
                        sequence: output.sequence,
                        deadline: now.saturating_add(TERMINAL_QUIET_AFTER.saturating_sub(age)),
                    });
                } else {
                    self.state = TerminalActivityState::Unknown;
                    self.timer = None;
                }
            }
        }
        self.output = output;
        if let Some(output) = output
            && self
                .output_high_water
                .is_none_or(|old| output.sequence > old.sequence)
        {
            self.output_high_water = Some(output);
        }
        self.state != previous || self.timer != previous_timer
    }

    pub fn expire(&mut self, timer: &TerminalActivityTimer, now: std::time::Duration) -> bool {
        if self.timer.as_ref() != Some(timer)
            || timer.terminal_id != self.terminal_id
            || timer.proof != self.proof
            || self
                .output
                .is_none_or(|output| output.sequence != timer.sequence)
            || self.state != TerminalActivityState::OutputActive
            || now < self.observed_at
            || now < timer.deadline
        {
            return false;
        }
        self.observed_at = now;
        self.state = TerminalActivityState::Quiet;
        self.timer = None;
        true
    }
}

#[cfg(test)]
mod activity_tests {
    use super::*;
    use std::time::Duration;

    fn proof(incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new("host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new(incarnation),
        }
    }

    fn output(sequence: u64) -> Option<TerminalOutputActivity> {
        Some(TerminalOutputActivity {
            sequence,
            last_output_at_unix_ms: 10_000,
        })
    }

    #[test]
    fn activity_timers_cannot_overwrite_output_identity_or_lifecycle() {
        let proof = proof("first");
        let mut activity = TerminalActivity::new(TerminalId::new(), proof.clone());
        activity.observe(
            &proof,
            1,
            TerminalActivityLifecycle::Live,
            output(1),
            Duration::ZERO,
            10_000,
        );
        let old = activity.timer().unwrap();
        activity.observe(
            &proof,
            2,
            TerminalActivityLifecycle::Live,
            output(2),
            Duration::from_secs(1),
            10_000,
        );
        assert!(!activity.expire(&old, Duration::from_secs(10)));
        let current = activity.timer().unwrap();
        assert!(!activity.observe(
            &proof,
            1,
            TerminalActivityLifecycle::Live,
            output(1),
            Duration::from_secs(2),
            10_000
        ));
        assert!(!activity.expire(&current, Duration::ZERO));
        assert!(activity.expire(&current, Duration::from_secs(6)));
        assert_eq!(activity.state(), TerminalActivityState::Quiet);
        let mut other = TerminalActivity::new(TerminalId::new(), proof.clone());
        assert!(!other.expire(&current, Duration::from_secs(10)));
        activity.observe(
            &proof,
            3,
            TerminalActivityLifecycle::Exited,
            output(2),
            Duration::from_secs(7),
            10_000,
        );
        assert!(!activity.expire(&current, Duration::from_secs(10)));
        assert_eq!(activity.state(), TerminalActivityState::Exited);
        activity.observe(
            &proof,
            4,
            TerminalActivityLifecycle::Failed,
            None,
            Duration::from_secs(1),
            10_000,
        );
        assert_eq!(activity.state(), TerminalActivityState::Failed);
        activity.observe(
            &proof,
            5,
            TerminalActivityLifecycle::Live,
            output(3),
            Duration::from_secs(10),
            10_000,
        );
        assert_eq!(activity.state(), TerminalActivityState::Failed);
        let mut active = TerminalActivity::new(TerminalId::new(), proof.clone());
        active.observe(
            &proof,
            1,
            TerminalActivityLifecycle::Starting,
            output(1),
            Duration::ZERO,
            10_000,
        );
        assert_eq!(active.state(), TerminalActivityState::Starting);
        assert!(active.timer().is_none());
        active.observe(
            &proof,
            2,
            TerminalActivityLifecycle::Live,
            output(2),
            Duration::from_secs(1),
            10_000,
        );
        let active_timer = active.timer().unwrap();
        active.observe(
            &proof,
            3,
            TerminalActivityLifecycle::Exited,
            output(2),
            Duration::from_secs(2),
            10_000,
        );
        assert_eq!(active.state(), TerminalActivityState::Exited);
        assert!(!active.expire(&active_timer, Duration::from_secs(10)));
    }

    #[test]
    fn activity_restart_missing_facts_and_clock_reversal_are_unknown() {
        let current = proof("new");
        let mut activity = TerminalActivity::new(TerminalId::new(), current.clone());
        assert!(!activity.observe(
            &proof("old"),
            9,
            TerminalActivityLifecycle::Live,
            output(9),
            Duration::ZERO,
            10_000
        ));
        assert_eq!(activity.state(), TerminalActivityState::Unknown);
        activity.observe(
            &current,
            1,
            TerminalActivityLifecycle::Live,
            None,
            Duration::ZERO,
            10_000,
        );
        assert_eq!(activity.state(), TerminalActivityState::Unknown);
        activity.observe(
            &current,
            2,
            TerminalActivityLifecycle::Live,
            output(1),
            Duration::from_secs(2),
            9_999,
        );
        assert_eq!(activity.state(), TerminalActivityState::Unknown);
        activity.observe(
            &current,
            3,
            TerminalActivityLifecycle::Live,
            output(2),
            Duration::from_secs(3),
            10_000,
        );
        let deadline = activity.timer().unwrap();
        activity.observe(
            &current,
            4,
            TerminalActivityLifecycle::Live,
            output(2),
            Duration::from_secs(4),
            1,
        );
        assert_eq!(activity.timer(), Some(deadline.clone()));
        activity.observe(
            &current,
            5,
            TerminalActivityLifecycle::Live,
            output(3),
            Duration::from_secs(1),
            10_000,
        );
        assert_eq!(activity.state(), TerminalActivityState::Unknown);
        assert!(!activity.expire(&deadline, Duration::from_secs(10)));
        let mut incomplete = current.clone();
        incomplete.boot_id = BootId::new("");
        let mut invalid = TerminalActivity::new(TerminalId::new(), incomplete.clone());
        invalid.observe(
            &incomplete,
            1,
            TerminalActivityLifecycle::Starting,
            output(1),
            Duration::ZERO,
            10_000,
        );
        assert_eq!(invalid.state(), TerminalActivityState::Unknown);
    }

    #[test]
    fn missing_activity_does_not_erase_sequence_high_water() {
        let proof = proof("host");
        let mut activity = TerminalActivity::new(TerminalId::new(), proof.clone());
        activity.observe(
            &proof,
            1,
            TerminalActivityLifecycle::Live,
            output(10),
            Duration::ZERO,
            10_000,
        );
        let stale_timer = activity.timer().unwrap();
        activity.observe(
            &proof,
            1,
            TerminalActivityLifecycle::Live,
            None,
            Duration::from_secs(1),
            10_000,
        );
        assert_eq!(activity.state(), TerminalActivityState::Unknown);
        assert!(!activity.observe(
            &proof,
            1,
            TerminalActivityLifecycle::Live,
            output(9),
            Duration::from_secs(2),
            10_000
        ));
        assert_eq!(activity.state(), TerminalActivityState::Unknown);
        assert!(!activity.expire(&stale_timer, Duration::from_secs(10)));
        activity.observe(
            &proof,
            1,
            TerminalActivityLifecycle::Live,
            output(11),
            Duration::from_secs(3),
            10_000,
        );
        assert_eq!(activity.state(), TerminalActivityState::OutputActive);
    }
}

#[path = "agent_recovery.rs"]
pub mod agent_recovery;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSession {
    pub id: TerminalId,
    /// Native Codex listener provenance retained while this is its CLI view.
    #[serde(default)]
    pub owned_codex_listener: Option<crate::chat::OwnedCodexListener>,
    /// Stable provenance across replacement daemon incarnations. Unlike the
    /// current terminal ID, this remains unchanged through repeated recovery.
    #[serde(default)]
    pub recovery_origin: Option<TerminalId>,
    /// The exact session this one replaces. The daemon offers this session
    /// that one's lineage, and with it the agents it owns, when it first
    /// starts; `recovery_origin` names only the root of the chain.
    #[serde(default)]
    pub predecessor: Option<TerminalId>,
    pub launch: LaunchRecipe,
    pub transcript_path: Option<PathBuf>,
    pub resume_hint: Option<String>,
    pub continuity: TerminalContinuity,
}

impl TerminalSession {
    pub fn owns_codex_listener(&self) -> bool {
        crate::agent::Provider::from_program(&self.launch.program)
            == Some(crate::agent::Provider::Codex)
            && self
                .owned_codex_listener
                .as_ref()
                .is_some_and(|owned| owned.matches(self.id, &self.launch))
    }

    /// An exact saved agent identity, never a newest-session guess.
    pub fn agent_resume_recipe(&self) -> Option<LaunchRecipe> {
        self.resume_hint
            .as_deref()
            .and_then(agent_recovery::recipe_from_hint)
            .or_else(|| agent_recovery::resume_recipe(&self.launch))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptMetadata {
    pub path: PathBuf,
    pub byte_len: u64,
    pub durable_offset: u64,
    pub updated_at_unix_ms: i64,
    /// Fresh host evidence for the private transcript-location action. Saved
    /// metadata without this receipt is never authority to reveal a path.
    #[serde(default)]
    pub location: Option<TranscriptLocationReceipt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptLocationReceipt {
    pub root_device: u64,
    pub root_inode: u64,
    pub file_device: u64,
    pub file_inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserRestoreFidelity {
    /// The in-memory live view is still attached. This value is never promised
    /// after process restart and is downgraded before persistence.
    ExactLive,
    HistoryAndScroll,
    UrlOnly,
}

/// How many history entries a browser card retains.
///
/// The whole history vector is re-serialized into `browser_restore.history_json`
/// on every navigation, so an uncapped history makes each single navigation cost
/// time proportional to the card's entire lifetime — on both the serializer and
/// the SQLite blob write. A few hundred entries is far more back-history than
/// any user traverses while keeping that per-navigation cost flat; older
/// entries are dropped from the front.
pub const BROWSER_HISTORY_LIMIT: usize = 256;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserSession {
    pub id: BrowserId,
    pub current_url: Url,
    pub history: Vec<Url>,
    pub history_index: usize,
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub fidelity: BrowserRestoreFidelity,
}

impl BrowserSession {
    /// Returns a descriptor that makes only restart-safe claims.
    pub fn durable_descriptor(&self) -> Self {
        let mut descriptor = self.clone();
        if matches!(self.fidelity, BrowserRestoreFidelity::UrlOnly) {
            descriptor.history = vec![descriptor.current_url.clone()];
            descriptor.history_index = 0;
            descriptor.scroll_x = 0.0;
            descriptor.scroll_y = 0.0;
            descriptor.fidelity = BrowserRestoreFidelity::UrlOnly;
            return descriptor;
        }

        if descriptor.history.is_empty() {
            descriptor.history.push(descriptor.current_url.clone());
            descriptor.history_index = 0;
        } else if descriptor.history_index >= descriptor.history.len() {
            descriptor.history_index = descriptor.history.len() - 1;
        }
        trim_browser_history(&mut descriptor.history, &mut descriptor.history_index);
        descriptor.fidelity = BrowserRestoreFidelity::HistoryAndScroll;
        descriptor
    }
}

/// Drops the oldest entries until the history fits [`BROWSER_HISTORY_LIMIT`],
/// keeping `index` pointed at the same entry it pointed at before.
///
/// Recent history — everything a user can reach with the back button — is
/// preserved exactly; only entries older than the cap are forgotten.
pub fn trim_browser_history(history: &mut Vec<Url>, index: &mut usize) {
    let Some(excess) = history.len().checked_sub(BROWSER_HISTORY_LIMIT) else {
        return;
    };
    if excess == 0 {
        return;
    }
    history.drain(..excess);
    *index = index.saturating_sub(excess);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestView {
    pub repository: PathBuf,
    pub selected_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_panel_width: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_panel_collapsed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryView {
    pub project: ProjectId,
    pub repository: PathBuf,
    pub worktree: PathBuf,
    /// A dedicated file editor, rooted at `worktree`, rather than a repository
    /// browser. The relative path survives restore without requiring a file list.
    #[serde(
        default,
        with = "crate::selected_file_path",
        skip_serializing_if = "Option::is_none"
    )]
    pub selected_file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectState {
    pub id: ProjectId,
    pub generation: u64,
    pub name: String,
    pub repository: PathBuf,
    #[serde(default)]
    pub kind: ProjectKind,
    pub existence: Existence,
}

/// A folder keeps its identity when Git metadata appears or disappears later.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectKind {
    #[default]
    Git,
    Folder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeState {
    pub id: WorktreeId,
    pub project_id: ProjectId,
    pub generation: u64,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub existence: Existence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectChannel {
    pub id: ChannelId,
    pub project_id: ProjectId,
    pub title: String,
    #[serde(default)]
    pub title_source: ChannelTitleSource,
    /// The creation rank the shared registry assigned, mirrored here so the
    /// order survives a restart without a daemon round trip.
    ///
    /// A workspace serialized before the shared model existed has no rank, and
    /// zero is the right default for it: such a channel predates every ranked
    /// one, and ties keep their durable order, so the pre-existing channels stay
    /// in the order they were created in.
    #[serde(default)]
    pub ordinal: u32,
    /// Whether the registry this window last reconciled against is known to
    /// hold this channel.
    ///
    /// This is what makes the registry's silence about a channel mean something.
    /// A channel the registry lacks and this window never shared is new to the
    /// registry, so it is published; a channel the registry lacks that this
    /// window did share was deleted somewhere else, so it retires. Without the
    /// flag the two cases are indistinguishable, and treating them alike either
    /// tombstones a diverged window's cards or never honours a deletion.
    ///
    /// A workspace serialized before the shared model existed has shared
    /// nothing, so `false` is both the default and the truth.
    #[serde(default)]
    pub shared: bool,
    pub cards: Vec<CardId>,
    pub focused: Option<CardId>,
}

/// Legacy nonempty names and explicit renames remain authoritative. Automatic
/// names may refresh only with the current conversation's shared-view proof.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ChannelTitleSource {
    Prompt,
    Provider,
    #[default]
    Manual,
}

impl ChannelTitleSource {
    pub fn accepts(self, title: &str, suggested: Self) -> bool {
        suggested != Self::Manual && (title.is_empty() || suggested > self)
    }

    /// Refreshes from the same conversation only after its shared ownership has
    /// been checked. Legacy unproven suggestions retain the stricter upgrade rule.
    pub fn accepts_refresh(self, title: &str, suggested: Self) -> bool {
        suggested != Self::Manual && (title.is_empty() || suggested >= self)
    }
}

/// Repair navigation references without deleting any durable card. First
/// ownership wins for malformed imports; empty channels remain usable scopes.
pub(crate) fn normalize_project_channels(
    channels: &mut Vec<ProjectChannel>,
    active: &mut Option<ChannelId>,
    open_cards: &HashSet<CardId>,
) {
    let mut ids = HashSet::new();
    let mut owned = HashSet::new();
    channels.retain(|channel| ids.insert(channel.id));
    for channel in channels.iter_mut() {
        channel
            .cards
            .retain(|id| open_cards.contains(id) && owned.insert(*id));
        if !channel
            .focused
            .is_some_and(|id| channel.cards.contains(&id))
        {
            channel.focused = channel.cards.first().copied();
        }
    }
    if active.is_some_and(|id| !ids.contains(&id)) {
        *active = None;
    }
}

/// A stable navigation identity, independent of a window's focused pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkspaceDestination {
    Folder(ProjectId),
    Worktree(WorktreeId),
    Channel(ChannelId),
}

/// Editor navigation belongs to a window, independently of a shared pane's recipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepositoryNavigation {
    pub project: ProjectId,
    pub worktree: PathBuf,
    #[serde(with = "crate::selected_file_path")]
    pub selected_file: Option<PathBuf>,
    pub editor: Option<EditorNavigation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EditorPosition {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EditorNavigation {
    pub caret: EditorPosition,
    pub selection: Option<EditorPosition>,
    pub scroll_line: usize,
    pub scroll_vertical: f32,
    pub scroll_horizontal: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub schema_version: u32,
    pub id: WorkspaceId,
    pub generation: u64,
    pub existence: Existence,
    pub cards: Vec<CardState>,
    pub focused: Option<CardId>,
    #[serde(default)]
    pub active_worktree: Option<PathBuf>,
    #[serde(default)]
    pub worktree_focus: HashMap<PathBuf, CardId>,
    #[serde(default)]
    pub project_order: Vec<ProjectId>,
    /// A workspace serialized before the project-level container was renamed
    /// spells these `threads` and `active_thread`; the aliases are what let
    /// such a payload restore its channels instead of defaulting them away.
    #[serde(default, alias = "threads")]
    pub channels: Vec<ProjectChannel>,
    #[serde(default, alias = "active_thread")]
    pub active_channel: Option<ChannelId>,
    /// Explicit workspace filing, independent of a card's launch directory.
    ///
    /// Adoption records the chosen workspace without rewriting launch recipes.
    /// Cloning and reopening preserve that ownership, including editors whose
    /// descriptor points into a nested directory, so startup projection keeps
    /// them in the workspace where they were opened.
    #[serde(default)]
    pub card_scopes: HashMap<CardId, PathBuf>,
    /// Unsent native-chat composer text, keyed by the card that owns it.
    #[serde(default)]
    pub chat_drafts: HashMap<CardId, String>,
    #[serde(default)]
    pub chat_draft_attachments: HashMap<CardId, Vec<crate::chat::Attachment>>,
    /// Unseen semantic agent updates, including their last known worktree owner.
    #[serde(default)]
    pub agent_updates: BTreeMap<CardId, Option<(ProjectId, WorktreeId)>>,
    /// Most recently visited workspace identities, newest first. Missing
    /// identities are pruned when navigation opens; this grants no ownership.
    #[serde(default)]
    pub workspace_history: Vec<WorkspaceDestination>,
    /// Current file, caret and viewport for each editor in this window.
    #[serde(default)]
    pub repository_navigation: HashMap<CardId, RepositoryNavigation>,
    /// The shared registry this window last reconciled against.
    ///
    /// Paired with [`ProjectChannel::shared`]: a flag only means anything
    /// relative to the registry it was set against. A daemon whose data
    /// directory was replaced presents a new identity, and every flag has to be
    /// cleared then, because nothing local has been shared with *that* registry.
    #[serde(default)]
    pub workspace_model_identity: Option<uuid::Uuid>,
    #[serde(default)]
    pub local_only_cards: HashSet<CardId>,
    /// Rejected saved pane sources. These remain inert across restarts and
    /// registry replacement until exact authoritative proof admits them.
    /// This marker grants no provider, attachment or retirement authority.
    #[serde(default)]
    pub quarantined_panes: HashMap<CardId, CardKind>,
    /// Saved shared sources kept inert until their exact descriptor is admitted.
    /// This is local presentation state, never provider or retirement authority.
    #[serde(default)]
    pub pending_chat_admissions: HashMap<CardId, crate::chat::Session>,
    /// Explicit terminal creations awaiting the daemon's accepted view. Their
    /// exact recipe, scope and insertion order are stored with the pane.
    #[serde(default)]
    pub pending_terminal_creations: HashSet<CardId>,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            id: WorkspaceId::new(),
            generation: 0,
            existence: Existence::Open,
            cards: Vec::new(),
            focused: None,
            active_worktree: None,
            worktree_focus: HashMap::new(),
            project_order: Vec::new(),
            channels: Vec::new(),
            active_channel: None,
            card_scopes: HashMap::new(),
            chat_drafts: HashMap::new(),
            chat_draft_attachments: HashMap::new(),
            agent_updates: BTreeMap::new(),
            workspace_history: Vec::new(),
            repository_navigation: HashMap::new(),
            workspace_model_identity: None,
            local_only_cards: HashSet::new(),
            quarantined_panes: Default::default(),
            pending_chat_admissions: Default::default(),
            pending_terminal_creations: HashSet::new(),
        }
    }
}

impl WorkspaceState {
    pub fn normalize_channels(&mut self) {
        let open_cards = self
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .map(|card| card.id)
            .collect();
        normalize_project_channels(&mut self.channels, &mut self.active_channel, &open_cards);
    }

    pub fn focused_index(&self) -> Option<usize> {
        let focused = self.focused?;
        self.cards
            .iter()
            .position(|card| card.id == focused && card.existence.is_open())
    }

    pub fn normalize_focus(&mut self) {
        if self.focused_index().is_none() {
            self.focused = self
                .cards
                .iter()
                .find(|card| card.existence.is_open())
                .map(|card| card.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn legacy_repository_descriptor_remains_a_browser() {
        let view: RepositoryView = serde_json::from_value(serde_json::json!({
            "project": ProjectId::new(), "repository": "/repo", "worktree": "/repo/worktree"
        }))
        .unwrap();
        assert_eq!(view.selected_file, None);
        assert!(
            serde_json::to_value(view)
                .unwrap()
                .get("selected_file")
                .is_none()
        );
    }

    #[test]
    fn review_file_panel_preferences_round_trip_and_old_descriptors_keep_auto_layout() {
        let mut view: PullRequestView = serde_json::from_value(
            serde_json::json!({ "repository": "/repo", "selected_number": 12 }),
        )
        .unwrap();
        assert_eq!(view.file_panel_width, None);
        assert_eq!(view.file_panel_collapsed, None);
        view.file_panel_width = Some(360);
        view.file_panel_collapsed = Some(true);
        assert_eq!(
            serde_json::from_slice::<PullRequestView>(&serde_json::to_vec(&view).unwrap()).unwrap(),
            view
        );
    }

    #[test]
    fn legacy_workspace_descriptor_defaults_navigation_context() {
        let id = WorkspaceId::new();
        let descriptor = serde_json::json!({
            "schema_version": 1,
            "id": id,
            "generation": 0,
            "existence": "open",
            "cards": [],
            "focused": null,
        });
        let restored: WorkspaceState = serde_json::from_value(descriptor).unwrap();
        assert_eq!(restored.id, id);
        assert_eq!(restored.active_worktree, None);
        assert!(restored.worktree_focus.is_empty());
        assert!(restored.channels.is_empty());
        assert_eq!(restored.active_channel, None);
    }

    /// A workspace saved before the project-level container was renamed spells
    /// its navigation state `threads` and `active_thread`. Such a payload must
    /// restore complete: the channel's identity, its chats, which chat it had
    /// focused, its title, and which channel was selected.
    #[test]
    fn workspace_saved_before_the_channel_rename_restores_its_channels() {
        let workspace_id = WorkspaceId::new();
        let project_id = ProjectId::new();
        let investigation = ChannelId::new();
        let review = ChannelId::new();
        let planner = CardId::new();
        let reader = CardId::new();
        let reviewer = CardId::new();
        let chat = |id: CardId, title: &str| {
            serde_json::to_value(CardState {
                id,
                generation: 9,
                title: title.to_owned(),
                width_fraction: 0.5,
                existence: Existence::Open,
                exposure: Exposure::Visible,
                residency: Residency::Hot,
                kind: CardKind::Chat(crate::chat::Session {
                    native_continuity: Default::default(),
                    owned_codex_listener: None,
                    id: TerminalId::new(),
                    launch: crate::chat::Launch {
                        provider: crate::agent::Provider::Omp,
                        recipe: LaunchRecipe {
                            unset_environment: Default::default(),
                            program: "/usr/bin/omp".into(),
                            arguments: Vec::new(),
                            working_directory: PathBuf::from("/repo"),
                            environment: Default::default(),
                        },
                        session_id: None,
                    },
                    proof: None,
                    terminal: None,
                }),
            })
            .unwrap()
        };
        let descriptor = serde_json::json!({
            "schema_version": 1,
            "id": workspace_id,
            "generation": 12,
            "existence": "open",
            "focused": reader,
            "active_worktree": "/repo",
            "worktree_focus": { "/repo": reader },
            "project_order": [project_id],
            "cards": [
                chat(planner, "Plan the migration"),
                chat(reader, "Read the parser"),
                chat(reviewer, "Review the diff"),
            ],
            "threads": [
                {
                    "id": investigation,
                    "project_id": project_id,
                    "title": "Investigate startup",
                    "cards": [planner, reader],
                    "focused": reader,
                },
                {
                    "id": review,
                    "project_id": project_id,
                    "title": "",
                    "cards": [reviewer],
                    "focused": reviewer,
                },
            ],
            "active_thread": investigation,
        });

        let mut restored: WorkspaceState = serde_json::from_value(descriptor).unwrap();
        restored.normalize_channels();
        assert_eq!(restored.id, workspace_id);
        assert_eq!(restored.active_channel, Some(investigation));
        assert_eq!(restored.channels.len(), 2);
        assert_eq!(restored.channels[0].id, investigation);
        assert_eq!(restored.channels[0].project_id, project_id);
        assert_eq!(restored.channels[0].title, "Investigate startup");
        assert_eq!(restored.channels[0].cards, vec![planner, reader]);
        assert_eq!(restored.channels[0].focused, Some(reader));
        assert_eq!(restored.channels[1].id, review);
        assert_eq!(restored.channels[1].cards, vec![reviewer]);
        assert_eq!(restored.channels[1].focused, Some(reviewer));
        assert!(restored.channels[1].title.is_empty());
    }

    #[test]
    fn channel_reference_repair_keeps_empty_scopes_and_exclusive_card_ownership() {
        let first = CardId::new();
        let second = CardId::new();
        let missing = CardId::new();
        let project_id = ProjectId::new();
        let a = ChannelId::new();
        let b = ChannelId::new();
        let mut channels = vec![
            ProjectChannel {
                title_source: Default::default(),
                id: a,
                project_id,
                title: "A".into(),
                ordinal: 1,
                shared: false,
                cards: vec![first, first, missing],
                focused: Some(missing),
            },
            ProjectChannel {
                title_source: Default::default(),
                id: b,
                project_id,
                title: "B".into(),
                ordinal: 2,
                shared: false,
                cards: vec![first, second],
                focused: Some(first),
            },
        ];
        let mut active = Some(b);
        normalize_project_channels(&mut channels, &mut active, &HashSet::from([first, second]));
        assert_eq!(channels[0].cards, vec![first]);
        assert_eq!(channels[0].focused, Some(first));
        assert_eq!(channels[1].cards, vec![second]);
        assert_eq!(channels[1].focused, Some(second));
        normalize_project_channels(&mut channels, &mut active, &HashSet::from([first]));
        assert!(channels[1].cards.is_empty());
        assert_eq!(channels[1].focused, None);
        assert_eq!(active, Some(b));
        active = Some(ChannelId::new());
        normalize_project_channels(&mut channels, &mut active, &HashSet::from([first]));
        assert_eq!(active, None);
    }

    #[test]
    fn passwd_shell_wins_when_environment_is_absent_or_stale() {
        let valid = |candidate: &str| candidate == "/bin/fish";

        assert_eq!(
            choose_login_shell(Some("/bin/fish".into()), None, valid).unwrap(),
            "/bin/fish"
        );
        assert_eq!(
            choose_login_shell(
                Some("/bin/fish".into()),
                Some("/stale/environment-shell".into()),
                valid,
            )
            .unwrap(),
            "/bin/fish"
        );
    }

    #[test]
    fn shell_selection_falls_back_in_documented_order() {
        let valid = |candidate: &str| matches!(candidate, "/bin/zsh" | "/bin/sh");
        assert_eq!(
            choose_login_shell(Some("relative".into()), Some("/bin/zsh".into()), valid,).unwrap(),
            "/bin/zsh"
        );
        assert_eq!(
            choose_login_shell(Some("relative".into()), None, valid).unwrap(),
            "/bin/sh"
        );
        assert_eq!(
            choose_login_shell(Some("relative".into()), None, |_| false),
            Err(LoginShellError::Unavailable)
        );
    }

    #[test]
    fn valid_shell_must_be_absolute_regular_and_executable() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("shell");
        std::fs::write(&executable, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(is_valid_shell(executable.to_str().unwrap()));
        assert!(!is_valid_shell("shell"));

        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!is_valid_shell(executable.to_str().unwrap()));
        assert!(!is_valid_shell(directory.path().to_str().unwrap()));
    }

    #[test]
    fn explicit_launch_recipe_round_trip_is_unchanged() {
        let recipe = LaunchRecipe {
            unset_environment: BTreeSet::from(["ANTHROPIC_API_KEY".into()]),
            program: "/usr/bin/env".into(),
            arguments: vec!["printf".into(), "%s".into(), "unchanged".into()],
            working_directory: PathBuf::from("/tmp/worktree"),
            environment: BTreeMap::from([("EXPLICIT".into(), "yes".into())]),
        };

        let encoded = serde_json::to_vec(&recipe).unwrap();
        assert_eq!(
            serde_json::from_slice::<LaunchRecipe>(&encoded).unwrap(),
            recipe
        );
        let mut legacy = serde_json::to_value(&recipe).unwrap();
        legacy.as_object_mut().unwrap().remove("unset_environment");
        assert!(
            serde_json::from_value::<LaunchRecipe>(legacy)
                .unwrap()
                .unset_environment
                .is_empty()
        );
        let wire = postcard::to_allocvec(&recipe).unwrap();
        assert_eq!(postcard::from_bytes::<LaunchRecipe>(&wire).unwrap(), recipe);
    }

    #[test]
    fn browser_history_trim_drops_the_oldest_entries_and_follows_the_cursor() {
        let mut history: Vec<Url> = (0..BROWSER_HISTORY_LIMIT + 10)
            .map(|index| Url::parse(&format!("https://example.test/{index}")).unwrap())
            .collect();
        let newest = history.last().cloned().unwrap();
        let mut index = history.len() - 1;

        trim_browser_history(&mut history, &mut index);

        assert_eq!(history.len(), BROWSER_HISTORY_LIMIT);
        assert_eq!(history[index], newest, "the cursor must follow its entry");
        assert_eq!(
            history[0],
            Url::parse("https://example.test/10").unwrap(),
            "the oldest entries are the ones dropped"
        );

        // Already inside the bound: nothing moves.
        let mut short = vec![Url::parse("https://example.test/only").unwrap()];
        let mut short_index = 0;
        trim_browser_history(&mut short, &mut short_index);
        assert_eq!(short.len(), 1);
        assert_eq!(short_index, 0);
    }
}
