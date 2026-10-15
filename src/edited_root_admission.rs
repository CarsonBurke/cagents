//! Bounded window custody for original edited-root admissions.
//!
//! This owns no registry and grants no filesystem authority. A window reserves
//! a ticket before preparing a root; the actual authenticated client charges
//! native ownership in its existing RecoveryContext. Custody is installed
//! before any enrollment RPC, and sealed retirement custody before any release.
use crate::{fs_safety::Root, process::CancellationToken, recovery_domain::{EditedRootLease, EditedRootRetirement, RecoveryPlacementCustody, RecoveryRetirement}};
use std::{collections::HashMap, fmt, io, sync::{Arc, Mutex, MutexGuard}, time::Instant};
use uuid::Uuid;

const MAX_ADMISSIONS: usize = 32;
const MAX_NOTICE_CHARS: usize = 2048;

/// A resource correlation ID, never a registry epoch or effect capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AdmissionId(Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionPhase { Reserved, Preparing, Prepared, Enrolling, Active, RetirementRequested, Retiring, Interrupted, Settled }

#[derive(Debug)]
struct State {
    phase: AdmissionPhase,
    worker: bool,
    retiring: bool,
    retirement_armed: bool,
    admitted: bool,
    lost_owner: bool,
    owner: Option<Arc<EditedRootLease>>,
    retirement: Option<EditedRootRetirement>,
    notice: Option<String>,
}

/// Clone only this handle into task/receipt payloads. The window keeps its
/// original handle independently, before stale/card/generation filters.
#[derive(Debug, Clone)]
pub struct AdmissionTicket { id: AdmissionId, state: Arc<Mutex<State>> }

/// Actual window-owned custody. Never clear it to implement pane/project close.
/// Request retirement and keep the window/process alive until is_settled().
#[derive(Debug, Default)]
pub struct EditedRootAdmissions {
    entries: HashMap<AdmissionId, AdmissionTicket>,
    placements: HashMap<AdmissionId, PlacementTicket>,
}
/// App/Ceditor retain this original window owner. Each Runtime gets the SAME
/// Arc so internal file loads can reserve before constructing their first task.
pub type SharedEditedRootAdmissions = Arc<Mutex<EditedRootAdmissions>>;

#[derive(Debug, Clone)]
pub struct AdmissionFailure { message: String, ticket: AdmissionTicket }
impl fmt::Display for AdmissionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.message) }
}
impl std::error::Error for AdmissionFailure {}
impl AdmissionFailure {
    pub fn ticket(&self) -> &AdmissionTicket { &self.ticket }
}

/// A phase-one payload retains the original owner while its message waits.
/// This is not an Editable grant. Only consuming it can start enrollment.
#[derive(Debug, Clone)]
pub struct PreparedRoot { ticket: AdmissionTicket, _owner: Arc<EditedRootLease> }
impl PreparedRoot {
    pub fn ticket(&self) -> &AdmissionTicket { &self.ticket }
    pub fn begin_enrollment(self) -> Result<EnrollmentWork, AdmissionFailure> {
        // Set the in-flight fence BEFORE this payload releases its passive Arc.
        self.ticket.begin_enrollment()
    }
}

/// The successful async result retains both the actual lease and its custody
/// ticket. Consumers clone this SAME lease into Source/Doc/Undo/queued workers.
#[derive(Debug, Clone)]
pub struct AdmittedRoot { ticket: AdmissionTicket, lease: Arc<EditedRootLease> }
impl AdmittedRoot {
    pub fn ticket(&self) -> &AdmissionTicket { &self.ticket }
    pub fn lease(&self) -> Arc<EditedRootLease> { self.lease.clone() }
    /// Worker only. This retains and validates the SAME enrolled original
    /// lease against the supplied held Root; no pathname is recaptured.
    pub fn validate_for_root(&self, root: &Root, deadline: Instant) -> io::Result<()> {
        self.lease.validate_for_root(root, deadline)
    }
}

