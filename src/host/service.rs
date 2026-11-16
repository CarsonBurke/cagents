//! A failed service step must not silently retire its long-lived worker.
//!
//! Catch at service boundaries, after unwinding has released the step's
//! guards. Retryable services consult their durable registries again; other
//! services report their unavailability. The panic hook still records
//! a typed recovered-panic event with the panic's location; explicit local
//! debug capture includes detail.
//!
//! [`spawn`] and [`periodic`] are the one way to start a detached service
//! thread: nothing joins them, so they may wait out a backoff without delaying
//! shutdown. A service that a `Drop` joins keeps its stop-aware wait and
//! contains each step with [`attempt`] instead.
use std::{io, thread, time::Duration};

/// The longest wait a backoff grows to, unless its base is already longer.
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);

/// A panic that [`contain`] stopped at a service boundary.
#[derive(Debug)]
pub(super) struct Panicked {
    service: &'static str,
}

impl std::fmt::Display for Panicked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} panicked; inspect `cagents diagnostics`",
            self.service
        )
    }
}

/// Runs one step of `service`, turning a panic into [`Panicked`].
pub(super) fn contain<T>(service: &'static str, work: impl FnOnce() -> T) -> Result<T, Panicked> {
    let _recovered = crate::instance::recovered_panic_scope();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).map_err(|_| Panicked { service })
}

/// Runs one fallible step of `service`, reporting a panic as its error.
pub(super) fn attempt<T>(
    service: &'static str,
    work: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    contain(service, work).unwrap_or_else(|panicked| Err(panicked.to_string()))
}

/// Whether a supervised service runs another step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Next {
    Continue,
    /// The daemon the service works for is gone or stopping.
    Stop,
}

/// Successive waits after consecutive failures: `base`, doubling, at most an
/// hour or `base`, whichever is longer. A step that panics every time is
/// retried, but is not recorded in diagnostics every second for days.
#[derive(Debug, Clone)]
pub(super) struct Backoff {
    base: Duration,
    failures: u32,
}

impl Backoff {
    pub(super) const fn new(base: Duration) -> Self {
        Self { base, failures: 0 }
    }

    /// The wait after one more consecutive failure.
    pub(super) fn fail(&mut self) -> Duration {
        let wait = self
            .base
            .saturating_mul(1 << self.failures.min(16))
            .min(MAX_BACKOFF.max(self.base));
        self.failures = self.failures.saturating_add(1);
        wait
    }

    pub(super) fn succeed(&mut self) {
        self.failures = 0;
    }
}

/// Starts the detached service thread `name`, which runs `step` until it
/// returns [`Next::Stop`]. The step does its own waiting. A panicking step is
/// contained: the panic hook records where it happened, the service logs it,
/// waits out a [`Backoff`] from `retry` (since the step may have panicked
/// before its own wait) and runs the next step. The thread never ends because
/// of a panic, so whoever registered it can keep handing it work.
pub(super) fn spawn(
    name: &'static str,
    retry: Duration,
    mut step: impl FnMut() -> Next + Send + 'static,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new().name(name.into()).spawn(move || {
        let mut backoff = Backoff::new(retry);
        loop {
            match contain(name, &mut step) {
                Ok(Next::Continue) => backoff.succeed(),
                Ok(Next::Stop) => break,
                Err(panicked) => {
                    let wait = backoff.fail();
                    tracing::error!(
                        %panicked,
                        retry_seconds = wait.as_secs_f64(),
                        "a daemon service step panicked; the service continues"
                    );
                    thread::sleep(wait);
                }
            }
        }
    })
}

/// Starts the detached service thread `name`, which runs `pass` now and then
/// every `interval` until it returns [`Next::Stop`]. A pass that panics is
/// retried after `interval`, backing off while it keeps panicking.
pub(super) fn periodic(
    name: &'static str,
    interval: Duration,
    mut pass: impl FnMut() -> Next + Send + 'static,
) -> io::Result<thread::JoinHandle<()>> {
    spawn(name, interval, move || {
        let next = pass();
        if next == Next::Continue {
            thread::sleep(interval);
        }
        next
    })
}

#[cfg(test)]
pub(super) fn test_host() -> super::state::SharedHost {
    use crate::model::{BootId, HostIncarnation, HostSessionId, HostSessionProof};
    std::sync::Arc::new(super::state::HostLock::new(super::state::HostState::new(
        HostSessionProof {
            boot_id: BootId::new("service-test"),
            host_session_id: HostSessionId::new("service-test"),
            host_incarnation: HostIncarnation::new("service-test"),
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const PROMPT: Duration = Duration::from_secs(10);

    #[test]
    fn a_panic_is_reported_as_the_service_error() {
        assert_eq!(attempt("probe", || Ok::<_, String>(3)), Ok(3));
        assert_eq!(
            attempt::<()>("probe", || panic!("probe failure")),
            Err("probe panicked; inspect `cagents diagnostics`".to_owned())
        );
        assert_eq!(contain("probe", || 4).unwrap(), 4);
    }

    #[test]
    fn backoff_doubles_to_its_limit_and_restarts_after_success() {
        let mut backoff = Backoff::new(Duration::from_secs(600));
        let waits: Vec<_> = (0..4).map(|_| backoff.fail().as_secs()).collect();
        assert_eq!(waits, [600, 1200, 2400, 3600]);
        assert_eq!(backoff.fail(), MAX_BACKOFF);
        backoff.succeed();
        assert_eq!(backoff.fail(), Duration::from_secs(600));
        // A base beyond the limit is never shortened by it.
        let mut long = Backoff::new(Duration::from_secs(6 * 60 * 60));
        assert_eq!(long.fail(), Duration::from_secs(6 * 60 * 60));
        assert_eq!(long.fail(), Duration::from_secs(6 * 60 * 60));
    }

    #[test]
    fn a_panicking_step_does_not_end_the_service() {
        let (steps, stepped) = mpsc::channel();
        let mut count = 0;
        let service = spawn("service-test", Duration::from_millis(1), move || {
            count += 1;
            steps.send(count).unwrap();
            match count {
                1 | 2 => panic!("step {count} fails"),
                3 => Next::Continue,
                _ => Next::Stop,
            }
        })
        .unwrap();
        let observed: Vec<_> = (0..4)
            .map(|_| stepped.recv_timeout(PROMPT).unwrap())
            .collect();
        assert_eq!(observed, [1, 2, 3, 4]);
        service
            .join()
            .expect("the service thread ends only when a step stops it");
    }

    #[test]
    fn a_periodic_pass_that_panics_runs_again() {
        let (passes, passed) = mpsc::channel();
        let mut count = 0;
        let service = periodic("periodic-test", Duration::from_millis(1), move || {
            count += 1;
            passes.send(count).unwrap();
            if count == 1 {
                panic!("first pass fails");
            }
            if count < 3 {
                Next::Continue
            } else {
                Next::Stop
            }
        })
        .unwrap();
        let observed: Vec<_> = (0..3)
            .map(|_| passed.recv_timeout(PROMPT).unwrap())
            .collect();
        assert_eq!(observed, [1, 2, 3]);
        service.join().unwrap();
    }
}
