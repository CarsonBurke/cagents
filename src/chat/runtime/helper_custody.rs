//! The private helper owns every local worker before its startup gate opens.
//! Frontend destruction stays nonblocking; only this helper can hand its
//! process-wide child waiter to the fallback reaper.
use super::LocalHandle;
use std::{
    sync::{Arc, Mutex, MutexGuard, atomic::Ordering},
    time::Instant,
};

#[derive(Clone, Default)]
pub(super) struct HelperCustody(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    sealed: bool,
    actors: Vec<LocalHandle>,
}

pub(super) struct Admission<'a>(MutexGuard<'a, State>);

/// Holding this fence prevents any new worker from starting until reaping
/// finishes. Its private constructor requires every original owner to have
/// completed clean teardown and an actual OS-thread join.
pub(super) struct OrphanReapPermit<'a> {
    _creation_fence: MutexGuard<'a, State>,
}

impl HelperCustody {
    pub(super) fn admit(&self) -> Result<Admission<'_>, String> {
        let state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if state.sealed {
            return Err("Retained helper is stopping; provider creation is sealed".into());
        }
        Ok(Admission(state))
    }

    pub(super) fn seal(&self) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).sealed = true;
    }

    pub(super) fn reap_permit_until(
        &self,
        deadline: Instant,
    ) -> Result<OrphanReapPermit<'_>, String> {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        // Workers and the join service never acquire this admission lock.
        // Stop every generation before waiting for any particular worker.
        for actor in &state.actors {
            actor.0.stop.store(true, Ordering::Release);
        }
        for actor in &state.actors {
            actor.wake();
        }
        for actor in &state.actors {
            // Ok requires the original JoinHandle's actual join (including
            // TLS/destructors) AND close_result's clean teardown receipt.
            // Timeout, full/failed join admission, or panic keeps every owner
            // in this scope. None of those outcomes admits fallback reaping.
            actor.close_until(deadline)?;
        }
        state.actors.clear();
        Ok(OrphanReapPermit {
            _creation_fence: state,
        })
    }
}

impl Admission<'_> {
    /// Called only after installing the original worker, before opening its
    /// startup gate. No fallible publication may intervene.
    pub(super) fn register(&mut self, actor: &LocalHandle) {
        self.0.actors.push(actor.clone());
    }
}
