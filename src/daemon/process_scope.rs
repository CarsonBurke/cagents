//! Give a process owner and its future descendants their own OOM selection unit.
//! Holders enroll before spawning their provider/PTY root. Direct providers
//! enroll before receiving their first prompt.
//!
//! The unit is optional isolation, never process ownership: every entry point
//! leaves the existing launch in place when the system offers no such unit.
//! On Linux the OS user manager gives one, a systemd scope. macOS has none: it
//! has no systemd-oomd and its memory killer (jetsam) ends whole processes, so
//! there is no cgroup to choose between a provider and its commands. There
//! every enrollment answers that no scope exists, a provider and its commands
//! share whatever a memory kill does to them, and no command is reported as
//! stopped for memory.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as backend;
#[cfg(target_os = "macos")]
mod unsupported;
#[cfg(target_os = "macos")]
use unsupported as backend;

pub use backend::collect_empty_scopes;
#[cfg(test)]
pub(crate) use backend::tests;
pub(crate) use backend::{
    ProviderScope, enroll_dividing_holder, enroll_holder, enroll_provider,
    warn_if_bootstrap_shared, warn_if_shared,
};

/// What ended processes in a cgroup because memory ran out.
// Only a Linux scope reports one; elsewhere callers still read the type.
#[cfg_attr(target_os = "macos", allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Killer {
    /// systemd-oomd, because the system's memory, swap or memory pressure
    /// passed its limits.
    Oomd,
    /// The kernel's OOM killer.
    Kernel,
}

/// A command that memory exhaustion stopped while its provider kept running.
#[cfg_attr(target_os = "macos", allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandStop {
    pub(crate) killer: Killer,
    /// Processes ended, when the killer reports it.
    pub(crate) processes: Option<u64>,
}
