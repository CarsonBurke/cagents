//! Exact, process-local ownership of one terminal start attempt.
//! A failed caller cannot donate an unknown helper/PTY to another launch.
use super::{TerminalHandle, ipc::TerminalDescriptor};
use crate::model::{TerminalId, TerminalSession};
use std::{
    fs::File,
    os::fd::AsRawFd,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

const PREPARED: u8 = 0;
const RUNNING: u8 = 1;
const FAILED: u8 = 2;
const PUBLISHED: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalStartDisposition {
    Pending,
    /// This ended invocation never crossed the helper/provider creation cut.
    /// It says nothing about unrelated registry metadata written by its caller.
    NoProvider,
    Unknown,
    /// Exact v5 family cleanup and held helper exit were positively proved.
    FamilySettled,
    /// Recover this exact published worker; never launch another one instead.
    Published,
}

#[derive(Clone)]
pub(crate) struct TerminalStartAttempt(Arc<Inner>);
struct Inner {
    id: uuid::Uuid,
    session: TerminalSession,
    phase: AtomicU8,
    worker_admitted: AtomicBool,
    worker_start_finished: AtomicBool,
    helper_possible: AtomicBool,
    family_settled: AtomicBool,
    reconciliation_queued: AtomicBool,
    #[cfg(test)]
    lose_start_reply: AtomicBool,
    worker_instance: OnceLock<uuid::Uuid>,
    worker: Mutex<Option<TerminalHandle>>,
    published: Mutex<Option<(TerminalDescriptor, TerminalHandle)>>,
    proof: OnceLock<Proof>,
    cleanup: Mutex<Option<std::sync::mpsc::Receiver<super::CompletedRelease>>>,
}
struct Proof {
    record: super::holder::Record,
    source: super::holder::cleanup::SourceReceipt,
    holder: File,
}

impl std::fmt::Debug for TerminalStartAttempt {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("TerminalStartAttempt")
            .field("id", &self.id())
            .field("terminal", &self.terminal_id())
            .field("disposition", &self.disposition())
            .finish()
    }
}
impl TerminalStartAttempt {
    pub(crate) fn new(session: TerminalSession) -> Self {
        Self(Arc::new(Inner {
            id: uuid::Uuid::new_v4(),
            session,
            phase: AtomicU8::new(PREPARED),
            worker_admitted: AtomicBool::new(false),
            worker_start_finished: AtomicBool::new(false),
            helper_possible: AtomicBool::new(false),
            family_settled: AtomicBool::new(false),
            reconciliation_queued: AtomicBool::new(false),
            #[cfg(test)]
            lose_start_reply: AtomicBool::new(false),
            worker_instance: OnceLock::new(),
            worker: Mutex::new(None),
            published: Mutex::new(None),
            proof: OnceLock::new(),
            cleanup: Mutex::new(None),
        }))
    }
    #[cfg(test)]
    pub(crate) fn lose_worker_start_reply_for_test(&self) {
        self.0.lose_start_reply.store(true, Ordering::Release);
    }
    pub(crate) fn id(&self) -> uuid::Uuid {
        self.0.id
    }
    pub(crate) fn terminal_id(&self) -> TerminalId {
        self.0.session.id
    }
    pub(crate) fn session(&self) -> &TerminalSession {
        &self.0.session
    }
    /// Resident-only classification. No RPC, filesystem probe or guessed PID.
    pub(crate) fn disposition(&self) -> TerminalStartDisposition {
        match self.0.phase.load(Ordering::Acquire) {
            PUBLISHED => TerminalStartDisposition::Published,
            FAILED if self.0.family_settled.load(Ordering::Acquire) => {
                TerminalStartDisposition::FamilySettled
            }
            FAILED
                if !self.0.helper_possible.load(Ordering::Acquire)
                    && (!self.0.worker_admitted.load(Ordering::Acquire)
                        || self.0.worker_start_finished.load(Ordering::Acquire)) =>
            {
                TerminalStartDisposition::NoProvider
            }
            FAILED => TerminalStartDisposition::Unknown,
            _ => TerminalStartDisposition::Pending,
        }
    }
    pub(crate) fn published(&self) -> Option<(TerminalDescriptor, TerminalHandle)> {
        if self.disposition() != TerminalStartDisposition::Published {
            return None;
        }
        self.0.published.try_lock().ok()?.clone()
    }
    pub(crate) fn worker_instance(&self) -> Option<uuid::Uuid> {
        self.0.worker_instance.get().copied()
    }
    pub(crate) fn holder_record(&self) -> Option<super::holder::Record> {
        self.0.proof.get().map(|proof| proof.record.clone())
    }

    pub(crate) fn begin(&self, session: &TerminalSession) -> Result<AttemptRun, String> {
        if session != &self.0.session {
            return Err(
                "The terminal start attempt does not match its exact issued session".into(),
            );
        }
        self.0.phase.compare_exchange(PREPARED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "This terminal start was already attempted; reconcile its exact retained ticket instead of spawning again".to_string())?;
        Ok(AttemptRun(self.clone()))
    }

