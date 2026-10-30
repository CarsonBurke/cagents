//! Internal retired-history candidates and reader custody, without a wire entry.
//!
//! Slot admission is resource bookkeeping, never History permission. The actual
//! opener must first compose the qualified recorded-namespace implementation
//! and repeat current control/startup/role/account/worktree/managed/owner and
//! physical-file admission. No opener or provider operation is exposed here.
use super::super::{
    delivery::DeliveryQueue,
    model::RetiredHistoryCandidate,
    state::{ClientId, HostState, SharedHost},
};
use crate::{chat, model::HostSessionProof};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use uuid::Uuid;

const MAX_READERS: usize = 16;
const OPEN_TIME: Duration = Duration::from_secs(30);
const PAGE_TIME: Duration = Duration::from_secs(10);
const SETTLEMENT_OBSERVATION: Duration = Duration::from_secs(10);

/// A selector only. This is deliberately not a serialized request.
pub(super) struct ReadRequest {
    pub(super) source: chat::Session,
}

impl std::fmt::Debug for ReadRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OriginalHistoryReadRequest")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadError {
    ConnectionChanged,
    AgentRefused,
    ShuttingDown,
    ReaderLimit,
    ConnectionOccupied,
    SourceChanged,
    Revoked,
    Busy,
    Deadline,
    WorkerFailed,
    WorkerPanicked,
    CustodyMismatch,
}

/// Captured from the actual registered server connection, not a client route.
/// This pins one process-local registration only. It is not a durable anti-copy
/// proof or a complete current read-admission witness.
#[derive(Clone)]
pub(super) struct OriginalConnection {
    client: ClientId,
    delivery: Weak<DeliveryQueue>,
    epoch: HostSessionProof,
}

impl std::fmt::Debug for OriginalConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OriginalHistoryConnection")
    }
}

impl OriginalConnection {
    pub(super) fn capture(state: &HostState, client: ClientId) -> Result<Self, ReadError> {
        if state.shutting_down {
            return Err(ReadError::ShuttingDown);
        }
        if state.agent_clients.contains_key(&client) {
            return Err(ReadError::AgentRefused);
        }
        let delivery = state
            .clients
            .get(&client)
            .ok_or(ReadError::ConnectionChanged)?;
        Ok(Self {
            client,
            delivery: Arc::downgrade(delivery),
            epoch: state.proof().clone(),
        })
    }

    pub(super) fn current(&self, state: &HostState) -> Result<(), ReadError> {
        if state.shutting_down {
            return Err(ReadError::ShuttingDown);
        }
        if state.agent_clients.contains_key(&self.client) {
            return Err(ReadError::AgentRefused);
        }
        if state.proof() != &self.epoch
            || !state.clients.get(&self.client).is_some_and(|current| {
                self.delivery
                    .upgrade()
                    .is_some_and(|original| Arc::ptr_eq(current, &original))
            })
        {
            return Err(ReadError::ConnectionChanged);
        }
        Ok(())
    }

    fn same(&self, other: &Self) -> bool {
        self.client == other.client
            && self.epoch == other.epoch
            && Weak::ptr_eq(&self.delivery, &other.delivery)
    }
}

/// Data-reader correlation. It is neither an actor/terminal allocation nor a
/// HostSessionProof and has no provider-side operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ReadBinding {
    client: ClientId,
    nonce: Uuid,
}

/// Rendering data only. It cannot carry Ready, a request/permission grant,
/// execution activity, provider completion or native ownership evidence.
pub(super) struct TranscriptOnly {
    binding: ReadBinding,
    original: Arc<RetiredHistoryCandidate>,
    page_generation: u64,
    items: Vec<Arc<chat::Item>>,
    subagents: Vec<chat::Subagent>,
    has_earlier: bool,
    has_later: bool,
}

impl std::fmt::Debug for TranscriptOnly {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OriginalHistoryTranscriptOnly")
    }
}

