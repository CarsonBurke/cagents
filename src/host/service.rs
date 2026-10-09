//! A failed service step must not silently retire its long-lived worker.
//!
//! Catch at service boundaries, after unwinding has released the step's
//! guards. Retryable services consult their durable registries again; other
//! services report their unavailability. The panic hook still records
//! the original failure and backtrace.
pub(super) fn attempt<T>(
    service: &str,
    work: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
        .unwrap_or_else(|_| Err(format!("{service} panicked; see cagentsd-crash.log")))
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