impl EditedRootAdmissions {
    /// Pure, zero-FD reservation BEFORE the phase-one worker. The actual client
    /// independently reserves its measured shared native context before RPC.
    pub fn reserve(&mut self) -> Result<AdmissionTicket, &'static str> {
        self.collect_settled();
        if self.len() >= MAX_ADMISSIONS {
            return Err("Root admission custody is full. Retire an earlier root before opening another.");
        }
        let ticket = AdmissionTicket { id: AdmissionId(Uuid::new_v4()), state: Arc::new(Mutex::new(State {
            phase: AdmissionPhase::Reserved, worker: false, retiring: false, retirement_armed: false, admitted: false, lost_owner: false,
            owner: None, retirement: None, notice: None,
        })) };
        self.entries.insert(ticket.id, ticket.clone());
        Ok(ticket)
    }

    /// A placement shares this SAME bounded pool. Its descriptor-free actual
    /// client custody slot is retained BEFORE constructing the intake worker.
    pub fn reserve_placement(&mut self) -> Result<PlacementTicket, &'static str> {
        self.collect_settled();
        if self.len() >= MAX_ADMISSIONS { return Err("Recovery admission custody is full. Settle an earlier original owner first."); }
        let ticket = PlacementTicket { id: AdmissionId(Uuid::new_v4()), custody: RecoveryPlacementCustody::new(), state: Arc::new(Mutex::new(PlacementState {
            phase: AdmissionPhase::Reserved, worker: false, retiring: false, armed: false, notice: None,
        })) };
        self.placements.insert(ticket.id, ticket.clone());
        Ok(ticket)
    }

    pub fn placement_tickets(&self) -> impl Iterator<Item = &PlacementTicket> { self.placements.values() }
    pub fn next_placement_retirement(&self) -> Option<PlacementRetirementWork> {
        self.placements.values().find_map(PlacementTicket::begin_retirement)
    }

    /// Pure original-Arc lookup for already enrolled Sources. Ordinary file
    /// loads in one checkout reuse that lease instead of spending a new context.
    /// A worker must still validate_for_root before Editable publication.
    pub fn ticket_for_lease(&self, lease: &Arc<EditedRootLease>) -> Option<AdmissionTicket> {
        self.entries.values().find(|ticket| {
            let state = ticket.lock();
            state.phase == AdmissionPhase::Active && !state.retiring
                && state.owner.as_ref().is_some_and(|owner| Arc::ptr_eq(owner, lease))
        }).cloned()
    }

    /// Produce one bounded cleanup work item without creating another keeper
    /// for an active lease. Call again after actual settlement, not in a loop
    /// that automatically retries an uncertain network effect.
    pub fn next_retirement(&self) -> Result<Option<RetirementWork>, AdmissionFailure> {
        let mut first_error = None;
        for ticket in self.entries.values() {
            match ticket.begin_retirement() {
                Ok(Some(work)) => return Ok(Some(work)),
                Ok(None) => {},
                Err(error) => { if first_error.is_none() { first_error = Some(error); } },
            }
        }
        match first_error { Some(error) => Err(error), None => Ok(None) }
    }

    /// Pure first cleanup admission after a stale/discarded result or final
    /// passive owner drops. Prepared payloads and active pairs retain raw Arcs;
    /// observers alone do not keep an abandoned root registered forever.
    /// Unknown/failed cleanup is never rearmed by this ordinary sweep.
    pub fn request_unowned_retirements(&self) {
        for ticket in self.entries.values() {
            let mut state = ticket.lock();
            if !state.worker && !state.retiring
                && matches!(state.phase, AdmissionPhase::Prepared | AdmissionPhase::Active)
                && state.owner.as_ref().is_some_and(|owner| Arc::strong_count(owner) == 1) {
                state.retiring = true;
                state.retirement_armed = true;
                state.phase = AdmissionPhase::RetirementRequested;
            }
        }
    }

    pub fn request_all_retirements(&self) {
        for ticket in self.entries.values() { ticket.request_retirement(); }
        for ticket in self.placements.values() { ticket.request_retirement(); }
    }
    pub fn is_settled(&self) -> bool {
        self.entries.values().all(|ticket| ticket.phase() == AdmissionPhase::Settled)
            && self.placements.values().all(|ticket| ticket.phase() == AdmissionPhase::Settled)
    }
    pub fn len(&self) -> usize { self.entries.len() + self.placements.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() && self.placements.is_empty() }
    pub fn tickets(&self) -> impl Iterator<Item = &AdmissionTicket> { self.entries.values() }
    pub fn collect_settled(&mut self) {
        self.entries.retain(|_, ticket| ticket.phase() != AdmissionPhase::Settled);
        self.placements.retain(|_, ticket| ticket.phase() != AdmissionPhase::Settled);
    }
}

impl AdmissionTicket {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Poison cannot erase original custody. This mutex never encloses IO,
        // native validation or a user callback; only typed in-memory state.
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn failure(&self, message: impl ToString) -> AdmissionFailure {
        AdmissionFailure { message: message.to_string().chars().take(MAX_NOTICE_CHARS).collect(), ticket: self.clone() }
    }
    pub fn id(&self) -> AdmissionId { self.id }
    pub fn phase(&self) -> AdmissionPhase { self.lock().phase }
    pub fn notice(&self) -> Option<String> { self.lock().notice.clone() }
    pub fn has_worker(&self) -> bool { self.lock().worker }

