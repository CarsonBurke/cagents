//! Deadline-driven retention considers only completed, unattached terminals.
//! Saved pane pins are checked before retirement; holder teardown happens
//! outside the host lock.
use super::state::SharedHost;
use std::{collections::BTreeSet, time::{Duration, Instant}};

const EXITED_RETENTION: Duration = Duration::from_secs(10 * 60);

pub(super) struct Lifecycle {
    retention: Duration,
    retention_error_reported: bool,
    idle: IdleTimer,
}

impl Lifecycle {
    pub(super) fn new() -> Self {
        Self {
            retention: duration_setting("CAGENTS_EXITED_TERMINAL_RETENTION_SECS", EXITED_RETENTION),
            retention_error_reported: false,
            idle: IdleTimer::new(crate::instance::IS_DEVELOPMENT, duration_setting("CAGENTS_DEVELOPMENT_IDLE_TIMEOUT_SECS", Duration::from_secs(30 * 60))),
        }
    }

    pub(super) fn maintain(&mut self, host: &SharedHost, connected: bool) -> Option<Instant> {
        let retention = self.maintain_retention(host);
        if !self.idle.enabled { return retention; }
        let requested = crate::daemon::take_idle_exit_request();
        let (candidate, gate, registry) = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            (state.development_idle() && !connected, state.control_gate.clone(), state.execution_registry.clone())
        };
        let now = Instant::now();
        let idle = if candidate {
            let _control = gate.write().unwrap_or_else(|p| p.into_inner());
            let quiescent = registry.is_some_and(|registry| {
                let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
                matches!(registry.cleanup_execution_is_quiescent(), Ok(true))
                    && registry.active_automation_firings().is_ok_and(|firings| firings.is_empty())
                    && matches!(registry.next_automation_deadline(), Ok(None))
            });
            let no_holders = quiescent && holders_empty();
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let idle = no_holders && state.development_idle();
            if idle && (requested || self.idle.expired(now)) {
                tracing::info!(requested, "Stopping an unused development daemon");
                state.request_shutdown();
                crate::daemon::wake();
            }
            idle
        } else { false };
        let idle = self.idle.update(now, idle);
        match (retention, idle) {
            (Some(retention), Some(idle)) => Some(retention.min(idle)),
            (deadline, None) | (None, deadline) => deadline,
        }
    }

    fn maintain_retention(&mut self, host: &SharedHost) -> Option<Instant> {
        let now = Instant::now();
        let (deadline, gate, registry) = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.observe_completed_terminals(now);
            (state.terminal_retention_deadline(self.retention, &BTreeSet::new()),
                state.control_gate.clone(), state.execution_registry.clone())
        };
        let deadline = deadline?;
        if deadline > now { return Some(deadline); }

        // Fence pane edits, attaches and upgrades while deciding which completed
        // descriptors can retire. Terminal output does not acquire this gate.
        let _control = gate.write().unwrap_or_else(|p| p.into_inner());
        let pinned = registry.ok_or_else(|| "The workspace registry is unavailable".to_owned())
            .and_then(|registry| registry.lock().unwrap_or_else(|p| p.into_inner()).pinned_terminals());
        let pinned = match pinned {
            Ok(pinned) => { self.retention_error_reported = false; pinned },
            Err(error) => {
                if !self.retention_error_reported {
                    tracing::warn!(%error, "Exited terminal retention is deferred; pane ownership is unknown");
                    self.retention_error_reported = true;
                }
                // Retry on actual activity, never on a periodic failure timer.
                return None;
            },
        };
        let (retired, next) = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let retired = state.retire_expired_terminals(now, self.retention, &pinned);
            let next = state.terminal_retention_deadline(self.retention, &pinned);
            (retired, next)
        };
        // Removal fenced ownership. Holder teardown may wait on another
        // process, so it must not retain the global control admission gate.
        drop(_control);
        for record in retired {
            let id = record.session.id;
            if let Err(error) = record.handle.shutdown() {
                tracing::warn!(%error, terminal = %id, "Retired terminal holder cleanup failed");
            } else {
                tracing::debug!(terminal = %id, "Retired an exited unattached terminal");
            }
        }
        next
    }
}

fn duration_setting(name: &str, fallback: Duration) -> Duration {
    match std::env::var(name) {
        Ok(value) => match value.parse::<u64>() {
            Ok(seconds) if seconds <= 365 * 24 * 60 * 60 => Duration::from_secs(seconds),
            _ => { tracing::warn!(setting = name, "Invalid lifecycle duration; using default"); fallback },
        },
        Err(_) => fallback,
    }
}

/// No periodic idle checks: becoming empty arms one deadline, and any work
/// cancels it. Production identities never create that deadline.
struct IdleTimer {
    enabled: bool,
    timeout: Duration,
    deadline: Option<Instant>,
}
impl IdleTimer {
    fn new(development: bool, timeout: Duration) -> Self {
        Self { enabled: development, timeout, deadline: None }
    }
    fn expired(&self, now: Instant) -> bool { self.deadline.is_some_and(|deadline| now >= deadline) }
    fn update(&mut self, now: Instant, idle: bool) -> Option<Instant> {
        if self.enabled && idle { self.deadline.get_or_insert(now + self.timeout); }
        else { self.deadline = None; }
        self.deadline
    }
}

fn holders_empty() -> bool {
    use crate::daemon::holder_registry::{self as registry, Kind};
    let Ok(boot) = crate::daemon::read_boot_id() else { return false; };
    [Kind::Terminal, Kind::Native].into_iter().all(|kind| {
        registry::directory(kind).and_then(|directory| {
            if !registry::discover(&directory, &boot, kind)?.is_empty() { return Ok(false); }
            // Discovery ignores malformed/unsupported evidence. An idle exit
            // must preserve ownership it cannot safely classify.
            let entries = std::fs::read_dir(directory).map_err(|error| error.to_string())?;
            for entry in entries {
                if entry.map_err(|error| error.to_string())?.path().extension()
                    .is_some_and(|extension| extension == "json") { return Ok(false); }
            }
            Ok(true)
        }).unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn development_idle_arms_one_deadline_and_work_cancels_it() {
        let now = Instant::now();
        let mut timer = IdleTimer::new(true, Duration::from_secs(30));
        assert_eq!(timer.update(now, true), Some(now + Duration::from_secs(30)));
        assert_eq!(timer.update(now + Duration::from_secs(10), true), Some(now + Duration::from_secs(30)));
        assert!(!timer.expired(now + Duration::from_secs(29)));
        assert!(timer.expired(now + Duration::from_secs(30)));
        assert_eq!(timer.update(now + Duration::from_secs(31), false), None);
        assert_eq!(timer.update(now + Duration::from_secs(32), true), Some(now + Duration::from_secs(62)));
    }
    #[test]
    fn production_identity_never_arms_an_idle_deadline() {
        let mut timer = IdleTimer::new(false, Duration::ZERO);
        assert_eq!(timer.update(Instant::now(), true), None);
        assert!(!timer.expired(Instant::now()));
    }
}