/// Internal response shape only; no router or serialized variant is enrolled.
pub(super) enum ReadReply {
    Transcript(TranscriptOnly),
    Unavailable(ReadError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Opening,
    Readable,
    Revoked,
    Settling,
}

struct Operation {
    id: Uuid,
    deadline: Instant,
}

struct Slot {
    binding: ReadBinding,
    connection: OriginalConnection,
    candidate: Arc<RetiredHistoryCandidate>,
    cancelled: Arc<AtomicBool>,
    phase: Phase,
    operation: Option<Operation>,
    worker: Option<ReaderWorker>,
}

/// No live provider Handle is stored under HostState. A qualified future opener
/// keeps its actual reader/cache/files in this one worker, drops them there,
/// and returns only after their cleanup. The join remains separately owned.
struct ReaderWorker {
    thread: JoinHandle<Result<(), ReadError>>,
}

impl std::fmt::Debug for ReaderWorker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OriginalHistoryWorker")
    }
}

/// A consumed launch reservation cannot also report a pre-spawn failure.
/// Only this module's future qualified opener may launch/install a worker.
struct Opening {
    binding: ReadBinding,
    operation: Uuid,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

struct PageOperation {
    binding: ReadBinding,
    operation: Uuid,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl Opening {
    fn cancelled_or_expired(&self, now: Instant) -> bool {
        self.cancelled.load(Ordering::Acquire) || now >= self.deadline
    }
}

impl PageOperation {
    fn cancelled_or_expired(&self, now: Instant) -> bool {
        self.cancelled.load(Ordering::Acquire) || now >= self.deadline
    }
}

/// Metadata and original join custody remain counted in every phase. A taken
/// settlement leaves its slot present until an actual off-lock join returns.
#[derive(Default)]
pub(in crate::host) struct ReaderSlots {
    slots: BTreeMap<ClientId, Slot>,
    closed: bool,
    custody_problem: bool,
    unmatched_settlements: Vec<Settlement>,
}

impl ReaderSlots {
    fn reserve(
        &mut self,
        connection: OriginalConnection,
        candidate: Arc<RetiredHistoryCandidate>,
        now: Instant,
    ) -> Result<Opening, ReadError> {
        if self.closed {
            return Err(ReadError::ShuttingDown);
        }
        if self.slots.contains_key(&connection.client) {
            return Err(ReadError::ConnectionOccupied);
        }
        if self.slots.len() + self.unmatched_settlements.len() >= MAX_READERS {
            return Err(ReadError::ReaderLimit);
        }
        let binding = ReadBinding {
            client: connection.client,
            nonce: Uuid::new_v4(),
        };
        let operation = Uuid::new_v4();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.slots.insert(
            connection.client,
            Slot {
                binding,
                connection,
                candidate,
                cancelled: cancelled.clone(),
                phase: Phase::Opening,
                operation: Some(Operation {
                    id: operation,
                    deadline: now + OPEN_TIME,
                }),
                worker: None,
            },
        );
        Ok(Opening {
            binding,
            operation,
            cancelled,
            deadline: now + OPEN_TIME,
        })
    }

    /// This is only the no-worker branch of an actual failed spawn. There is
    /// no timeout/detach path which may call it instead of joining a worker.
    fn failed_before_spawn(&mut self, opening: Opening) -> Result<(), ReadError> {
        let slot = self
            .slots
            .get(&opening.binding.client)
            .ok_or(ReadError::CustodyMismatch)?;
        if slot.binding != opening.binding
            || slot.worker.is_some()
            || !slot
                .operation
                .as_ref()
                .is_some_and(|operation| operation.id == opening.operation)
        {
            return Err(ReadError::CustodyMismatch);
        }
        self.slots.remove(&opening.binding.client);
        Ok(())
    }

    /// A failed insertion returns the original join owner; no rejection drops
    /// or joins it under the caller's HostState lock.
    fn install_worker(
        &mut self,
        opening: Opening,
        worker: ReaderWorker,
    ) -> Result<(), ReaderWorker> {
        let Some(slot) = self.slots.get_mut(&opening.binding.client) else {
            return Err(worker);
        };
        if slot.binding != opening.binding
            || slot.worker.is_some()
            || !slot
                .operation
                .as_ref()
                .is_some_and(|operation| operation.id == opening.operation)
            || !Arc::ptr_eq(&slot.cancelled, &opening.cancelled)
        {
            return Err(worker);
        }
        // Cancellation may beat spawn publication. Still enroll and settle
        // that actual worker; it cannot publish data or free its own credit.
        slot.worker = Some(worker);
        Ok(())
    }