    /// Phase one may do worker-only capture/read and Connect/Catalog bootstrap,
    /// but no negative registration effects. Client owner preparation is NO RPC.
    /// Its guard marks interrupted work; it never issues Drop cleanup.
    pub fn begin_preparation(&self) -> Result<PreparationWork, AdmissionFailure> {
        let mut state = self.lock();
        if state.phase != AdmissionPhase::Reserved || state.worker || state.retiring {
            return Err(self.failure("Original root preparation is no longer available."));
        }
        state.worker = true;
        state.phase = AdmissionPhase::Preparing;
        Ok(PreparationWork { ticket: self.clone(), finished: false })
    }

    /// The phase-one worker must call prepared BEFORE producing its message.
    /// UI ingress does not need delivery to preserve this original owner.
    fn begin_enrollment(&self) -> Result<EnrollmentWork, AdmissionFailure> {
        let mut state = self.lock();
        if state.phase != AdmissionPhase::Prepared || state.worker || state.retiring || state.owner.is_none() {
            return Err(self.failure("Original root enrollment is unavailable; retain its admission ticket."));
        }
        state.worker = true;
        state.phase = AdmissionPhase::Enrolling;
        Ok(EnrollmentWork { ticket: self.clone(), finished: false })
    }

    /// A physical Root check still belongs to a worker. This pure getter only
    /// exposes a new pair only while the successfully enrolled original remains
    /// open. Existing Source/Doc/Undo pairs keep their lease during retirement.
    pub fn admitted(&self) -> Option<AdmittedRoot> {
        let state = self.lock();
        // Existing Doc/Undo/Source pairs retain their negative lease, but a
        // closed admission cannot mint new pairs for delayed publication.
        (state.admitted && !state.retiring).then(|| state.owner.as_ref().map(|lease| AdmittedRoot {
            ticket: self.clone(), lease: lease.clone(),
        })).flatten()
    }
    pub fn admitted_lease(&self) -> Option<Arc<EditedRootLease>> {
        self.admitted().map(|admitted| admitted.lease)
    }

    pub fn request_retirement(&self) {
        let mut state = self.lock();
        if state.phase == AdmissionPhase::Settled { return; }
        if !state.retiring { state.retirement_armed = true; }
        state.retiring = true;
        if !state.worker { state.phase = AdmissionPhase::RetirementRequested; }
    }
    /// Explicit retry keeps the original sealed receipt; it never re-enrolls or
    /// follows a new endpoint. Ordinary polling does not replay unknown cleanup.
    pub fn retry_retirement(&self) {
        let mut state = self.lock();
        if state.retiring && !state.worker && state.phase != AdmissionPhase::Settled {
            state.retirement_armed = true;
        }
    }

    /// Pure last-owner handoff, NO RPC/FS. Live Source/Doc/Undo/workers postpone
    /// it without invalidating their original negative lease. Store the sealed
    /// receipt here BEFORE returning a runnable retirement task to the caller.
    pub fn begin_retirement(&self) -> Result<Option<RetirementWork>, AdmissionFailure> {
        let mut state = self.lock();
        if !state.retiring || !state.retirement_armed || state.worker || state.phase == AdmissionPhase::Settled { return Ok(None); }
        if state.lost_owner {
            return Err(self.failure("Original admission custody is unavailable; explicit authenticated reconciliation is required."));
        }
        if state.retirement.is_none() {
            if state.owner.as_ref().is_some_and(|owner| Arc::strong_count(owner) != 1) { return Ok(None); }
            if let Some(owner) = state.owner.take() {
                match owner.prepare_retirement() {
                    Ok(receipt) => { state.admitted = false; state.retirement = Some(receipt); },
                    Err(error) => {
                        let (error, owner) = error.into_parts();
                        state.lost_owner = owner.is_none();
                        state.owner = owner;
                        state.notice = Some(error.to_string().chars().take(MAX_NOTICE_CHARS).collect());
                        return Err(self.failure(error));
                    }
                }
            } else {
                // No owner and no worker means no Prepare could have occurred:
                // phase one has no negative registration effects; enrollment requires custody.
                state.phase = AdmissionPhase::Settled;
                state.notice = None;
                return Ok(None);
            }
        }
        let receipt = state.retirement.as_ref().expect("sealed receipt installed above").clone();
        state.worker = true;
        state.retirement_armed = false;
        state.phase = AdmissionPhase::Retiring;
        Ok(Some(RetirementWork { ticket: self.clone(), receipt, finished: false }))
    }
}