    /// Explicit failed-start recovery only. Call off HostLock. Keep this ticket
    /// on every unknown/error: queue admission and child exit alone prove nothing.
    pub(crate) fn retry_cleanup(&self) -> TerminalStartDisposition {
        if self.disposition() != TerminalStartDisposition::Unknown {
            return self.disposition();
        }
        if self
            .0
            .reconciliation_queued
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            if TerminalHandle::reconcile_start_attempt(self.clone()).is_err() {
                self.0.reconciliation_queued.store(false, Ordering::Release);
            }
        }
        self.disposition()
    }

    /// Runs only on the existing fixed cleanup service. The caller never waits
    /// behind receipt IO or a contended exact original worker.
    pub(in crate::terminal) fn reconcile_cleanup_owned(&self) {
        struct Finished(Arc<Inner>);
        impl Drop for Finished {
            fn drop(&mut self) {
                self.0.reconciliation_queued.store(false, Ordering::Release);
            }
        }
        let _finished = Finished(self.0.clone());
        if self.0.phase.load(Ordering::Acquire) != FAILED {
            return;
        }
        if let Some(proof) = self.0.proof.get() {
            let mut holder = libc::pollfd {
                fd: proof.holder.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut holder, 1, 0) } == 1
                && holder.revents & libc::POLLIN != 0
                && proof.source.confirmed(&proof.record).unwrap_or(false)
            {
                self.0.family_settled.store(true, Ordering::Release);
            }
        }
        if matches!(
            self.disposition(),
            TerminalStartDisposition::NoProvider | TerminalStartDisposition::FamilySettled
        ) {
            return;
        }
        let worker = self
            .0
            .worker
            .try_lock()
            .ok()
            .and_then(|worker| worker.clone());
        if let Some(worker) = worker {
            worker.request_shutdown();
            if let Ok(mut cleanup) = self.0.cleanup.try_lock() {
                if let Some(result) = cleanup.as_ref() {
                    match result.try_recv() {
                        Ok(_) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            *cleanup = None;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    }
                }
                if cleanup.is_none() && worker.is_completed() {
                    *cleanup = worker.release_completed_holder_async().ok();
                }
            }
        }
        // Only the exact Holder's positive callback or verified durable proof
        // may set family_settled. Even a cleanup service result is not a new proof.
    }
}

pub(crate) struct AttemptRun(TerminalStartAttempt);
impl AttemptRun {
    pub(in crate::terminal) fn observer(&self) -> StartObserver {
        StartObserver(Arc::downgrade(&self.0.0))
    }
    pub(crate) fn publish(
        &self,
        descriptor: TerminalDescriptor,
        worker: TerminalHandle,
    ) -> Result<(), String> {
        if descriptor.session.id != self.0.terminal_id()
            || descriptor.session.launch != self.0.session().launch
            || descriptor.execution_instance != worker.execution_instance()
            || Some(descriptor.execution_instance) != self.0.worker_instance()
        {
            return Err(
                "The terminal publication does not match its original start attempt".into(),
            );
        }
        *self.0.0.published.lock().unwrap_or_else(|p| p.into_inner()) = Some((descriptor, worker));
        self.0.0.phase.store(PUBLISHED, Ordering::Release);
        Ok(())
    }
}
impl Drop for AttemptRun {
    fn drop(&mut self) {
        let _ =
            self.0
                .0
                .phase
                .compare_exchange(RUNNING, FAILED, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// Held by the actual worker/helper cleanup owner, with no strong Handle cycle.
#[derive(Clone)]
pub(in crate::terminal) struct StartObserver(Weak<Inner>);
impl StartObserver {
    #[cfg(test)]
    pub(in crate::terminal) fn lose_start_reply(&self) -> bool {
        self.0
            .upgrade()
            .is_some_and(|inner| inner.lose_start_reply.swap(false, Ordering::AcqRel))
    }
    pub(in crate::terminal) fn worker_admitted(&self, instance: uuid::Uuid) {
        if let Some(inner) = self.0.upgrade() {
            let _ = inner.worker_instance.set(instance);
            inner.worker_admitted.store(true, Ordering::Release);
        }
    }
    pub(in crate::terminal) fn worker_start_finished(&self) {
        if let Some(inner) = self.0.upgrade() {
            inner.worker_start_finished.store(true, Ordering::Release);
        }
    }
    pub(in crate::terminal) fn worker_created(&self, handle: TerminalHandle) {
        if let Some(inner) = self.0.upgrade() {
            *inner.worker.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
        }
    }
    pub(in crate::terminal) fn before_helper_spawn(&self) {
        if let Some(inner) = self.0.upgrade() {
            inner.helper_possible.store(true, Ordering::Release);
        }
    }
    pub(in crate::terminal) fn holder(
        &self,
        record: &super::holder::Record,
        source: &super::holder::cleanup::SourceReceipt,
        holder: &File,
    ) {
        if let Some(inner) = self.0.upgrade() {
            if record.terminal != inner.session.id {
                return;
            }
            if let Ok(holder) = holder.try_clone() {
                let _ = inner.proof.set(Proof {
                    record: record.clone(),
                    source: source.clone(),
                    holder,
                });
            }
        }
    }
    pub(in crate::terminal) fn family_settled(&self) {
        if let Some(inner) = self.0.upgrade() {
            inner.family_settled.store(true, Ordering::Release);
        }
    }
}