    fn check_operation(
        &self,
        connection: &OriginalConnection,
        candidate: &RetiredHistoryCandidate,
        binding: ReadBinding,
        operation: Uuid,
        now: Instant,
    ) -> Result<(), ReadError> {
        let slot = self
            .slots
            .get(&binding.client)
            .filter(|slot| slot.binding == binding)
            .ok_or(ReadError::SourceChanged)?;
        if !slot.connection.same(connection) || slot.candidate.as_ref() != candidate {
            return Err(ReadError::SourceChanged);
        }
        if slot.cancelled.load(Ordering::Acquire)
            || matches!(slot.phase, Phase::Revoked | Phase::Settling)
        {
            return Err(ReadError::Revoked);
        }
        let active = slot.operation.as_ref().ok_or(ReadError::Busy)?;
        if active.id != operation {
            return Err(ReadError::Busy);
        }
        if now >= active.deadline {
            return Err(ReadError::Deadline);
        }
        Ok(())
    }

    /// Bookkeeping only, after a real opener/publication has independently
    /// repeated every current physical/role/account/file fence.
    fn finish_open(
        &mut self,
        binding: ReadBinding,
        operation: Uuid,
        now: Instant,
    ) -> Result<(), ReadError> {
        let slot = self
            .slots
            .get_mut(&binding.client)
            .filter(|slot| slot.binding == binding)
            .ok_or(ReadError::SourceChanged)?;
        if slot.phase != Phase::Opening
            || slot.worker.is_none()
            || slot.cancelled.load(Ordering::Acquire)
            || !slot
                .operation
                .as_ref()
                .is_some_and(|active| active.id == operation)
        {
            return Err(ReadError::Revoked);
        }
        if now >= slot.operation.as_ref().unwrap().deadline {
            return Err(ReadError::Deadline);
        }
        slot.operation = None;
        slot.phase = Phase::Readable;
        Ok(())
    }

    fn begin_page(
        &mut self,
        binding: ReadBinding,
        now: Instant,
    ) -> Result<PageOperation, ReadError> {
        let slot = self
            .slots
            .get_mut(&binding.client)
            .filter(|slot| slot.binding == binding)
            .ok_or(ReadError::SourceChanged)?;
        if slot.phase != Phase::Readable || slot.cancelled.load(Ordering::Acquire) {
            return Err(ReadError::Revoked);
        }
        if slot.operation.is_some() {
            return Err(ReadError::Busy);
        }
        let operation = Uuid::new_v4();
        slot.operation = Some(Operation {
            id: operation,
            deadline: now + PAGE_TIME,
        });
        Ok(PageOperation {
            binding,
            operation,
            cancelled: slot.cancelled.clone(),
            deadline: now + PAGE_TIME,
        })
    }

    fn finish_page(&mut self, operation: PageOperation) -> Result<(), ReadError> {
        let slot = self
            .slots
            .get_mut(&operation.binding.client)
            .filter(|slot| slot.binding == operation.binding)
            .ok_or(ReadError::SourceChanged)?;
        if !slot
            .operation
            .as_ref()
            .is_some_and(|active| active.id == operation.operation)
        {
            return Err(ReadError::Busy);
        }
        slot.operation = None;
        Ok(())
    }

    pub(in crate::host) fn revoke_client(&mut self, client: ClientId) {
        if let Some(slot) = self.slots.get_mut(&client) {
            slot.cancelled.store(true, Ordering::Release);
            if slot.phase != Phase::Settling {
                slot.phase = Phase::Revoked;
            }
        }
    }

    pub(in crate::host) fn revoke_all(&mut self) {
        self.closed = true;
        for slot in self.slots.values_mut() {
            slot.cancelled.store(true, Ordering::Release);
            if slot.phase != Phase::Settling {
                slot.phase = Phase::Revoked;
            }
        }
    }