#[derive(Debug)]
pub struct PreparationWork { ticket: AdmissionTicket, finished: bool }
impl PreparationWork {
    /// Store actual client.prepare_edited_root_owner result BEFORE any enroll
    /// RPC. Even a later panic or lost message leaves the window's SAME owner.
    pub fn prepared(mut self, owner: Arc<EditedRootLease>) -> PreparedRoot {
        let mut state = self.ticket.lock();
        debug_assert!(state.owner.is_none() && state.retirement.is_none());
        let payload = PreparedRoot { ticket: self.ticket.clone(), _owner: owner.clone() };
        state.owner = Some(owner);
        state.worker = false;
        state.phase = if state.retiring { AdmissionPhase::RetirementRequested } else { AdmissionPhase::Prepared };
        self.finished = true;
        drop(state);
        payload
    }
    /// Normal read-only/binary/PDF completion, or reuse of an existing validated
    /// AdmittedRoot pair. No new owner or negative registration effect exists.
    pub fn finish_read_only(mut self) -> AdmissionTicket {
        let mut state = self.ticket.lock();
        debug_assert!(state.owner.is_none() && state.retirement.is_none());
        state.worker = false;
        state.phase = AdmissionPhase::Settled;
        state.notice = None;
        self.finished = true;
        drop(state);
        self.ticket.clone()
    }
    pub fn failed(mut self, error: impl ToString) -> AdmissionFailure {
        let failure = self.ticket.failure(error);
        let mut state = self.ticket.lock();
        state.worker = false;
        state.retiring = true;
        state.retirement_armed = true;
        state.phase = AdmissionPhase::RetirementRequested;
        state.notice = Some(failure.message.clone());
        self.finished = true;
        drop(state);
        failure
    }
}

#[derive(Debug)]
pub struct EnrollmentWork { ticket: AdmissionTicket, finished: bool }
impl EnrollmentWork {
    /// Blocking worker. Only the original client-owned lease talks to the
    /// daemon; record outcome in preheld custody BEFORE returning any message.
    pub fn run(mut self, cancellation: &CancellationToken, deadline: Instant) -> Result<AdmittedRoot, AdmissionFailure> {
        let owner = self.ticket.lock().owner.as_ref().expect("enrollment requires installed owner").clone();
        let result = owner.enroll(cancellation, deadline);
        let mut state = self.ticket.lock();
        state.worker = false;
        self.finished = true;
        match result {
            Ok(()) if !state.retiring => {
                state.phase = AdmissionPhase::Active;
                state.admitted = true;
                state.notice = None;
                Ok(AdmittedRoot { ticket: self.ticket.clone(), lease: owner })
            }
            result => {
                let message = match result { Ok(()) => "The original root closed during enrollment; retirement remains pending.".to_owned(), Err(error) => error.to_string() };
                state.retiring = true;
                state.retirement_armed = true;
                state.phase = AdmissionPhase::RetirementRequested;
                state.notice = Some(message.chars().take(MAX_NOTICE_CHARS).collect());
                Err(self.ticket.failure(message))
            }
        }
    }
}

#[derive(Debug)]
pub struct RetirementWork { ticket: AdmissionTicket, receipt: EditedRootRetirement, finished: bool }
impl RetirementWork {
    /// Blocking explicit cleanup. Custody also holds a receipt clone throughout
    /// this RPC, so panic/lost delivery cannot lose the original sealed owner.
    pub fn run(mut self, deadline: Instant) -> Result<(), AdmissionFailure> {
        let result = self.receipt.retire(deadline);
        let mut state = self.ticket.lock();
        state.worker = false;
        self.finished = true;
        match result {
            Ok(()) => { state.phase = AdmissionPhase::Settled; state.retirement.take(); state.notice = None; Ok(()) }
            Err(error) => {
                state.phase = AdmissionPhase::RetirementRequested;
                state.notice = Some(error.to_string().chars().take(MAX_NOTICE_CHARS).collect());
                Err(self.ticket.failure(error))
            }
        }
    }
}

fn interrupted(ticket: &AdmissionTicket, finished: bool) {
    if finished { return; }
    let mut state = ticket.lock();
    state.worker = false;
    state.retiring = true;
    // Before cleanup starts there is still one initial retirement to perform.
    // A panicked release keeps its receipt but needs an explicit retry.
    state.retirement_armed = state.retirement.is_none();
    state.phase = AdmissionPhase::Interrupted;
    state.notice = Some("Original root worker was interrupted; custody is retained for explicit retirement.".into());
}
impl Drop for PreparationWork { fn drop(&mut self) { interrupted(&self.ticket, self.finished); } }
impl Drop for EnrollmentWork { fn drop(&mut self) { interrupted(&self.ticket, self.finished); } }
impl Drop for RetirementWork { fn drop(&mut self) { interrupted(&self.ticket, self.finished); } }

