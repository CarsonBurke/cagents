//! Deadline-driven retention considers only completed, unattached terminals.
//! Saved pane pins are checked before retirement; holder teardown happens
//! outside the host lock.
use super::state::SharedHost;
use std::{collections::BTreeSet, time::{Duration, Instant}};

const EXITED_RETENTION: Duration = Duration::from_secs(10 * 60);

pub(super) struct Lifecycle {
    retention: Duration,
    retention_error_reported: bool,
    cleanup_admission_error_reported: bool,
    idle: IdleTimer,
}

impl Lifecycle {
    pub(super) fn new() -> Self {
        Self {
            retention: duration_setting("CAGENTS_EXITED_TERMINAL_RETENTION_SECS", EXITED_RETENTION),
            retention_error_reported: false,
            cleanup_admission_error_reported: false,
            idle: IdleTimer::new(crate::instance::IS_DEVELOPMENT, duration_setting("CAGENTS_DEVELOPMENT_IDLE_TIMEOUT_SECS", Duration::from_secs(30 * 60))),
        }
    }

    #[cfg(test)]
    pub(super) fn testing(retention: Duration) -> Self {
        Self { retention, retention_error_reported: false, cleanup_admission_error_reported: false,
            idle: IdleTimer::new(false, Duration::ZERO) }
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

    fn settle_retirements(&mut self, host: &SharedHost) {
        use std::sync::mpsc::TryRecvError;
        let (gate, ready) = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let ready = state.terminal_retirements.iter_mut().filter_map(|(id, retirement)| {
                let outcome = match retirement.result.as_ref()?.try_recv() {
                    Ok(outcome) => outcome,
                    Err(TryRecvError::Empty) => return None,
                    Err(TryRecvError::Disconnected) => Err("Terminal cleanup owner ended without a proof result".into()),
                };
                retirement.result = None;
                Some((*id, retirement.ticket, outcome))
            }).collect::<Vec<_>>();
            (state.control_gate.clone(), ready)
        };
        // Streaming activity must not acquire exclusive admission while an
        // asynchronous cleanup is still waiting or no retirement is pending.
        if ready.is_empty() { return; }
        let control = gate.write().unwrap_or_else(|p| p.into_inner());
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let mut released = Vec::new();
        let mut retired = Vec::new();
        let mut reports = Vec::new();
        for (id, ticket, outcome) in ready {
            if !state.terminal_retirements.get(&id).is_some_and(|entry| entry.ticket == ticket) { continue; }
            let Some(mut reservation) = state.terminal_retirements.remove(&id) else { continue; };
            if state.terminals.get(&id).is_some_and(|record| record.handle.same_worker(&reservation.handle)) {
                match outcome {
                    Ok(()) => {
                        if let Some(record) = state.retire_terminal_if_current(id, &reservation.handle) { retired.push(record); }
                        reports.push((id, None));
                    }
                    Err(error) => {
                        reports.push((id, Some(error.clone())));
                        reservation.failure = Some(error);
                        state.terminal_retirements.insert(id, reservation);
                        state.inventory_generation += 1;
                        continue;
                    }
                }
            }
            released.push(reservation);
        }
        drop(state);
        drop(control);
        for (id, error) in reports {
            if let Some(error) = error {
                tracing::warn!(terminal = %id, %error, "Exited terminal remains listed for explicit cleanup retry");
            } else {
                tracing::debug!(terminal = %id, "Retired an exited terminal after exact cleanup proof");
            }
        }
        // The last handle or record may own teardown; drop outside HostState.
        drop(released);
        drop(retired);
    }

    fn maintain_retention(&mut self, host: &SharedHost) -> Option<Instant> {
        self.settle_retirements(host);
        let now = Instant::now();
        let (deadline, gate, registry) = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.observe_completed_terminals(now);
            (state.terminal_retention_deadline(self.retention, &BTreeSet::new()),
                state.control_gate.clone(), state.execution_registry.clone())
        };
        let deadline = deadline?;
        if deadline > now { return Some(deadline); }

        let control = gate.write().unwrap_or_else(|p| p.into_inner());
        let candidates = host.lock().unwrap_or_else(|p| p.into_inner()).terminals.keys().copied().collect::<BTreeSet<_>>();
        let pinned = registry.ok_or_else(|| "The workspace registry is unavailable".to_owned())
            .and_then(|registry| registry.lock().unwrap_or_else(|p| p.into_inner()).pinned_terminals(&candidates));
        let mut pinned = match pinned {
            Ok(pinned) => { self.retention_error_reported = false; pinned },
            Err(error) => {
                if !self.retention_error_reported {
                    tracing::warn!(%error, "Exited terminal retention is deferred; pane ownership is unknown");
                    self.retention_error_reported = true;
                }
                return None;
            },
        };
        let handles = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let handles = state.expired_terminal_handles(now, self.retention, &pinned);
            for (id, handle) in &handles {
                state.terminal_retirements.insert(*id, super::state::Retirement::submitting(handle.clone()));
                state.inventory_generation += 1;
            }
            handles
        };
        drop(control);
        // Admission is nonblocking and uses the existing fixed cleanup pool.
        // Neither provider cleanup nor worker joins borrow global admission.
        for (id, handle) in handles {
            let result = handle.release_completed_holder_async();
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let current = state.terminals.get(&id).is_some_and(|record| record.handle.same_worker(&handle));
            let mut rejected = None;
            if current && state.terminal_retirements.get(&id).is_some_and(|entry| entry.handle.same_worker(&handle)) {
                match result {
                    Ok(receiver) => {
                        state.terminal_retirements.get_mut(&id).unwrap().result = Some(receiver);
                        self.cleanup_admission_error_reported = false;
                    }
                    Err(error) => {
                        rejected = state.terminal_retirements.remove(&id);
                        state.inventory_generation += 1;
                        // No overdue timer for queue pressure/service failure.
                        // Existing pool task completion wakes the daemon to retry.
                        pinned.insert(id);
                        if !self.cleanup_admission_error_reported {
                            tracing::warn!(%error, terminal = %id, "Terminal retirement admission deferred; exact worker remains listed");
                            self.cleanup_admission_error_reported = true;
                        }
                    }
                }
            }
            drop(state);
            drop(rejected);
        }
        host.lock().unwrap_or_else(|p| p.into_inner()).terminal_retention_deadline(self.retention, &pinned)
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