    fn take_settlements(&mut self) -> Vec<Settlement> {
        let mut owners = std::mem::take(&mut self.unmatched_settlements);
        for slot in self.slots.values_mut() {
            if matches!(slot.phase, Phase::Revoked | Phase::Settling)
                && let Some(worker) = slot.worker.take()
            {
                slot.phase = Phase::Settling;
                owners.push(Settlement {
                    binding: slot.binding,
                    worker,
                });
            }
        }
        owners
    }

    fn return_unsettled(&mut self, owner: Settlement) {
        if let Some(slot) = self.slots.get_mut(&owner.binding.client)
            && slot.binding == owner.binding
            && slot.phase == Phase::Settling
            && slot.worker.is_none()
        {
            slot.worker = Some(owner.worker);
            return;
        }
        // A bookkeeping defect must retain the original join rather than
        // detach it or credit a different/newer reader as settled.
        self.custody_problem = true;
        self.unmatched_settlements.push(owner);
    }

    fn complete_join(&mut self, joined: JoinedReader) -> Result<(), ReadError> {
        let Some(slot) = self.slots.get(&joined.binding.client) else {
            self.custody_problem = true;
            return Err(ReadError::CustodyMismatch);
        };
        if slot.binding != joined.binding || slot.phase != Phase::Settling || slot.worker.is_some()
        {
            self.custody_problem = true;
            return Err(ReadError::CustodyMismatch);
        }
        // This slot contains metadata only. The original worker has actually
        // joined off-lock, including its resource destructors, before removal.
        self.slots.remove(&joined.binding.client);
        joined.result
    }

    fn settled(&self) -> bool {
        self.slots.is_empty() && self.unmatched_settlements.is_empty() && !self.custody_problem
    }
}

struct Settlement {
    binding: ReadBinding,
    worker: ReaderWorker,
}

struct JoinedReader {
    binding: ReadBinding,
    result: Result<(), ReadError>,
}

enum SettlementObservation {
    Joined(JoinedReader),
    StillSettling(Settlement),
}

impl Settlement {
    /// Off-lock only. The observation deadline never consumes an unfinished
    /// join, declares a timeout complete, or drops its original owner.
    fn observe(self, deadline: Instant) -> SettlementObservation {
        while !self.worker.thread.is_finished() {
            let now = Instant::now();
            if now >= deadline {
                return SettlementObservation::StillSettling(self);
            }
            std::thread::park_timeout((deadline - now).min(Duration::from_millis(10)));
        }
        let result = self
            .worker
            .thread
            .join()
            .unwrap_or(Err(ReadError::WorkerPanicked));
        SettlementObservation::Joined(JoinedReader {
            binding: self.binding,
            result,
        })
    }
}

/// Revoke under a short state lock, keep every original reader alive while
/// observing its worker off-lock, then record only actual join completion.
/// Returning false retains reader capacity and the ordinary shutdown barrier.
pub(in crate::host) fn finish_shutdown_readers(host: &SharedHost) -> bool {
    {
        let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.history_readers.revoke_all();
    }
    settle_revoked_readers(host)
}

/// Disconnect callers must invoke this after releasing both their control gate
/// and HostState locks. Unfinished owners go back to their counted slots.
pub(in crate::host) fn settle_revoked_readers(host: &SharedHost) -> bool {
    let owners = host
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .history_readers
        .take_settlements();
    let deadline = Instant::now() + SETTLEMENT_OBSERVATION;
    for owner in owners {
        let observed = owner.observe(deadline);
        let failed = {
            let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match observed {
                SettlementObservation::Joined(joined) => {
                    state.history_readers.complete_join(joined).is_err()
                }
                SettlementObservation::StillSettling(owner) => {
                    state.history_readers.return_unsettled(owner);
                    false
                }
            }
        };
        if failed {
            // Logging also stays outside HostState and exposes no raw source.
            tracing::warn!("original history reader settlement reported a failure");
        }
    }
    host.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .history_readers
        .settled()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        account::scope::LaunchAccountScope,
        agent::Provider,
        daemon::holder_registry::{Kind, Process, Record},
        host::model::Registry,
        model::{LaunchRecipe, TerminalId},
    };
    use std::{os::unix::fs::MetadataExt, sync::mpsc};