// Placement custody remains concrete: the actual client owns the only
// candidate/receipt transitions, with no validation callback or recreated Root.
#[derive(Debug)]
struct PlacementState {
    phase: AdmissionPhase, worker: bool, retiring: bool, armed: bool, notice: Option<String>,
}
#[derive(Debug, Clone)]
pub struct PlacementTicket { id: AdmissionId, custody: RecoveryPlacementCustody, state: Arc<Mutex<PlacementState>> }
#[derive(Debug, Clone)]
pub struct PlacementFailure { message: String, ticket: PlacementTicket }
impl fmt::Display for PlacementFailure { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.message) } }
impl std::error::Error for PlacementFailure {}
impl PlacementFailure { pub fn ticket(&self) -> &PlacementTicket { &self.ticket } }
impl PlacementTicket {
    fn lock(&self) -> MutexGuard<'_, PlacementState> { self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner) }
    pub fn id(&self) -> AdmissionId { self.id }
    pub fn phase(&self) -> AdmissionPhase { self.lock().phase }
    pub fn notice(&self) -> Option<String> { self.lock().notice.clone() }
    pub fn begin_issue(&self) -> Result<PlacementWork, PlacementFailure> {
        let mut state = self.lock();
        if state.phase != AdmissionPhase::Reserved || state.worker || state.retiring {
            return Err(PlacementFailure { message: "Original placement intake is unavailable; retain its custody.".into(), ticket: self.clone() });
        }
        state.worker = true;
        state.phase = AdmissionPhase::Enrolling;
        Ok(PlacementWork { ticket: self.clone(), finished: false })
    }
    pub fn request_retirement(&self) {
        // Atomic client seal: a queued-but-not-started intake cannot issue later.
        self.custody.seal();
        let mut state = self.lock();
        if state.phase == AdmissionPhase::Settled { return; }
        if !state.retiring { state.armed = true; }
        state.retiring = true;
        if !state.worker { state.phase = AdmissionPhase::RetirementRequested; }
    }
    pub fn retry_retirement(&self) {
        let mut state = self.lock();
        if state.retiring && !state.worker && state.phase != AdmissionPhase::Settled { state.armed = true; }
    }
    pub fn begin_retirement(&self) -> Option<PlacementRetirementWork> {
        let mut state = self.lock();
        if !state.retiring || !state.armed || state.worker || state.phase == AdmissionPhase::Settled { return None; }
        self.custody.seal();
        state.worker = true; state.armed = false; state.phase = AdmissionPhase::Retiring;
        Some(PlacementRetirementWork { ticket: self.clone(), finished: false })
    }
}
#[derive(Debug)]
pub struct PlacementWork { ticket: PlacementTicket, finished: bool }
impl PlacementWork {
    /// Pass this SAME preheld slot to the actual reserve_with_candidates_in_custody.
    /// The client installs each original candidate before its first Prepare.
    pub fn custody(&self) -> &RecoveryPlacementCustody { &self.ticket.custody }
    /// Record quiescence BEFORE publishing any success/error message. The
    /// caller's result still owns the actual issued context/typed error receipt;
    /// this ticket independently retains the same custody on message loss.
    pub fn finish(mut self) -> PlacementTicket {
        self.ticket.custody.seal();
        let mut state = self.ticket.lock();
        state.worker = false;
        state.phase = if state.retiring { AdmissionPhase::RetirementRequested } else { AdmissionPhase::Prepared };
        self.finished = true;
        drop(state); self.ticket.clone()
    }
}
impl Drop for PlacementWork {
    fn drop(&mut self) {
        if self.finished { return; }
        self.ticket.custody.seal();
        let mut state = self.ticket.lock();
        state.worker = false; state.retiring = true; state.armed = true;
        state.phase = AdmissionPhase::Interrupted;
        state.notice = Some("Original placement worker was interrupted; candidate custody is retained.".into());
    }
}
#[derive(Debug)]
pub struct PlacementRetirementWork { ticket: PlacementTicket, finished: bool }
impl PlacementRetirementWork {
    /// Blocking explicit cleanup. Receipt/candidate remains in the original
    /// slot throughout RPC and unwind. Never infer settlement from UI lifetime.
    pub fn run(mut self, deadline: Instant) -> Result<RecoveryRetirement, PlacementFailure> {
        self.ticket.custody.seal();
        let result = (|| -> io::Result<RecoveryRetirement> {
            if self.ticket.custody.is_inflight() { return Ok(RecoveryRetirement::PendingCleanup); }
            if let Some(receipt) = self.ticket.custody.receipt(deadline)? {
                if receipt.retire(deadline)? == RecoveryRetirement::PendingCleanup { return Ok(RecoveryRetirement::PendingCleanup); }
            }
            if self.ticket.custody.forget_settled(deadline)? { Ok(RecoveryRetirement::Settled) }
            else { Ok(RecoveryRetirement::PendingCleanup) }
        })();
        let mut state = self.ticket.lock(); state.worker = false; self.finished = true;
        match result {
            Ok(RecoveryRetirement::Settled) => { state.phase = AdmissionPhase::Settled; state.notice = None; Ok(RecoveryRetirement::Settled) }
            Ok(RecoveryRetirement::PendingCleanup) => { state.phase = AdmissionPhase::RetirementRequested; state.notice = Some("Original placement still owns pending cleanup; retain its context and retry explicitly after cleanup.".into()); Ok(RecoveryRetirement::PendingCleanup) }
            Err(error) => { let message = error.to_string().chars().take(MAX_NOTICE_CHARS).collect::<String>(); state.phase = AdmissionPhase::RetirementRequested; state.notice = Some(message.clone()); Err(PlacementFailure { message, ticket: self.ticket.clone() }) }
        }
    }
}
impl Drop for PlacementRetirementWork {
    fn drop(&mut self) {
        if self.finished { return; }
        let mut state = self.ticket.lock(); state.worker = false; state.retiring = true; state.armed = false;
        state.phase = AdmissionPhase::Interrupted;
        state.notice = Some("Original placement retirement was interrupted; explicit retry retains the same receipt.".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn root_admission_slots_are_bounded_before_any_worker_or_rpc() {
        let mut custody = EditedRootAdmissions::default();
        let tickets: Vec<_> = (0..MAX_ADMISSIONS).map(|_| custody.reserve().unwrap()).collect();
        assert!(custody.reserve().is_err());
        assert!(!custody.is_settled());
        tickets[0].request_retirement();
        assert!(tickets[0].begin_retirement().unwrap().is_none());
        custody.collect_settled();
        assert_eq!(custody.len(), MAX_ADMISSIONS - 1);
        assert!(custody.reserve().is_ok());
    }
    #[test]
    fn phase_one_panic_keeps_ticket_visible_and_never_sends_drop_cleanup() {
        let mut custody = EditedRootAdmissions::default();
        let ticket = custody.reserve().unwrap();
        let worker = ticket.begin_preparation().unwrap();
        assert!(ticket.begin_preparation().is_err());
        assert!(ticket.begin_enrollment().is_err());
        let result = std::panic::catch_unwind(|| { let _worker = worker; panic!("phase one before any RPC"); });
        assert!(result.is_err());
        assert_eq!(ticket.phase(), AdmissionPhase::Interrupted);
        assert!(!ticket.has_worker());
        assert!(ticket.admitted().is_none());
        ticket.request_retirement();
        assert!(ticket.begin_retirement().unwrap().is_none());
        assert!(custody.is_settled());
    }
    #[test]
    fn closing_before_phase_one_finishes_waits_for_its_actual_worker() {
        let mut custody = EditedRootAdmissions::default();
        let ticket = custody.reserve().unwrap();
        let work = ticket.begin_preparation().unwrap();
        custody.request_all_retirements();
        assert!(ticket.begin_retirement().unwrap().is_none());
        assert!(!custody.is_settled());
        let failure = work.failed("read refused before owner preparation");
        assert_eq!(failure.ticket().id(), ticket.id());
        assert!(ticket.begin_retirement().unwrap().is_none());
        assert!(custody.is_settled());
        custody.collect_settled();
        assert!(custody.is_empty());
    }

    #[cfg(target_os = "linux")]
    fn private_catalog_client() -> (tempfile::TempDir, crate::recovery_domain::RecoveryPlacementClient, std::thread::JoinHandle<()>) {
        use crate::terminal_ipc::{read_frame, write_frame, ClientEnvelope, ClientRequest, ServerEnvelope, ServerMessage, ServerResponse, PROTOCOL_VERSION};
        use crate::model::{BootId, HostSessionId, HostIncarnation, HostSessionProof};
        use crate::recovery_domain::{LocalRecoveryEndpoint, Operation, Reply, RecoveryPlacementClient};
        use std::{io, os::unix::{fs::PermissionsExt, net::UnixListener}, thread, time::Duration};
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path().join("private-admission.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let registry = Uuid::new_v4();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "owned private client did not connect");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("private accept: {error}"),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
            let proof = HostSessionProof { boot_id: BootId::new("fixture-boot"), host_session_id: HostSessionId::new("fixture-session"), host_incarnation: HostIncarnation::new("fixture-incarnation") };
            let hello: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(hello.request, ClientRequest::Connect { .. }));
            write_frame(&mut stream, &ServerEnvelope { version: PROTOCOL_VERSION, request_id: Some(hello.request_id), proof: proof.clone(), message: ServerMessage::Response { response: ServerResponse::Connected { proof: proof.clone(), resumed: vec![] } } }).unwrap();
            let catalog: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(catalog.request, ClientRequest::RecoveryPlacement { operation: Operation::Catalog }));
            write_frame(&mut stream, &ServerEnvelope { version: PROTOCOL_VERSION, request_id: Some(catalog.request_id), proof, message: ServerMessage::Response { response: ServerResponse::RecoveryPlacement { reply: Reply::Catalog { registry, projects: vec![] } } } }).unwrap();
            // These contracts exercise NO-RPC ownership preparation/retirement.
            // Any unexpected Prepare/Release fails rather than being ignored.
            assert!(read_frame::<ClientEnvelope>(&mut stream).unwrap().is_none());
        });
        let endpoint = LocalRecoveryEndpoint::fixture(socket, Some(registry)).unwrap();
        let client = RecoveryPlacementClient::connect_local_until(endpoint, Instant::now() + Duration::from_secs(2)).unwrap();
        (directory, client, server)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn lost_phase_one_delivery_keeps_actual_original_owner_and_retirement_custody() {
        use crate::fs_safety::Root;
        use std::time::Duration;
        let (_runtime, client, server) = private_catalog_client();
        let files = tempfile::tempdir().unwrap();
        let root = Arc::new(Root::open(files.path()).unwrap());
        let original = Arc::downgrade(&root);
        let mut custody = EditedRootAdmissions::default();
        let ticket = custody.reserve().unwrap();
        let preparation = ticket.begin_preparation().unwrap();
        let owner = client.prepare_edited_root_owner(root).unwrap();
        let lost_message = preparation.prepared(owner);
        drop(lost_message);
        assert!(original.upgrade().is_some(), "original charged Root must survive message loss");
        assert_eq!(ticket.phase(), AdmissionPhase::Prepared);
        ticket.request_retirement();
        let work = custody.next_retirement().unwrap().unwrap();
        assert!(original.upgrade().is_some(), "sealed receipt retained BEFORE cleanup starts");
        drop(work); // lost task/producer: Drop is memory-only and cannot release.
        assert_eq!(ticket.phase(), AdmissionPhase::Interrupted);
        assert!(original.upgrade().is_some());
        assert!(custody.next_retirement().unwrap().is_none(), "uncertain cleanup is not automatically replayed");
        ticket.retry_retirement();
        custody.next_retirement().unwrap().unwrap().run(Instant::now() + Duration::from_secs(2)).unwrap();
        assert!(original.upgrade().is_none(), "positive effect-free settlement drops original Root");
        assert!(custody.is_settled());
        custody.collect_settled();
        assert!(custody.is_empty());
        drop(client); server.join().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn actual_passive_lease_owners_delay_retirement_but_status_tickets_do_not() {
        use crate::fs_safety::Root;
        use std::time::Duration;
        let (_runtime, client, server) = private_catalog_client();
        let files = tempfile::tempdir().unwrap();
        let root = Arc::new(Root::open(files.path()).unwrap());
        let mut custody = EditedRootAdmissions::default();
        let ticket = custody.reserve().unwrap();
        let prepared = client.prepare_edited_root_owner(root).unwrap();
        let passive_original = prepared.clone();
        ticket.begin_preparation().unwrap().prepared(prepared);
        let diagnostic = ticket.clone();
        ticket.request_retirement();
        assert!(custody.next_retirement().unwrap().is_none(), "actual original Arc owner cannot be sealed early");
        drop(passive_original);
        custody.next_retirement().unwrap().unwrap().run(Instant::now() + Duration::from_secs(2)).unwrap();
        assert_eq!(diagnostic.phase(), AdmissionPhase::Settled, "diagnostic ticket is not an Editable owner");
        drop(client); server.join().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn close_during_actual_phase_one_retains_late_owner_without_enrolling_it() {
        use crate::fs_safety::Root;
        use std::time::Duration;
        let (_runtime, client, server) = private_catalog_client();
        let files = tempfile::tempdir().unwrap();
        let root = Arc::new(Root::open(files.path()).unwrap());
        let mut custody = EditedRootAdmissions::default();
        let ticket = custody.reserve().unwrap();
        let work = ticket.begin_preparation().unwrap();
        custody.request_all_retirements();
        assert!(custody.next_retirement().unwrap().is_none());
        let late = work.prepared(client.prepare_edited_root_owner(root).unwrap());
        assert!(late.begin_enrollment().is_err(), "closed original source cannot publish a new editable actor");
        custody.next_retirement().unwrap().unwrap().run(Instant::now() + Duration::from_secs(2)).unwrap();
        assert!(custody.is_settled());
        drop(client); server.join().unwrap();
    }


    #[test]
    fn normal_read_only_or_reused_root_completion_settles_only_the_unused_ticket() {
        let mut custody = EditedRootAdmissions::default();
        let ticket = custody.reserve().unwrap();
        let work = ticket.begin_preparation().unwrap();
        let diagnostic = work.finish_read_only();
        assert_eq!(diagnostic.id(), ticket.id());
        assert_eq!(ticket.phase(), AdmissionPhase::Settled);
        assert!(ticket.notice().is_none());
        assert!(custody.is_settled());
        custody.collect_settled();
        assert!(custody.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn automatic_first_retirement_waits_for_the_actual_phase_one_payload() {
        use crate::fs_safety::Root;
        use std::time::Duration;
        let (_runtime, client, server) = private_catalog_client();
        let files = tempfile::tempdir().unwrap();
        let root = Arc::new(Root::open(files.path()).unwrap());
        let mut custody = EditedRootAdmissions::default();
        let ticket = custody.reserve().unwrap();
        let payload = ticket.begin_preparation().unwrap().prepared(client.prepare_edited_root_owner(root).unwrap());
        let delayed_copy = payload.clone();
        custody.request_unowned_retirements();
        assert_eq!(ticket.phase(), AdmissionPhase::Prepared, "queued payload is a real passive original owner");
        drop(payload);
        custody.request_unowned_retirements();
        assert_eq!(ticket.phase(), AdmissionPhase::Prepared, "every queued clone must drain");
        drop(delayed_copy);
        custody.request_unowned_retirements();
        assert_eq!(ticket.phase(), AdmissionPhase::RetirementRequested);
        custody.next_retirement().unwrap().unwrap().run(Instant::now() + Duration::from_secs(2)).unwrap();
        assert!(custody.is_settled());
        drop(client); server.join().unwrap();
    }


    #[test]
    fn root_and_placement_tickets_share_one_preworker_capacity() {
        let mut custody = EditedRootAdmissions::default();
        for _ in 0..MAX_ADMISSIONS - 1 { custody.reserve().unwrap(); }
        let placement = custody.reserve_placement().unwrap();
        assert!(custody.reserve().is_err());
        assert!(custody.reserve_placement().is_err());
        placement.request_retirement();
        custody.next_placement_retirement().unwrap().run(Instant::now() + std::time::Duration::from_secs(1)).unwrap();
        custody.collect_settled();
        assert_eq!(custody.len(), MAX_ADMISSIONS - 1);
        assert!(custody.reserve().is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dropped_placement_worker_keeps_slot_and_seals_lazy_client_after_positive_empty_settlement() {
        use crate::fs_safety::Root;
        use crate::recovery_domain::RecoveryPlacementInputs;
        use std::time::Duration;
        let (_runtime, client, server) = private_catalog_client();
        let mut admissions = EditedRootAdmissions::default();
        let ticket = admissions.reserve_placement().unwrap();
        let work = ticket.begin_issue().unwrap();
        let original_slot = work.custody().clone();
        drop(work);
        assert_eq!(ticket.phase(), AdmissionPhase::Interrupted);
        admissions.next_placement_retirement().unwrap().run(Instant::now() + Duration::from_secs(2)).unwrap();
        assert_eq!(ticket.phase(), AdmissionPhase::Settled);
        let files = tempfile::tempdir().unwrap();
        let source = Arc::new(Root::open(files.path()).unwrap());
        let error = client.reserve_with_candidates_in_custody(RecoveryPlacementInputs {
            observed_registry: None, observed_projects: vec![], held_roots: vec![],
        }, source, Arc::new(None), vec![], 1, &original_slot, &CancellationToken::new(), Instant::now() + Duration::from_secs(2)).unwrap_err();
        assert!(error.to_string().contains("sealed"), "lazy worker must refuse before Prepare");
        drop(client); server.join().unwrap();
    }

}