    /// Synthetic candidate/slot metadata, never a current read witness or a
    /// fabricated native/permission ACK. No History constructor runs here.
    fn fixture() -> (tempfile::TempDir, HostState, Arc<RetiredHistoryCandidate>) {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let proof = HostSessionProof {
            host_session_id: "history-slot-test".into(),
            boot_id: "history-old-boot".into(),
            host_incarnation: "original".into(),
        };
        let source = chat::Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: Provider::Claude,
                recipe: LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: root.path().into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some("11111111-1111-4111-8111-111111111111".into()),
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        };
        let owner = Record {
            version: 4,
            build: None,
            kind: Kind::Native,
            application: crate::instance::APPLICATION_ID.into(),
            boot: proof.boot_id.clone(),
            terminal: source.id,
            holder: Process { pid: 42, start: 1 },
            root: Process { pid: 43, start: 2 },
        };
        let metadata = std::fs::metadata(root.path()).unwrap();
        let scope = LaunchAccountScope {
            provider: Provider::Claude,
            canonical_home: root.path().canonicalize().unwrap(),
            device: metadata.dev(),
            inode: metadata.ino(),
            registered_id: None,
            purge_ids: Vec::new(),
        };
        let mut registry = Registry::at(&root.path().join("candidate-registry")).unwrap();
        let actor = Uuid::new_v4();
        registry
            .register_native_actor_ownership(actor, &owner, &source, Ok(&scope))
            .unwrap();
        registry
            .commit_native_actor_retirement(
                actor,
                &owner,
                &source,
                Ok(&scope),
                source.launch.session_id.as_deref(),
            )
            .unwrap();
        let candidate = Arc::new(
            registry
                .retired_history_candidate(&source)
                .unwrap()
                .unwrap(),
        );
        let mut state = HostState::new(proof);
        state.register_client(1);
        (root, state, candidate)
    }

    #[test]
    fn reader_connection_rejects_same_numeric_id_with_a_new_registration() {
        let (_root, mut state, _candidate) = fixture();
        let original = OriginalConnection::capture(&state, 1).unwrap();
        assert!(original.current(&state).is_ok());
        state.register_client(1);
        assert_eq!(original.current(&state), Err(ReadError::ConnectionChanged));
        let replacement = OriginalConnection::capture(&state, 1).unwrap();
        assert!(!original.same(&replacement));
        assert_eq!(format!("{original:?}"), "OriginalHistoryConnection");
    }

    #[test]
    fn revoked_or_expired_opening_keeps_global_and_connection_credit() {
        let (_root, mut state, candidate) = fixture();
        let mut slots = ReaderSlots::default();
        let now = Instant::now();
        let mut openings = Vec::new();
        for client in 1..=MAX_READERS as u64 {
            state.register_client(client);
            let connection = OriginalConnection::capture(&state, client).unwrap();
            let opening = slots
                .reserve(connection.clone(), candidate.clone(), now)
                .unwrap();
            assert!(!opening.cancelled_or_expired(now));
            assert!(opening.cancelled_or_expired(now + OPEN_TIME));
            assert_eq!(
                slots.check_operation(
                    &connection,
                    &candidate,
                    opening.binding,
                    opening.operation,
                    now + OPEN_TIME
                ),
                Err(ReadError::Deadline)
            );
            slots.revoke_client(client);
            assert!(opening.cancelled.load(Ordering::Acquire));
            assert!(matches!(
                slots.reserve(connection, candidate.clone(), now),
                Err(ReadError::ConnectionOccupied)
            ));
            openings.push(opening);
        }
        state.register_client(100);
        let connection = OriginalConnection::capture(&state, 100).unwrap();
        assert!(matches!(
            slots.reserve(connection.clone(), candidate.clone(), now),
            Err(ReadError::ReaderLimit)
        ));
        assert!(!slots.settled());
        // These reservations never launched anything. Only this exact branch
        // may release them without a worker join, not a timeout/detach branch.
        slots.failed_before_spawn(openings.remove(0)).unwrap();
        let next = slots.reserve(connection, candidate, now).unwrap();
        slots.failed_before_spawn(next).unwrap();
        for opening in openings {
            slots.failed_before_spawn(opening).unwrap();
        }
        assert!(slots.settled());
    }

    #[test]
    fn page_operations_are_single_and_stale_completion_cannot_clear_a_new_one() {
        let (_root, state, candidate) = fixture();
        let connection = OriginalConnection::capture(&state, 1).unwrap();
        let mut slots = ReaderSlots::default();
        let now = Instant::now();
        let opening = slots
            .reserve(connection.clone(), candidate.clone(), now)
            .unwrap();
        let binding = opening.binding;
        let opening_id = opening.operation;
        let (release, wait) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = wait.recv_timeout(Duration::from_secs(5));
            Ok(())
        });
        slots
            .install_worker(opening, ReaderWorker { thread: worker })
            .unwrap();
        slots.finish_open(binding, opening_id, now).unwrap();
        let first = slots.begin_page(binding, now).unwrap();
        let old = first.operation;
        assert!(matches!(
            slots.begin_page(binding, now),
            Err(ReadError::Busy)
        ));
        assert_eq!(
            slots.check_operation(&connection, &candidate, binding, old, now + PAGE_TIME),
            Err(ReadError::Deadline)
        );
        slots.finish_page(first).unwrap();
        let second = slots.begin_page(binding, now).unwrap();
        assert_ne!(old, second.operation);
        assert!(!second.cancelled_or_expired(now));
        assert!(second.cancelled_or_expired(now + PAGE_TIME));
        assert_eq!(
            slots.finish_page(PageOperation {
                binding,
                operation: old,
                cancelled: slots.slots[&1].cancelled.clone(),
                deadline: now + PAGE_TIME,
            }),
            Err(ReadError::Busy)
        );
        assert!(
            slots
                .check_operation(&connection, &candidate, binding, second.operation, now)
                .is_ok()
        );
        slots.finish_page(second).unwrap();
        slots.revoke_all();
        let _ = release.send(());
        for owner in slots.take_settlements() {
            let SettlementObservation::Joined(joined) =
                owner.observe(Instant::now() + Duration::from_secs(5))
            else {
                panic!("private worker did not join");
            };
            slots.complete_join(joined).unwrap();
        }
        assert!(slots.settled());
    }

    #[test]
    fn settlement_deadline_retains_original_worker_through_resource_drop_and_join() {
        struct ResourceDrop {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl Drop for ResourceDrop {
            fn drop(&mut self) {
                let _ = self.entered.send(());
                let _ = self.release.recv_timeout(Duration::from_secs(5));
            }
        }
        let (_root, state, candidate) = fixture();
        let connection = OriginalConnection::capture(&state, 1).unwrap();
        let mut slots = ReaderSlots::default();
        let opening = slots
            .reserve(connection.clone(), candidate.clone(), Instant::now())
            .unwrap();
        let (entered, body_done) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _resource = ResourceDrop {
                entered,
                release: wait,
            };
            Ok(())
        });
        slots
            .install_worker(opening, ReaderWorker { thread: worker })
            .unwrap();
        body_done.recv_timeout(Duration::from_secs(5)).unwrap();
        slots.revoke_client(1);
        let owner = slots.take_settlements().pop().unwrap();
        let SettlementObservation::StillSettling(owner) = owner.observe(Instant::now()) else {
            panic!("body return is not resource disposal or actual join");
        };
        slots.return_unsettled(owner);
        assert!(!slots.settled());
        assert!(matches!(
            slots.reserve(connection.clone(), candidate.clone(), Instant::now()),
            Err(ReadError::ConnectionOccupied)
        ));
        release.send(()).unwrap();
        let owner = slots.take_settlements().pop().unwrap();
        let SettlementObservation::Joined(joined) =
            owner.observe(Instant::now() + Duration::from_secs(5))
        else {
            panic!("released resource did not join");
        };
        slots.complete_join(joined).unwrap();
        assert!(slots.settled());
        let next = slots
            .reserve(connection, candidate, Instant::now())
            .unwrap();
        slots.failed_before_spawn(next).unwrap();
    }
}
