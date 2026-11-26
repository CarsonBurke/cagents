//! One daemon broker: provider callbacks only coalesce bounded semantic records.
//! The broker alone touches SQLite and the desktop's notification service,
//! outside HostState. Linux delivers through the freedesktop portal; macOS,
//! where the daemon has no app bundle to post from, through `osascript`.
mod batching;
mod journal;
pub(super) mod orphaned;
#[cfg(target_os = "macos")]
mod osascript;
#[cfg(target_os = "linux")]
mod portal;
mod review_monitor;
mod withdrawals;
use journal::Journal;
#[cfg(target_os = "macos")]
use osascript::{Activation, PortalOwner, connect, remove, send, send_test};
#[cfg(target_os = "linux")]
use portal::{Activation, PortalOwner, connect, remove, send, send_test};
use review_monitor::ReviewMonitor;
use withdrawals::Withdrawals;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, Weak},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use uuid::Uuid;

use super::state::{ClientId, HostLock, SharedHost};
use crate::{
    model::TerminalId,
    notifications::{Event, Kind, Notice, Preferences, Presence, Snapshot, Target, TestOutcome},
};

const MAX_PENDING: usize = 2048;
const MAX_REVIEW_UPDATES: usize = 16;
// One stable portal ID and one legacy journal ID per retained notice.
const MAX_WITHDRAWALS: usize = MAX_PENDING * 2;
const MAX_ACTIONS: usize = 32;
const MAX_ACTIVATION_TOKEN: usize = 4096;
const RETRY_DELAY: Duration = Duration::from_secs(30);
#[derive(Default)]
struct InboxAttention {
    pending: BTreeSet<crate::agent_execution::Lineage>,
    events: BTreeMap<Target, Event>,
}

pub(in crate::host) type CommitAcknowledgement = std::sync::mpsc::Receiver<Result<(), String>>;

/// An in-flight commit is unacknowledged until the FULL journal transaction
/// returns. Early shutdown or unwinding must explicitly fail the waiter.
struct CommitReceipt {
    reply: Option<std::sync::mpsc::Sender<Result<(), String>>>,
    resolved: bool,
    admission: journal::Admission,
}

struct RetainedTurn {
    session: TerminalId,
    actor: Uuid,
    generation: u64,
    through: u64,
    predecessors: BTreeSet<TerminalId>,
    before_retirement: bool,
    receipt: CommitReceipt,
    _slot: OwnedSemaphorePermit,
}

impl CommitReceipt {
    fn new(reply: std::sync::mpsc::Sender<Result<(), String>>) -> Self {
        Self::admitted(reply, journal::Admission::default())
    }

    fn admitted(
        reply: std::sync::mpsc::Sender<Result<(), String>>,
        admission: journal::Admission,
    ) -> Self {
        Self {
            reply: Some(reply),
            resolved: false,
            admission,
        }
    }

    fn finish(mut self, result: Result<(), String>) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(result);
        }
    }
}

impl Drop for CommitReceipt {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(Err(
                "Notification journal did not commit; outcome remains unacknowledged".into(),
            ));
        }
    }
}

enum ReviewUpdate {
    Monitor(crate::notifications::review::MonitorUpdate),
    Observed(VecDeque<(Event, Option<crate::notifications::review::ScanPoint>)>),
}

struct QueuedReviewUpdate {
    update: ReviewUpdate,
    received_at: i64,
    // Admission includes updates detached by the broker and deferred on a
    // journal failure. Moving the item between queues keeps its slot.
    _slot: OwnedSemaphorePermit,
}

#[derive(Default)]
struct Pending {
    events: BTreeMap<Target, Event>,
    committed_events: VecDeque<(Event, CommitReceipt)>,
    inboxes: Option<InboxAttention>,
    retired: BTreeSet<Target>,
    // Provider lifecycle settlements coalesce by exact target. Keep them
    // through a journal outage even after the process that sent one has ended.
    cleared_waiting: BTreeSet<Target>,
    acknowledged: BTreeSet<Uuid>,
    predecessor_reads: BTreeMap<Uuid, TerminalId>,
    review_updates: VecDeque<QueuedReviewUpdate>,
    started_turns: BTreeSet<TerminalId>,
    retained_turns: VecDeque<RetainedTurn>,
    presence_changed: bool,
    actions: VecDeque<(Uuid, Option<String>)>,
    /// The portal service's latest owner, when it changed; `Some(None)` means
    /// it exited. A different owner has forgotten this daemon's registration.
    portal_owner: Option<PortalOwner>,
    reload: bool,
    preferences: Option<Preferences>,
    /// Requesters waiting on a test notification, each answered once.
    tests: Vec<std::sync::mpsc::Sender<TestOutcome>>,
    stopped: bool,
}

impl Pending {
    fn supersede(&mut self, sessions: &BTreeSet<TerminalId>) {
        self.events
            .retain(|target, _| !matches!(target, Target::Session(id) if sessions.contains(id)));
        for (event, receipt) in &mut self.committed_events {
            if matches!(event.target, Target::Session(id) if sessions.contains(&id)) {
                receipt.admission.superseded = true;
            }
        }
    }

    fn start_turns(&mut self, sessions: BTreeSet<TerminalId>) {
        self.supersede(&sessions);
        // A confirmed provider boundary has no caller to retry it. Keep
        // one settlement per session through a journal outage.
        self.started_turns.extend(sessions);
    }

    fn flush_started_turns(
        &mut self,
        journal: &mut Journal,
    ) -> Result<Vec<(String, String)>, String> {
        if self.started_turns.is_empty() {
            return Ok(Vec::new());
        }
        let rows = journal.acknowledge_sessions(&self.started_turns)?;
        self.started_turns.clear();
        Ok(rows)
    }

    fn mark_retained_predecessors_of_retirement(&mut self, target: &Target) {
        if let Target::Session(session) = *target {
            for turn in self
                .retained_turns
                .iter_mut()
                .filter(|turn| turn.session == session)
            {
                turn.before_retirement = true;
            }
        }
    }

    fn flush_retained_turns(
        &mut self,
        journal: &mut Journal,
        before_retirement: bool,
        mut withdraw: impl FnMut(Vec<(String, String)>),
    ) -> Result<(), String> {
        while let Some(index) = self
            .retained_turns
            .iter()
            .position(|turn| turn.before_retirement == before_retirement)
        {
            let turn = &self.retained_turns[index];
            let rows = journal.acknowledge_retained_turn(
                turn.session,
                turn.actor,
                turn.generation,
                turn.through,
                &turn.predecessors,
            )?;
            let turn = self
                .retained_turns
                .remove(index)
                .expect("checked retained turn");
            if let Some(rows) = rows {
                self.supersede(&turn.predecessors);
                withdraw(rows);
            }
            turn.receipt.finish(Ok(()));
        }
        Ok(())
    }

    fn commit_next_retirement(&mut self, journal: &mut Journal) -> Result<Option<Target>, String> {
        let Some(target) = self.retired.first() else {
            return Ok(None);
        };
        journal.retire(target.clone())?;
        Ok(self.retired.pop_first())
    }

    /// This batch's work with `newer`, a later batch, applied after it, as if
    /// both had been queued as one: a later retirement withdraws an earlier
    /// event for its session, and a later event replaces an earlier one.
    fn followed_by(mut self, newer: Pending) -> Pending {
        for target in newer.retired {
            self.events.remove(&target);
            if matches!(target, Target::Session(_)) || self.retired.len() < MAX_PENDING {
                self.mark_retained_predecessors_of_retirement(&target);
                self.retired.insert(target);
            }
        }
        for target in newer.cleared_waiting {
            self.clear_waiting(target);
        }
        self.start_turns(newer.started_turns);
        for event in newer.events.into_values() {
            self.queue_event(event);
        }
        for id in newer.acknowledged {
            if self.acknowledged.len() < MAX_PENDING {
                self.acknowledged.insert(id);
            }
        }
        self.read_predecessors(newer.predecessor_reads);
        self.review_updates.extend(newer.review_updates);
        self.committed_events.extend(newer.committed_events);
        self.retained_turns.extend(newer.retained_turns);
        if newer.inboxes.is_some() {
            self.inboxes = newer.inboxes;
        }
        self.presence_changed |= newer.presence_changed;
        self.actions.extend(newer.actions);
        if newer.portal_owner.is_some() {
            self.portal_owner = newer.portal_owner;
        }
        if newer.reload {
            self.reload = true;
            self.preferences = newer.preferences;
        }
        self.tests.extend(newer.tests);
        self.stopped |= newer.stopped;
        self
    }

    fn queue_event(&mut self, event: Event) {
        let target = event.target();
        // Native callbacks have already fenced the exact session owner but
        // cannot replay a dropped outcome. Coalesce each session across a
        // journal outage rather than truncating it at the generic queue limit.
        if matches!(target, Target::Session(_))
            || self.events.len() < MAX_PENDING
            || self.events.contains_key(&target)
        {
            self.events.insert(target, event);
        }
    }

    fn clear_waiting(&mut self, target: Target) {
        if self
            .events
            .get(&target)
            .is_some_and(|event| event.evidence.kind == Kind::Waiting)
        {
            self.events.remove(&target);
        }
        for (event, receipt) in &mut self.committed_events {
            if event.target == target && event.evidence.kind == Kind::Waiting {
                receipt.resolved = true;
            }
        }
        self.cleared_waiting.insert(target);
    }

    fn read_predecessors(&mut self, reads: BTreeMap<Uuid, TerminalId>) {
        for (id, session) in reads {
            if self.predecessor_reads.len() >= MAX_PENDING {
                break;
            }
            self.predecessor_reads.insert(id, session);
        }
    }

    /// The work that waits for an unavailable journal rather than being lost:
    /// attention, retirements, acknowledgements and the settings and windows
    /// they are judged by. Retained turn receipts stay with their fence.
    /// Other receipts and tests are answered at once, and a
    /// click on a notice is not replayed long after the person made it.
    fn deferred(self) -> Pending {
        Pending {
            events: self.events,
            inboxes: self.inboxes,
            retired: self.retired,
            cleared_waiting: self.cleared_waiting,
            acknowledged: self.acknowledged,
            predecessor_reads: self.predecessor_reads,
            review_updates: self.review_updates,
            started_turns: self.started_turns,
            retained_turns: self.retained_turns,
            presence_changed: self.presence_changed,
            reload: self.reload,
            preferences: self.preferences,
            ..Pending::default()
        }
    }
}

fn take_pending(core: &Shared) -> Pending {
    let mut queue = core.pending.lock().unwrap_or_else(|p| p.into_inner());
    let pending = std::mem::take(&mut *queue);
    // Stop admission remains closed while the broker drains this final batch.
    queue.stopped = pending.stopped;
    pending
}

struct Shared {
    pending: Mutex<Pending>,
    review_slots: Arc<Semaphore>,
    retained_turn_slots: Arc<Semaphore>,
    presence: Mutex<BTreeMap<ClientId, Presence>>,
    snapshot: Mutex<Snapshot>,
    wake: mpsc::Sender<()>,
}

#[derive(Clone)]
pub(super) struct Handle(Arc<Shared>);

impl Handle {
    pub(super) fn observe_review_monitor(
        &self,
        update: crate::notifications::review::MonitorUpdate,
    ) -> Result<(), String> {
        use crate::notifications::review::MonitorUpdate;
        match update {
            MonitorUpdate::Observations(observed) => {
                self.enqueue_review_update(ReviewUpdate::Observed(
                    observed
                        .events()
                        .map(|(event, point)| (event, Some(point)))
                        .collect(),
                ))
            }
            update => self.enqueue_review_update(ReviewUpdate::Monitor(update)),
        }
    }

    pub(super) fn observe_review_requests(
        &self,
        observed: crate::notifications::review::ObservedRequests,
    ) -> Result<(), String> {
        self.enqueue_review_update(ReviewUpdate::Observed(
            observed.events().map(|event| (event, None)).collect(),
        ))
    }

    fn enqueue_review_update(&self, update: ReviewUpdate) -> Result<(), String> {
        let mut pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.stopped || self.0.wake.is_closed() {
            return Err("Review reconciliation is unavailable".into());
        }
        let slot = self
            .0
            .review_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "Review reconciliation is busy".to_owned())?;
        pending.review_updates.push_back(QueuedReviewUpdate {
            update,
            received_at: review_clock(),
            _slot: slot,
        });
        drop(pending);
        let _ = self.0.wake.try_send(());
        Ok(())
    }

    pub(super) fn session_started_turn(
        &self,
        session: TerminalId,
        mut predecessors: BTreeSet<TerminalId>,
    ) {
        predecessors.insert(session);
        self.change(|pending| pending.start_turns(predecessors));
    }

    pub(super) fn commit_retained_turn(
        &self,
        session: TerminalId,
        actor: Uuid,
        generation: u64,
        through: u64,
        predecessors: BTreeSet<TerminalId>,
    ) -> Result<CommitAcknowledgement, String> {
        let (reply, result) = std::sync::mpsc::channel();
        let mut pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.stopped {
            return Err("Notification journal cannot admit a retained turn".into());
        }
        let slot = self
            .0
            .retained_turn_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "Notification journal cannot admit a retained turn")?;
        pending.retained_turns.push_back(RetainedTurn {
            session,
            actor,
            generation,
            through,
            predecessors,
            before_retirement: false,
            receipt: CommitReceipt::new(reply),
            _slot: slot,
        });
        drop(pending);
        let _ = self.0.wake.try_send(());
        Ok(result)
    }
    /// Real bounded enqueue state without a broker, portal, or worker thread.
    #[cfg(test)]
    pub(in crate::host) fn inert_for_test() -> (Self, impl Fn() -> Vec<Event>) {
        let (wake, _receiver) = mpsc::channel(1);
        let shared = Arc::new(Shared {
            pending: Mutex::default(),
            review_slots: Arc::new(Semaphore::new(MAX_REVIEW_UPDATES)),
            retained_turn_slots: Arc::new(Semaphore::new(MAX_PENDING)),
            presence: Mutex::default(),
            snapshot: Mutex::default(),
            wake,
        });
        let observed = shared.clone();
        (Self(shared), move || {
            observed
                .pending
                .lock()
                .unwrap()
                .events
                .values()
                .cloned()
                .collect()
        })
    }

    fn change(&self, change: impl FnOnce(&mut Pending)) {
        let mut pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.stopped {
            return;
        }
        change(&mut pending);
        drop(pending);
        let _ = self.0.wake.try_send(());
    }

    pub(super) fn reconcile_automation_attention(&self) {
        self.change(|_| {});
    }

    pub fn record(&self, event: Event) {
        if event.validate().is_err() {
            return;
        }
        self.change(|pending| {
            // Producers fence worker identity before enqueueing. A newer
            // worker may reuse this session while retirement is still queued;
            // apply that retirement first, then its new event in the broker.
            pending.queue_event(event);
        });
    }

    /// Receipt confirms the FULL journal transaction, never merely enqueueing.
    /// Callers wait outside HostState and keep helper outbox entries on error.
    pub(in crate::host) fn record_committed(
        &self,
        event: Event,
    ) -> Result<std::sync::mpsc::Receiver<Result<(), String>>, String> {
        self.record_committed_admitted(event, journal::Admission::default(), false)
    }

    pub(in crate::host) fn record_retained(
        &self,
        event: Event,
        actor: Uuid,
        sequence: u64,
        resolved: bool,
    ) -> Result<CommitAcknowledgement, String> {
        self.record_committed_admitted(
            event,
            journal::Admission {
                retained: Some((actor, sequence)),
                ..journal::Admission::default()
            },
            resolved,
        )
    }

    fn record_committed_admitted(
        &self,
        event: Event,
        admission: journal::Admission,
        resolved: bool,
    ) -> Result<CommitAcknowledgement, String> {
        event.validate()?;
        let (receipt, result) = std::sync::mpsc::channel();
        let mut pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.stopped || self.0.wake.is_closed() {
            return Err("Notification journal is stopping; outcome remains unacknowledged".into());
        }
        if pending.committed_events.len() >= MAX_PENDING {
            return Err("Notification commit queue is full; outcome remains unacknowledged".into());
        }
        if resolved {
            pending.clear_waiting(event.target());
        }
        let mut receipt = CommitReceipt::admitted(receipt, admission);
        receipt.resolved = resolved;
        pending.committed_events.push_back((event, receipt));
        drop(pending);
        let _ = self.0.wake.try_send(());
        Ok(result)
    }

    pub(in crate::host) fn reconcile_inboxes(
        &self,
        pending_lineages: BTreeSet<crate::agent_execution::Lineage>,
        events: Vec<Event>,
    ) {
        let events = events
            .into_iter()
            .filter(|event| {
                matches!(event.target, Target::SessionInbox(_)) && event.validate().is_ok()
            })
            .take(MAX_PENDING)
            .map(|event| (event.target(), event))
            .collect();
        self.change(|pending| {
            pending.inboxes = Some(InboxAttention {
                pending: pending_lineages,
                events,
            })
        });
    }

    pub fn clear_session_waiting(&self, session: TerminalId) {
        self.change(|pending| pending.clear_waiting(Target::Session(session)));
    }

    pub fn retire(&self, session: TerminalId) {
        let session = Target::Session(session);
        self.change(|pending| {
            pending.events.remove(&session);
            pending
                .committed_events
                .retain(|(event, _)| event.target() != session);
            pending.mark_retained_predecessors_of_retirement(&session);
            pending.retired.insert(session);
        });
    }

    pub fn presence(&self, client: ClientId, presence: Option<Presence>) {
        let mut current = self.0.presence.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(presence) = presence {
            current.insert(client, presence);
        } else {
            current.remove(&client);
        }
        drop(current);
        self.change(|pending| {
            pending.presence_changed = true;
        });
    }

    pub fn acknowledge(&self, id: Uuid) {
        self.change(|pending| {
            if pending.acknowledged.len() < MAX_PENDING {
                pending.acknowledged.insert(id);
            }
        });
    }

    pub fn reload(&self, preferences: Option<Preferences>) {
        self.change(|pending| {
            pending.reload = true;
            pending.preferences = preferences;
        });
    }

    /// Queues a test notification. The receiver yields its outcome once the
    /// broker has tried it, and disconnects if the broker stops first.
    pub fn test(&self) -> std::sync::mpsc::Receiver<TestOutcome> {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.change(|pending| {
            if pending.tests.len() < MAX_PENDING {
                pending.tests.push(sender);
            }
        });
        receiver
    }

    pub(in crate::host) fn unread_automation_notices(&self) -> Vec<(Uuid, Uuid, u64)> {
        self.0
            .snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .notices
            .iter()
            .filter_map(|notice| match notice.event.target {
                Target::Automation { id, sequence } if !notice.acknowledged => {
                    Some((notice.id, id, sequence))
                }
                _ => None,
            })
            .collect()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.0
            .snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// Capture the exact committed predecessor notices at authenticated read
/// acceptance. The broker remains the only writer, and a replacement committed
/// before it handles this batch must remain unread.
pub(super) fn acknowledge_predecessors(host: &SharedHost, session: TerminalId) {
    let (notifications, registry) = {
        let state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (
            state.notifications.clone(),
            state.execution_registry.clone(),
        )
    };
    let (Some(notifications), Some(registry), Some(path)) =
        (notifications, registry, journal_path())
    else {
        return;
    };
    let predecessors = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .notice_predecessors(session);
    let reads = predecessors.and_then(|predecessors| {
        if predecessors.is_empty() {
            return Ok(BTreeMap::new());
        }
        Journal::open_reader(&path)?.unread_session_ids(&predecessors)
    });
    match reads {
        Ok(reads) if !reads.is_empty() => notifications.change(|pending| {
            pending.read_predecessors(reads);
        }),
        Ok(_) => {}
        Err(error) => {
            tracing::debug!(%session, %error, "could not capture committed predecessor attention")
        }
    }
}

fn apply_predecessor_reads(
    journal: &mut Journal,
    reads: &mut BTreeMap<Uuid, TerminalId>,
    delivery: &mut BTreeMap<Target, Notice>,
    mut cleared: impl FnMut(Uuid, TerminalId),
) -> Result<(), String> {
    // The person already read these exact notices. A failed disk write
    // must not allow a waiting desktop delivery to announce them again.
    delivery.retain(|_, notice| !reads.contains_key(&notice.id));
    let mut failure = None;
    reads.retain(
        |&id, &mut session| match journal.acknowledge_with_withdrawal(id) {
            Ok(acknowledged) => {
                if acknowledged.is_some() {
                    cleared(id, session);
                }
                false
            }
            Err(error) => {
                failure = Some(error);
                true
            }
        },
    );
    failure.map_or(Ok(()), Err)
}

pub(super) fn predecessor_sessions(
    host: &SharedHost,
    session: TerminalId,
) -> Result<BTreeSet<TerminalId>, String> {
    let registry = super::agent_execution::registry(host)?;
    let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    registry.notice_predecessors(session)
}

pub(super) struct Broker {
    handle: Handle,
    thread: Option<JoinHandle<()>>,
}

impl Broker {
    pub fn start(
        host: &SharedHost,
        application: crate::notifications::Application,
    ) -> std::io::Result<Self> {
        let (wake, receiver) = mpsc::channel(1);
        let handle = Handle(Arc::new(Shared {
            pending: Mutex::default(),
            review_slots: Arc::new(Semaphore::new(MAX_REVIEW_UPDATES)),
            retained_turn_slots: Arc::new(Semaphore::new(MAX_PENDING)),
            presence: Mutex::default(),
            snapshot: Mutex::default(),
            wake,
        }));
        let shared = Arc::downgrade(&handle.0);
        let host = Arc::downgrade(host);
        let thread = thread::Builder::new()
            .name("cagentsd-notifications".into())
            .spawn(move || {
                supervise(&shared, &host, || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| error.to_string())?;
                    runtime.block_on(run(shared.clone(), host.clone(), receiver, application));
                    Ok(())
                });
            })?;
        Ok(Self {
            handle,
            thread: Some(thread),
        })
    }

    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.handle.change(|pending| pending.stopped = true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn supervise(
    shared: &Weak<Shared>,
    host: &Weak<HostLock>,
    run: impl FnOnce() -> Result<(), String>,
) {
    if let Err(error) = super::service::attempt("notification service", run) {
        tracing::warn!(%error, "desktop notifications are unavailable");
        let Some(core) = shared.upgrade() else {
            return;
        };
        let pending = std::mem::replace(
            &mut *core.pending.lock().unwrap_or_else(|p| p.into_inner()),
            Pending {
                stopped: true,
                ..Default::default()
            },
        );
        answer_tests(&pending.tests, &TestOutcome::Unavailable(error.clone()));
        // Attention already collected by the broker remains readable even if
        // its portal/runtime fails. Only the delivery capability is lost.
        let mut snapshot = core
            .snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        snapshot.diagnostic = Some(format!("Notifications unavailable: {error}"));
        publish(shared, host, snapshot);
    }
}

/// Explicit notification reads see the current journal, even while the
/// broker awaits desktop delivery. Text remains subject to the broker's
/// published privacy projection.
pub(super) fn committed_snapshot(mut published: Snapshot) -> Snapshot {
    let Some(dirs) = crate::instance::project_dirs() else {
        return published;
    };
    match Journal::committed_notices(&dirs.data_dir().join("agent-attention.sqlite3")) {
        Ok(notices) => combine_committed_notices(&mut published, notices),
        Err(error) => {
            tracing::debug!(%error, "could not read the committed attention journal");
        }
    }
    published
}

fn combine_committed_notices(published: &mut Snapshot, mut notices: Vec<Notice>) {
    let previews = published
        .notices
        .drain(..)
        .filter_map(|notice| notice.event.content.map(|preview| (notice.id, preview)))
        .collect::<std::collections::HashMap<_, _>>();
    for notice in &mut notices {
        if notice.event.content.is_some() {
            notice.event.content = previews.get(&notice.id).cloned();
        }
    }
    published.notices = notices;
}

fn publish(shared: &Weak<Shared>, host: &Weak<HostLock>, snapshot: Snapshot) {
    if let Some(shared) = shared.upgrade() {
        let mut current = shared.snapshot.lock().unwrap_or_else(|p| p.into_inner());
        if *current == snapshot {
            return;
        }
        let automation_waiting = snapshot.notices.iter().any(|notice| {
            !notice.acknowledged && matches!(notice.event.target, Target::Automation { .. })
        });
        *current = snapshot;
        drop(current);
        if let Some(host) = host.upgrade() {
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .notifications_changed();
            if automation_waiting {
                super::automations::wake(&host);
            }
        }
    }
}

fn preferences() -> Result<Preferences, String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let Some(path) = crate::instance::settings_path() else {
        return Ok(Preferences::default());
    };
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Preferences::default());
        }
        Err(error) => return Err(format!("Cannot read notification settings: {error}")),
    };
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("Notification settings are not a regular file".into());
    }
    let mut text = String::new();
    file.take(1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|error| error.to_string())?;
    if text.len() > 1024 * 1024 {
        return Err("Notification settings exceed one MiB".into());
    }
    let value: toml::Value =
        toml::from_str(&text).map_err(|error| format!("Invalid notification settings: {error}"))?;
    value.get("notifications").cloned().map_or_else(
        || Ok(Preferences::default()),
        |value| {
            value
                .try_into()
                .map_err(|error| format!("Invalid notification settings: {error}"))
        },
    )
}

impl Shared {
    /// Queue one desktop activation of a notice for the broker loop.
    ///
    /// A click can arrive both as an application action and as a portal signal
    /// on backends that emit both. Deliveries that meet in the queue coalesce
    /// into one activation that keeps whichever carried an activation token;
    /// activation is idempotent, so a delivery after the drain is harmless.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fn activate(&self, id: Uuid, token: Option<String>) {
        let token = token.filter(|token| token.len() <= MAX_ACTIVATION_TOKEN);
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((_, queued)) = pending.actions.iter_mut().find(|(queued, _)| *queued == id) {
            if queued.is_none() {
                *queued = token;
            }
        } else if pending.actions.len() < MAX_ACTIONS {
            pending.actions.push_back((id, token));
        }
        drop(pending);
        let _ = self.wake.try_send(());
    }
}

fn queue_withdrawal(queue: &mut Withdrawals, id: Uuid) {
    queue.legacy(id);
}

fn withdraw_sessions(
    withdrawals: &mut Withdrawals,
    delivery: &mut BTreeMap<Target, Notice>,
    rows: Vec<(String, String)>,
) {
    for (session, id) in rows {
        if let (Ok(session), Ok(id)) = (session.parse(), id.parse()) {
            let target = Target::Session(session);
            delivery.remove(&target);
            withdraw_target(withdrawals, id, &target);
        }
    }
}

fn withdraw_notice(queue: &mut Withdrawals, notice: &Notice) {
    withdraw_target(queue, notice.id, &notice.event.target);
}

fn withdraw_target(queue: &mut Withdrawals, id: Uuid, target: &Target) {
    queue.group(batching::desktop_id(target));
    // A predecessor used the journal UUID as its portal ID. Retire that toast
    // too; removing an absent ID is harmless and preserves upgrade privacy.
    queue_withdrawal(queue, id);
}

/// Reads a journal that just opened into `snapshot`, without the content of
/// earlier notices when previews are off.
fn restore(
    journal: &mut Journal,
    policy: &Preferences,
    snapshot: &mut Snapshot,
    withdrawals: &mut Withdrawals,
) -> Result<(), String> {
    if !policy.message_previews {
        match journal.clear_content() {
            Ok(notices) => {
                for (id, target) in notices {
                    withdraw_target(withdrawals, id, &target);
                }
            }
            Err(error) => snapshot.diagnostic = Some(error),
        }
    }
    match journal.list() {
        Ok(notices) => snapshot.notices = notices,
        Err(error) => snapshot.diagnostic = Some(error),
    }
    for notice in &snapshot.notices {
        if notice.acknowledged {
            withdraw_notice(withdrawals, notice);
        }
    }
    if !policy.message_previews {
        revoke_previews(snapshot, withdrawals);
    }
    reserve_withdrawals(journal, snapshot, withdrawals).map(|_| ())
}

fn review_clock() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or_default()
}

fn expire_review_notices(
    monitor: &mut ReviewMonitor,
    journal: &mut Journal,
    snapshot: &mut Snapshot,
    withdrawals: &mut Withdrawals,
) -> BTreeSet<Uuid> {
    match monitor.expire(journal, &snapshot.notices, Instant::now(), review_clock()) {
        Ok(ids) => acknowledge_review_notices(snapshot, withdrawals, ids, |id| {
            journal.acknowledge(id).map(|_| ())
        }),
        Err(error) => {
            snapshot.diagnostic = Some(error);
            BTreeSet::new()
        }
    }
}

fn acknowledge_review_notices(
    snapshot: &mut Snapshot,
    withdrawals: &mut Withdrawals,
    candidates: Vec<Uuid>,
    mut persist: impl FnMut(Uuid) -> Result<(), String>,
) -> BTreeSet<Uuid> {
    let candidates: BTreeSet<_> = candidates.into_iter().collect();
    let mut acknowledged = BTreeSet::new();
    for notice in &mut snapshot.notices {
        if candidates.contains(&notice.id) && !notice.acknowledged {
            match persist(notice.id) {
                Ok(()) => {
                    notice.acknowledged = true;
                    acknowledged.insert(notice.id);
                    withdraw_notice(withdrawals, notice);
                }
                Err(error) => snapshot.diagnostic = Some(error),
            }
        }
    }
    acknowledged
}

/// Desktop revocation must not depend on a successful SQLite rewrite. Preserve
/// exact attention IDs while removing rich context from the published snapshot.
fn revoke_previews(snapshot: &mut Snapshot, withdrawals: &mut Withdrawals) {
    withdrawals.protect(
        snapshot
            .notices
            .iter()
            .map(|notice| batching::desktop_id(&notice.event.target)),
    );
    for notice in &mut snapshot.notices {
        if notice.event.content.take().is_some() {
            withdraw_notice(withdrawals, notice);
        }
    }
}

fn reserve_withdrawals(
    journal: &mut Journal,
    snapshot: &Snapshot,
    withdrawals: &mut Withdrawals,
) -> Result<BTreeSet<Uuid>, String> {
    let current: Vec<_> = snapshot
        .notices
        .iter()
        .map(|notice| batching::desktop_id(&notice.event.target))
        .collect();
    withdrawals.protect(current.iter().copied());
    let durable = journal.group_withdrawals_for(&current)?;
    for id in &durable {
        withdrawals.group(*id);
    }
    Ok(durable)
}

fn action_notice(snapshot: &Snapshot, id: Uuid) -> Option<&Notice> {
    snapshot.notices.iter().find(|notice| {
        if matches!(notice.event.target, Target::Automation { .. }) {
            !notice.acknowledged && notice.id == id
        } else {
            notice.id == id || batching::desktop_id(&notice.event.target) == id
        }
    })
}

fn action_if_current<T>(
    notice: &Notice,
    resolved: impl FnOnce(Uuid, u64) -> Result<bool, String>,
    route: impl FnOnce() -> T,
) -> Result<Option<T>, String> {
    if let Target::Automation { id, sequence } = notice.event.target
        && resolved(id, sequence)?
    {
        return Ok(None);
    }
    Ok(Some(route()))
}

fn route_notification_action(
    host: &SharedHost,
    notice: &Notice,
) -> Result<Option<crate::notifications::Destination>, String> {
    let route = || {
        host.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .activate_notification_destination(
                notice.event.target(),
                notice.event.source_session(),
                notice.window.as_deref(),
                notice.event.destination.as_ref(),
            )
    };
    if !matches!(notice.event.target, Target::Automation { .. }) {
        return Ok(Some(route()));
    }
    let registry = super::agent_execution::registry(host)?;
    // Serialize exact action authorization and its nonblocking routing with
    // resume, edit, and removal. Launching or raising a window comes later.
    let registry = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    action_if_current(
        notice,
        |id, seq| registry.automation_attention_resolved(id, seq),
        route,
    )
}

fn discard_resolved_delivery(
    journal: &mut Journal,
    snapshot: &mut Snapshot,
    withdrawals: &mut Withdrawals,
    delivery: &mut BTreeMap<Target, Notice>,
    notice: &Notice,
) -> Result<(), String> {
    // The registry already proved this toast obsolete. Leaving it ready in
    // the delivery queue makes the broker retry a failed SQLite write without
    // waiting for the reconciliation deadline.
    delivery.remove(&notice.event.target());
    journal.acknowledge(notice.id)?;
    if let Some(current) = snapshot
        .notices
        .iter_mut()
        .find(|current| current.id == notice.id)
    {
        current.acknowledged = true;
    }
    withdraw_notice(withdrawals, notice);
    Ok(())
}

struct ReviewDelivery<'a> {
    pending: &'a mut BTreeMap<Target, Notice>,
    schedule: &'a mut batching::Schedule,
    presence: &'a BTreeMap<ClientId, Presence>,
    policy: &'a Preferences,
}

fn apply_review_updates(
    monitor: &mut ReviewMonitor,
    journal: &mut Journal,
    snapshot: &mut Snapshot,
    withdrawals: &mut Withdrawals,
    updates: &mut VecDeque<QueuedReviewUpdate>,
    delivery: ReviewDelivery<'_>,
) -> Result<(), String> {
    while let Some(queued) = updates.front_mut() {
        match &mut queued.update {
            ReviewUpdate::Observed(observed) => {
                while let Some((event, point)) = observed.front() {
                    let focused = delivery
                        .presence
                        .values()
                        .any(|window| window.focused_target.as_ref() == Some(&event.target));
                    let suppressed = !delivery.policy.allows(event.evidence.kind, focused);
                    if let Some(notice) = ReviewMonitor::record(
                        journal,
                        event.clone(),
                        point.clone(),
                        suppressed,
                        queued.received_at,
                    )? {
                        if !suppressed {
                            delivery.schedule.update(
                                notice.event.target(),
                                notice.event.evidence.kind,
                                Instant::now(),
                            );
                            delivery
                                .pending
                                .insert(notice.event.target(), notice.clone());
                        }
                        // Later censuses in this FIFO see earlier committed observations.
                        snapshot.notices.push(notice);
                    }
                    // A partial page retries only its unfinished suffix.
                    observed.pop_front();
                }
            }
            ReviewUpdate::Monitor(update) => {
                let ids = monitor.apply(
                    update.clone(),
                    journal,
                    &snapshot.notices,
                    queued.received_at,
                )?;
                let cleared = acknowledge_review_notices(snapshot, withdrawals, ids, |_| Ok(()));
                if !cleared.is_empty() {
                    delivery
                        .pending
                        .retain(|_, notice| !cleared.contains(&notice.id));
                }
            }
        }
        updates.pop_front();
    }
    Ok(())
}

/// Defer a toast if an accepted census can retire that exact review request.
fn review_census_pending(updates: &VecDeque<QueuedReviewUpdate>, target: &Target) -> bool {
    use crate::notifications::review::MonitorUpdate;

    let Target::PullRequest(request) = target else {
        return false;
    };
    if updates.is_empty() {
        return false;
    }
    let repository = request.repository_key();
    updates.iter().any(|queued| match &queued.update {
        ReviewUpdate::Monitor(MonitorUpdate::Completed(census)) => {
            census.viewer == request.viewer
                && census.repository.same_repository(&repository)
                && census.current.get(&request.number) != Some(&request.request)
        }
        _ => false,
    })
}

fn shared_review_census_pending(shared: &Weak<Shared>, target: &Target) -> bool {
    matches!(target, Target::PullRequest(_))
        && shared.upgrade().is_some_and(|shared| {
            review_census_pending(
                &shared
                    .pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .review_updates,
                target,
            )
        })
}

fn delivery_deadline(
    delivery: &BTreeMap<Target, Notice>,
    schedule: &batching::Schedule,
    review_updates: &VecDeque<QueuedReviewUpdate>,
) -> Option<Instant> {
    delivery
        .keys()
        .filter(|target| !review_census_pending(review_updates, target))
        .filter_map(|target| schedule.due(target))
        .min()
}

fn delivery_batch(
    delivery: &BTreeMap<Target, Notice>,
    schedule: &batching::Schedule,
    withdrawals: &Withdrawals,
    review_updates: &VecDeque<QueuedReviewUpdate>,
    now: Instant,
) -> Vec<Notice> {
    delivery
        .values()
        .filter(|notice| {
            schedule.ready(&notice.event.target(), now)
                && !withdrawals.contains(&batching::desktop_id(&notice.event.target))
                && !review_census_pending(review_updates, &notice.event.target)
        })
        .take(8)
        .cloned()
        .collect()
}

fn answer_tests(requesters: &[std::sync::mpsc::Sender<TestOutcome>], outcome: &TestOutcome) {
    for requester in requesters {
        // A requester that gave up waiting has nothing left to tell.
        let _ = requester.send(outcome.clone());
    }
}

fn commit_waiting_clears(
    journal: &mut Journal,
    cleared: &BTreeSet<Target>,
    mut committed: impl FnMut(&Target, Uuid),
) -> Result<(), String> {
    for target in cleared {
        if let Some(id) = journal.acknowledge_waiting(target)? {
            committed(target, id);
        }
    }
    Ok(())
}

/// Flush accepted attention settlement and ensuing outcomes on ordinary exit.
fn settle_attention_at_shutdown(
    shared: &Weak<Shared>,
    journal: &mut Journal,
    pending: &mut Pending,
) -> bool {
    if let Some(shared) = shared.upgrade() {
        let mut queued = shared
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !queued.stopped {
            return false;
        }
        let last = std::mem::take(&mut *queued);
        queued.stopped = true;
        drop(queued);
        *pending = std::mem::take(pending).followed_by(last);
    }
    if !pending.review_updates.is_empty()
        && let Err(error) = settle_review_updates_at_shutdown(journal, &mut pending.review_updates)
    {
        tracing::warn!(%error, "accepted review updates could not settle during shutdown");
    }
    if let Err(error) = commit_waiting_clears(journal, &pending.cleared_waiting, |_, _| {}) {
        tracing::warn!(%error, "could not record waiting attention settlement at shutdown");
        return true;
    }
    if !pending.predecessor_reads.is_empty()
        && let Err(error) = apply_predecessor_reads(
            journal,
            &mut pending.predecessor_reads,
            &mut BTreeMap::new(),
            |_, _| {},
        )
    {
        tracing::warn!(%error, "accepted conversation reads could not settle during shutdown");
    }
    if let Err(error) = pending.flush_started_turns(journal) {
        tracing::warn!(%error, "confirmed turn attention could not settle during shutdown");
        return true;
    }
    if let Err(error) = pending.flush_retained_turns(journal, true, |_| {}) {
        tracing::warn!(%error, "retiring retained turn attention could not settle during shutdown");
        return true;
    }
    while !pending.retired.is_empty() {
        if let Err(error) = pending.commit_next_retirement(journal) {
            tracing::warn!(%error, "accepted notice retirement could not settle during shutdown");
            return true;
        }
    }
    if let Err(error) = pending.flush_retained_turns(journal, false, |_| {}) {
        tracing::warn!(%error, "retained turn attention could not settle during shutdown");
        return true;
    }
    // These accepted outcomes already follow the merged lifecycle boundaries.
    // Leave new unread entries in the journal without attempting a toast.
    for mut event in std::mem::take(&mut pending.events).into_values() {
        if matches!(event.target, Target::Session(_)) {
            event.content = None;
            if let Err(error) = journal.record_admitted(
                event,
                true,
                journal::Admission {
                    withdraw_previous: true,
                    ..journal::Admission::default()
                },
            ) {
                tracing::warn!(%error, "accepted session attention could not settle during shutdown");
            }
        }
    }
    true
}

fn settle_review_updates_at_shutdown(
    journal: &mut Journal,
    updates: &mut VecDeque<QueuedReviewUpdate>,
) -> Result<(), String> {
    // The journal can have a committed prefix from a previous retry or from
    // another batch. Reconcile pending censuses against its current contents.
    let mut snapshot = Snapshot {
        notices: journal.list()?,
        ..Default::default()
    };
    apply_review_updates(
        &mut ReviewMonitor::default(),
        journal,
        &mut snapshot,
        &mut Withdrawals::default(),
        updates,
        ReviewDelivery {
            pending: &mut BTreeMap::new(),
            schedule: &mut batching::Schedule::default(),
            presence: &BTreeMap::new(),
            policy: &Preferences {
                enabled: false,
                ..Default::default()
            },
        },
    )
}

fn session_settlement_admitted(shared: &Weak<Shared>, event: &Event) -> bool {
    let Target::Session(session) = event.target else {
        return false;
    };
    shared.upgrade().is_some_and(|shared| {
        let pending = shared.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending.started_turns.contains(&session)
            || (event.evidence.kind == Kind::Waiting
                && pending.cleared_waiting.contains(&event.target))
    })
}

fn retained_turn_pending(shared: &Weak<Shared>, event: &Event) -> bool {
    let Target::Session(session) = event.target else {
        return false;
    };
    shared.upgrade().is_some_and(|shared| {
        let pending = shared.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending
            .retained_turns
            .iter()
            .any(|turn| turn.session == session || turn.predecessors.contains(&session))
    })
}

async fn wait_for_work(receiver: &mut mpsc::Receiver<()>, retry: Option<Instant>) -> bool {
    if let Some(retry) = retry {
        tokio::select! {
            value = receiver.recv() => value.is_some(),
            _ = tokio::time::sleep_until(retry.into()) => true,
        }
    } else {
        receiver.recv().await.is_some()
    }
}

fn record_attention_committed_policy(
    journal: &mut Journal,
    mut event: Event,
    suppressed: bool,
    receipt: Option<CommitReceipt>,
    policy: &Preferences,
    resolved: bool,
) -> Result<Option<Notice>, String> {
    if !policy.message_previews {
        event.content = None;
    }
    if !resolved {
        return record_attention_committed(journal, event, suppressed, receipt);
    }
    // The retained actor may discard its outbox entry as soon as it receives
    // this receipt. Persist replay evidence and the acknowledgement together.
    let mut admission = receipt
        .as_ref()
        .map_or_else(journal::Admission::default, |receipt| receipt.admission);
    admission.resolved_waiting = true;
    let result = journal.record_admitted(event, suppressed, admission);
    finish_attention_receipt(&result, receipt);
    result
}

fn record_broker_event(
    journal: &mut Journal,
    event: Event,
    suppressed: bool,
    receipt: Option<CommitReceipt>,
    policy: &Preferences,
    registry: impl FnOnce() -> Result<super::agent_execution::SharedRegistry, String>,
) -> Result<Option<Notice>, String> {
    let resolved = receipt.as_ref().is_some_and(|receipt| receipt.resolved);
    if let Target::Automation { id, sequence } = event.target {
        // Management uses this registry guard. The journal edge must be
        // admitted before that transaction resolves it, or skipped afterward.
        let recorded = registry().and_then(|registry| {
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            if registry.automation_attention_resolved(id, sequence)? {
                return Ok(None);
            }
            protect_automation_toast(journal, id, &event.target, |previous| {
                registry.automation_attention_resolved(id, previous)
            })?;
            record_attention_committed_policy(journal, event, suppressed, None, policy, false)
        });
        if let Some(receipt) = receipt {
            receipt.finish(recorded.as_ref().map(|_| ()).map_err(Clone::clone));
        }
        return recorded;
    }
    record_attention_committed_policy(journal, event, suppressed, receipt, policy, resolved)
}

fn record_session_outcome(
    journal: &mut Journal,
    pending: &mut Pending,
    event: Event,
    suppressed: bool,
    policy: &Preferences,
) -> Result<Option<Notice>, String> {
    let result =
        record_attention_committed_policy(journal, event.clone(), suppressed, None, policy, false);
    if result.is_err() {
        pending.queue_event(event);
    }
    result
}

fn protect_automation_toast(
    journal: &mut Journal,
    id: Uuid,
    target: &Target,
    resolved: impl FnOnce(u64) -> Result<bool, String>,
) -> Result<(), String> {
    if let Some((previous, sequence)) = journal.automation_notice(id)?
        && resolved(sequence)?
    {
        // Commit the stable desktop cleanup before the new event can overwrite
        // the only journal row containing the old notice's sequence.
        journal.remember_notice_withdrawal(target, previous)?;
    }
    Ok(())
}

#[derive(Default)]
struct AutomationRetry(Option<(Instant, String)>);

impl AutomationRetry {
    fn deadline(&self) -> Option<Instant> {
        self.0.as_ref().map(|(due, _)| *due)
    }

    fn update(&mut self, result: Result<(), String>, snapshot: &mut Snapshot, now: Instant) {
        match result {
            Ok(()) => {
                if let Some((_, error)) = self.0.take()
                    && snapshot.diagnostic.as_deref() == Some(error.as_str())
                {
                    snapshot.diagnostic = None;
                }
            }
            Err(error) => {
                let due = self
                    .deadline()
                    .filter(|due| *due > now)
                    .unwrap_or(now + RETRY_DELAY);
                snapshot.diagnostic = Some(error.clone());
                self.0 = Some((due, error));
            }
        }
    }
}

fn restore_automation_attention(
    journal: &mut Journal,
    host: &Weak<HostLock>,
    snapshot: &mut Snapshot,
    withdrawals: &mut Withdrawals,
    reopening: bool,
    retry: &mut AutomationRetry,
) {
    let result = check_automation_attention(journal, host, snapshot, withdrawals, reopening);
    retry.update(result, snapshot, Instant::now());
}

fn check_automation_attention(
    journal: &mut Journal,
    host: &Weak<HostLock>,
    snapshot: &mut Snapshot,
    withdrawals: &mut Withdrawals,
    reopening: bool,
) -> Result<(), String> {
    if !snapshot.notices.iter().any(|notice| {
        matches!(notice.event.target, Target::Automation { .. })
            && (reopening || !notice.acknowledged)
    }) {
        return Ok(());
    }
    let Some(host) = host.upgrade() else {
        return Ok(());
    };
    let registry = super::agent_execution::registry(&host)?;
    let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    reconcile_automation_attention(journal, snapshot, withdrawals, reopening, |id, sequence| {
        registry.automation_attention_resolved(id, sequence)
    })
}

fn reconcile_automation_attention(
    journal: &mut Journal,
    snapshot: &mut Snapshot,
    withdrawals: &mut Withdrawals,
    reopening: bool,
    resolved: impl Fn(uuid::Uuid, u64) -> Result<bool, String>,
) -> Result<(), String> {
    let mut failure = None;
    for notice in &mut snapshot.notices {
        let Target::Automation { id, sequence } = notice.event.target else {
            continue;
        };
        if notice.acknowledged && !reopening {
            continue;
        }
        match resolved(id, sequence) {
            Ok(true) => {
                if !notice.acknowledged {
                    match journal.acknowledge(notice.id) {
                        Ok(_) => notice.acknowledged = true,
                        Err(error) => {
                            failure = Some(error);
                            continue;
                        }
                    }
                }
                // A daemon can exit between acknowledging and removing the
                // toast. Retry the idempotent withdrawal when reopening.
                withdraw_notice(withdrawals, notice);
            }
            Ok(false) => {}
            Err(error) => failure = Some(error),
        }
    }
    failure.map_or(Ok(()), Err)
}

fn record_attention_committed(
    journal: &mut Journal,
    event: Event,
    suppressed: bool,
    receipt: Option<CommitReceipt>,
) -> Result<Option<crate::notifications::Notice>, String> {
    let result = match &receipt {
        Some(receipt) => journal.record_admitted(event, suppressed, receipt.admission),
        None => journal.record(event, suppressed),
    };
    finish_attention_receipt(&result, receipt);
    result
}

fn finish_attention_receipt(
    result: &Result<Option<Notice>, String>,
    receipt: Option<CommitReceipt>,
) {
    if let Some(receipt) = receipt {
        let committed = result.as_ref().map(|_| ()).map_err(Clone::clone);
        receipt.finish(committed);
    }
}

/// Drives the real bounded commit queue and journal without a portal or thread.
#[cfg(test)]
pub(in crate::host) struct JournalTestBridge {
    handle: Handle,
    _receiver: mpsc::Receiver<()>,
    journal: Journal,
}

#[cfg(test)]
impl JournalTestBridge {
    pub(in crate::host) fn waiting_clear_pending(&self, session: TerminalId) -> bool {
        self.handle
            .0
            .pending
            .lock()
            .unwrap()
            .cleared_waiting
            .contains(&Target::Session(session))
    }
    pub(in crate::host) fn new(path: &std::path::Path) -> Result<(Handle, Self), String> {
        let journal = Journal::open(path)?;
        let (wake, receiver) = mpsc::channel(1);
        let handle = Handle(Arc::new(Shared {
            pending: Mutex::default(),
            review_slots: Arc::new(Semaphore::new(MAX_REVIEW_UPDATES)),
            retained_turn_slots: Arc::new(Semaphore::new(MAX_PENDING)),
            presence: Mutex::default(),
            snapshot: Mutex::default(),
            wake,
        }));
        Ok((
            handle.clone(),
            Self {
                handle,
                _receiver: receiver,
                journal,
            },
        ))
    }

    pub(in crate::host) fn commit_next(
        &mut self,
        policy: &Preferences,
        suppressed: bool,
    ) -> Result<Option<Notice>, String> {
        self.commit_next_with(|journal, event, receipt| {
            let resolved = receipt.resolved;
            record_attention_committed_policy(
                journal,
                event,
                suppressed,
                Some(receipt),
                policy,
                resolved,
            )
        })
    }

    pub(in crate::host) fn commit_next_automation(
        &mut self,
        registry: &super::agent_execution::SharedRegistry,
    ) -> Result<Option<Notice>, String> {
        self.commit_next_with(|journal, event, receipt| {
            record_broker_event(
                journal,
                event,
                false,
                Some(receipt),
                &Preferences::default(),
                || Ok(registry.clone()),
            )
        })
    }

    pub(in crate::host) fn flush_waiting_clears(&mut self) -> Result<(), String> {
        let mut pending = self.handle.0.pending.lock().unwrap();
        commit_waiting_clears(&mut self.journal, &pending.cleared_waiting, |_, _| {})?;
        pending.cleared_waiting.clear();
        Ok(())
    }

    fn commit_next_with(
        &mut self,
        commit: impl FnOnce(&mut Journal, Event, CommitReceipt) -> Result<Option<Notice>, String>,
    ) -> Result<Option<Notice>, String> {
        self.flush_waiting_clears()?;
        let next = self
            .handle
            .0
            .pending
            .lock()
            .unwrap()
            .committed_events
            .pop_front();
        let Some((event, receipt)) = next else {
            return Ok(None);
        };
        commit(&mut self.journal, event, receipt)
    }

    pub(in crate::host) fn pending_events(&self) -> Vec<Event> {
        self.handle
            .0
            .pending
            .lock()
            .unwrap()
            .committed_events
            .iter()
            .map(|(event, _)| event.clone())
            .collect()
    }

    pub(in crate::host) fn set_stopped(&self, stopped: bool) {
        self.handle.0.pending.lock().unwrap().stopped = stopped;
    }

    pub(in crate::host) fn drop_pending(&self) {
        self.handle
            .0
            .pending
            .lock()
            .unwrap()
            .committed_events
            .clear();
    }

    pub(in crate::host) fn list(&mut self) -> Result<Vec<Notice>, String> {
        self.journal.list()
    }

    pub(in crate::host) fn acknowledge(&mut self, id: Uuid) -> Result<Option<String>, String> {
        self.journal.acknowledge(id)
    }
}

fn journal_path() -> Option<std::path::PathBuf> {
    crate::instance::project_dirs().map(|dirs| dirs.data_dir().join("agent-attention.sqlite3"))
}

async fn run(
    shared: Weak<Shared>,
    host: Weak<HostLock>,
    mut receiver: mpsc::Receiver<()>,
    application: crate::notifications::Application,
) {
    let mut store = journal::Reopening::open(journal_path(), Instant::now());
    // Work held while the journal is unavailable, applied before the next batch.
    let mut deferred = Pending::default();
    let mut predecessor_retry = None;
    let mut review_retry = None;
    let settings = preferences();
    let settings_error = settings.as_ref().err().cloned();
    let mut policy = settings.unwrap_or_else(|_| Preferences {
        enabled: false,
        ..Default::default()
    });
    let mut presence = BTreeMap::<ClientId, Presence>::new();
    let mut proxy = None;
    let mut activation = Activation::default();
    // Only events observed during this daemon lifetime are eligible for retry.
    // Restored attention stays visible in the UI without announcing old backlog.
    let mut delivery = BTreeMap::<Target, Notice>::new();
    let mut delivery_schedule = batching::Schedule::default();
    let mut withdrawals = Withdrawals::default();
    let mut next_attempt = Instant::now();
    let mut retry_delay = RETRY_DELAY;
    let mut withdrawal_retry = None;
    let mut automation_retry = AutomationRetry::default();
    let mut snapshot = Snapshot {
        diagnostic: settings_error.clone(),
        ..Default::default()
    };
    let mut review_monitor = ReviewMonitor::default();
    match store.journal() {
        Ok(journal) => {
            if let Err(error) = restore(journal, &policy, &mut snapshot, &mut withdrawals) {
                next_attempt = Instant::now() + RETRY_DELAY;
                withdrawal_retry = Some((next_attempt, error.clone()));
                snapshot.diagnostic = Some(error);
            }
            restore_automation_attention(
                journal,
                &host,
                &mut snapshot,
                &mut withdrawals,
                true,
                &mut automation_retry,
            );
            expire_review_notices(
                &mut review_monitor,
                journal,
                &mut snapshot,
                &mut withdrawals,
            );
        }
        Err(error) => snapshot.diagnostic = Some(error.clone()),
    }
    publish(&shared, &host, snapshot.clone());
    'broker: while wait_for_work(
        &mut receiver,
        store.retry_at().or_else(|| {
            if proxy.is_none() {
                if !withdrawals.is_empty() {
                    Some(next_attempt)
                } else {
                    delivery_deadline(&delivery, &delivery_schedule, &deferred.review_updates)
                        .map(|due| due.max(next_attempt))
                }
            } else {
                delivery_deadline(&delivery, &delivery_schedule, &deferred.review_updates)
            }
            .into_iter()
            .chain(predecessor_retry.as_ref().map(|(due, _)| *due))
            .chain(withdrawal_retry.as_ref().map(|(due, _)| *due))
            .chain(automation_retry.deadline())
            .chain(review_retry.as_ref().map(|(due, _)| *due))
            .chain(review_monitor.deadline().filter(|_| review_retry.is_none()))
            .min()
        }),
    )
    .await
    {
        let Some(core) = shared.upgrade() else {
            break;
        };
        let mut pending = std::mem::take(&mut deferred).followed_by(take_pending(&core));
        let current_presence = pending.presence_changed.then(|| {
            core.presence
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        });
        drop(core);
        if pending.stopped {
            if !pending.cleared_waiting.is_empty()
                || !pending.predecessor_reads.is_empty()
                || !pending.review_updates.is_empty()
                || !pending.started_turns.is_empty()
                || !pending.retained_turns.is_empty()
                || !pending.retired.is_empty()
                || pending
                    .events
                    .keys()
                    .any(|target| matches!(target, Target::Session(_)))
            {
                match store.journal_for_shutdown() {
                    Ok(journal) => {
                        settle_attention_at_shutdown(&shared, journal, &mut pending);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not reopen attention journal at shutdown")
                    }
                }
            }
            break;
        }
        if let Some(owner) = pending.portal_owner.take()
            && activation.portal_changed(owner)
        {
            // Register with the new portal before anything else calls it, or
            // it identifies this connection without the application ID.
            proxy = None;
            next_attempt = Instant::now();
        }
        if let Some(recovered) = store.reopen_if_due(Instant::now())
            && let Ok(journal) = store.journal()
        {
            tracing::info!(error = %recovered, "the notification journal is available again");
            // The journal error replaced any settings error at startup;
            // recovering the journal shows that one again.
            if snapshot.diagnostic.as_ref() == Some(&recovered) {
                snapshot.diagnostic.clone_from(&settings_error);
            }
            if let Err(error) = restore(journal, &policy, &mut snapshot, &mut withdrawals) {
                snapshot.diagnostic = Some(error);
            }
            restore_automation_attention(
                journal,
                &host,
                &mut snapshot,
                &mut withdrawals,
                true,
                &mut automation_retry,
            );
        }
        let journal = match store.journal() {
            Ok(journal) => journal,
            Err(error) => {
                predecessor_retry = None;
                let error = error.clone();
                for (_, receipt) in pending.committed_events.drain(..) {
                    receipt.finish(Err(error.clone()));
                }
                // Without a journal there is no backlog to deliver, but the
                // portal may be fine, so a test is still answered by the portal
                // itself rather than dropped as if the broker had stopped.
                if !pending.tests.is_empty() {
                    if proxy.is_none() {
                        match connect(shared.clone(), application, &mut activation).await {
                            Ok(connected) => proxy = Some(connected),
                            Err(error) => {
                                answer_tests(&pending.tests, &TestOutcome::Unavailable(error))
                            }
                        }
                    }
                    if let Some(backend) = &proxy {
                        let outcome = send_test(backend).await;
                        if outcome != TestOutcome::Sent {
                            proxy = None;
                        }
                        answer_tests(&pending.tests, &outcome);
                    }
                }
                deferred = pending.deferred();
                continue;
            }
        };
        if let Err(error) =
            commit_waiting_clears(journal, &pending.cleared_waiting, |target, id| {
                if delivery.get(target).is_some_and(|notice| notice.id == id) {
                    delivery.remove(target);
                }
                withdraw_target(&mut withdrawals, id, target);
            })
        {
            snapshot.diagnostic = Some(error.clone());
            publish(&shared, &host, snapshot.clone());
            proxy = None;
            next_attempt = store.retry_failed_write(error, Instant::now());
            // Reapply these clears before a later request for the same
            // conversation is allowed to commit. Retained receipts can replay.
            deferred = pending.deferred();
            continue;
        }
        pending.cleared_waiting.clear();
        if let Some(current) = current_presence {
            for (client, update) in &current {
                if settle_attention_at_shutdown(&shared, journal, &mut pending) {
                    return;
                }
                if presence.is_empty() {
                    next_attempt = Instant::now();
                    retry_delay = RETRY_DELAY;
                }
                let changed = presence.get(client).is_none_or(|old| {
                    old.sessions != update.sessions || old.destination() != update.destination()
                });
                let sessions = if changed {
                    update.sessions.clone()
                } else {
                    update.focused.into_iter().collect()
                };
                if let Err(error) = journal.remember_window(sessions, &update.window) {
                    snapshot.diagnostic = Some(error);
                }
            }
            presence = current;
        }
        match pending.flush_started_turns(journal) {
            Ok(rows) => withdraw_sessions(&mut withdrawals, &mut delivery, rows),
            Err(error) => {
                // A later outcome may replace the unread notice for this
                // same session. Hold it until the earlier boundary commits.
                snapshot.diagnostic = Some(error.clone());
                publish(&shared, &host, snapshot.clone());
                answer_tests(&pending.tests, &TestOutcome::Unavailable(error.clone()));
                store.suspend(error, Instant::now());
                deferred = pending.deferred();
                continue;
            }
        }
        if pending.reload {
            match pending.preferences.take().map_or_else(preferences, Ok) {
                Ok(settings) => policy = settings,
                Err(error) => {
                    policy.enabled = false;
                    snapshot.diagnostic = Some(error);
                }
            }
            if !policy.message_previews {
                revoke_previews(&mut snapshot, &mut withdrawals);
                match journal.clear_content() {
                    Ok(notices) => {
                        for (id, target) in notices {
                            withdraw_target(&mut withdrawals, id, &target);
                        }
                    }
                    Err(error) => snapshot.diagnostic = Some(error),
                }
                for notice in delivery.values_mut() {
                    notice.event.content = None;
                }
            }
            // Disabling delivery or a category also retracts its already
            // published desktop group. Attention remains in the journal;
            // preferences never manufacture an acknowledgement.
            for notice in &snapshot.notices {
                let focused = presence.values().any(|window| {
                    notice
                        .event
                        .source_session()
                        .is_some_and(|session| window.focused == Some(session))
                        || window.focused_target.as_ref() == Some(&notice.event.target)
                });
                if !policy.allows(notice.event.evidence.kind, focused) {
                    withdraw_notice(&mut withdrawals, notice);
                }
            }
            next_attempt = Instant::now();
            retry_delay = RETRY_DELAY;
        }
        if let Some(inboxes) = pending.inboxes.take() {
            // A complete durable pending snapshot retires inbox attention only
            // when mail is gone; offline recipients keep existing attention.
            match journal.list() {
                Ok(notices) => {
                    for notice in notices {
                        if let Target::SessionInbox(lineage) = notice.event.target
                            && !inboxes.pending.contains(&lineage)
                        {
                            pending.retired.insert(Target::SessionInbox(lineage));
                        }
                    }
                }
                Err(error) => snapshot.diagnostic = Some(error),
            }
            pending.events.extend(inboxes.events);
        }
        // Finish any marker proved by the closing actor while its durable
        // replay fence still exists. A replacement marker follows retirement.
        if let Err(error) = pending.flush_retained_turns(journal, true, |rows| {
            withdraw_sessions(&mut withdrawals, &mut delivery, rows);
        }) {
            snapshot.diagnostic = Some(error.clone());
            publish(&shared, &host, snapshot.clone());
            answer_tests(&pending.tests, &TestOutcome::Unavailable(error.clone()));
            proxy = None;
            next_attempt = store.retry_failed_write(error, Instant::now());
            deferred = pending.deferred();
            continue;
        }
        while !pending.retired.is_empty() {
            if settle_attention_at_shutdown(&shared, journal, &mut pending) {
                return;
            }
            let target = match pending.commit_next_retirement(journal) {
                Ok(Some(target)) => target,
                Ok(None) => break,
                Err(error) => {
                    snapshot.diagnostic = Some(error.clone());
                    publish(&shared, &host, snapshot.clone());
                    answer_tests(&pending.tests, &TestOutcome::Unavailable(error.clone()));
                    proxy = None;
                    next_attempt = store.retry_failed_write(error, Instant::now());
                    deferred = pending.deferred();
                    continue 'broker;
                }
            };
            delivery.remove(&target);
            if let Some(old) = snapshot
                .notices
                .iter()
                .find(|notice| notice.event.target() == target)
            {
                withdraw_notice(&mut withdrawals, old);
            }
        }
        match apply_predecessor_reads(
            journal,
            &mut pending.predecessor_reads,
            &mut delivery,
            |id, session| {
                withdraw_target(&mut withdrawals, id, &Target::Session(session));
            },
        ) {
            Ok(()) => {
                if let Some((_, error)) = predecessor_retry.take()
                    && snapshot.diagnostic.as_ref() == Some(&error)
                {
                    snapshot.diagnostic.clone_from(&settings_error);
                }
            }
            Err(error) => {
                deferred.read_predecessors(pending.predecessor_reads.clone());
                let now = Instant::now();
                let due = predecessor_retry
                    .as_ref()
                    .map(|(due, _)| *due)
                    .filter(|due| *due > now)
                    .unwrap_or(now + RETRY_DELAY);
                snapshot.diagnostic = Some(error.clone());
                predecessor_retry = Some((due, error));
            }
        }
        for id in std::mem::take(&mut pending.acknowledged) {
            if settle_attention_at_shutdown(&shared, journal, &mut pending) {
                return;
            }
            delivery.retain(|_, notice| notice.id != id);
            if let Err(error) = journal.acknowledge(id) {
                snapshot.diagnostic = Some(error);
            }
            if let Some(notice) = snapshot.notices.iter().find(|notice| notice.id == id) {
                withdraw_notice(&mut withdrawals, notice);
            }
        }
        if settle_attention_at_shutdown(&shared, journal, &mut pending) {
            return;
        }
        if let Err(error) = pending.flush_retained_turns(journal, false, |rows| {
            withdraw_sessions(&mut withdrawals, &mut delivery, rows);
        }) {
            snapshot.diagnostic = Some(error.clone());
            publish(&shared, &host, snapshot.clone());
            answer_tests(&pending.tests, &TestOutcome::Unavailable(error.clone()));
            proxy = None;
            next_attempt = store.retry_failed_write(error, Instant::now());
            deferred = pending.deferred();
            continue;
        }
        let now = Instant::now();
        if review_retry.as_ref().is_none_or(|(due, _)| *due <= now) {
            match apply_review_updates(
                &mut review_monitor,
                journal,
                &mut snapshot,
                &mut withdrawals,
                &mut pending.review_updates,
                ReviewDelivery {
                    pending: &mut delivery,
                    schedule: &mut delivery_schedule,
                    presence: &presence,
                    policy: &policy,
                },
            ) {
                Ok(()) => {
                    if let Some((_, error)) = review_retry.take()
                        && snapshot.diagnostic.as_ref() == Some(&error)
                    {
                        snapshot.diagnostic.clone_from(&settings_error);
                    }
                }
                Err(error) => {
                    snapshot.diagnostic = Some(error.clone());
                    review_retry = Some((Instant::now() + RETRY_DELAY, error));
                }
            }
        }
        while !pending.events.is_empty() || !pending.committed_events.is_empty() {
            if settle_attention_at_shutdown(&shared, journal, &mut pending) {
                return;
            }
            let (event, receipt) = pending
                .events
                .pop_first()
                .map(|(_, event)| (event, None))
                .or_else(|| {
                    pending
                        .committed_events
                        .pop_front()
                        .map(|(event, receipt)| (event, Some(receipt)))
                })
                .expect("checked that an event remains");
            let focused = presence.values().any(|window| {
                event
                    .source_session()
                    .is_some_and(|session| window.focused == Some(session))
                    || window.focused_target.as_ref() == Some(&event.target)
            });
            let suppressed = !policy.allows(event.evidence.kind, focused);
            let resolved = receipt.as_ref().is_some_and(|receipt| receipt.resolved);
            let direct_session = receipt.is_none() && matches!(event.target, Target::Session(_));
            let recorded = if direct_session {
                record_session_outcome(journal, &mut pending, event, suppressed, &policy)
            } else {
                record_broker_event(journal, event, suppressed, receipt, &policy, || {
                    let host = host.upgrade().ok_or("The daemon has stopped")?;
                    super::agent_execution::registry(&host)
                })
            };
            match recorded {
                Ok(Some(notice)) => {
                    delivery.remove(&notice.event.target());
                    if let Some(old) = snapshot.notices.iter().find(|old| {
                        old.event.target.storage_key() == notice.event.target.storage_key()
                    }) {
                        if suppressed || resolved {
                            withdraw_notice(&mut withdrawals, old);
                        } else {
                            queue_withdrawal(&mut withdrawals, old.id);
                        }
                    }
                    if !suppressed && !resolved {
                        delivery_schedule.update(
                            notice.event.target(),
                            notice.event.evidence.kind,
                            Instant::now(),
                        );
                        delivery.insert(notice.event.target(), notice);
                    }
                }
                Ok(None) => {}
                Err(error) if direct_session => {
                    snapshot.diagnostic = Some(error.clone());
                    publish(&shared, &host, snapshot.clone());
                    answer_tests(&pending.tests, &TestOutcome::Unavailable(error.clone()));
                    proxy = None;
                    next_attempt = store.retry_failed_write(error, Instant::now());
                    deferred = pending.deferred();
                    continue 'broker;
                }
                Err(error) => snapshot.diagnostic = Some(error),
            }
        }
        match journal.list() {
            Ok(notices) => snapshot.notices = notices,
            Err(error) => snapshot.diagnostic = Some(error),
        }
        // An admitted watcher renewal must reach the journal before expiry.
        if review_retry.is_none() {
            let retired_review_ids = expire_review_notices(
                &mut review_monitor,
                journal,
                &mut snapshot,
                &mut withdrawals,
            );
            if !retired_review_ids.is_empty() {
                delivery.retain(|_, notice| !retired_review_ids.contains(&notice.id));
            }
        }
        restore_automation_attention(
            journal,
            &host,
            &mut snapshot,
            &mut withdrawals,
            false,
            &mut automation_retry,
        );
        let persisted_withdrawals = match reserve_withdrawals(journal, &snapshot, &mut withdrawals)
        {
            Ok(groups) => {
                if let Some((_, error)) = withdrawal_retry.take()
                    && snapshot.diagnostic.as_deref() == Some(error.as_str())
                {
                    snapshot.diagnostic = settings_error.clone();
                }
                groups
            }
            Err(error) => {
                // The same stable desktop group may still have an older
                // withdrawal in the journal. Wait before publishing or
                // activating another notice on that group.
                snapshot.diagnostic = Some(error.clone());
                if !policy.message_previews {
                    for notice in &mut snapshot.notices {
                        notice.event.content = None;
                    }
                }
                publish(&shared, &host, snapshot.clone());
                answer_tests(&pending.tests, &TestOutcome::Unavailable(error.clone()));
                proxy = None;
                next_attempt = Instant::now() + retry_delay;
                withdrawal_retry = Some((next_attempt, error));
                retry_delay = (retry_delay * 2).min(Duration::from_secs(300));
                deferred.review_updates = std::mem::take(&mut pending.review_updates);
                continue;
            }
        };
        pending
            .actions
            .retain(|(id, _)| !persisted_withdrawals.contains(id));
        if !policy.message_previews {
            for notice in &mut snapshot.notices {
                notice.event.content = None;
            }
        }
        for notice in delivery.values_mut() {
            if let Some(current) = snapshot
                .notices
                .iter()
                .find(|current| current.id == notice.id)
            {
                *notice = current.clone();
            }
        }
        // Journal eviction also bounds retries across multiple disconnected
        // batches. Never announce an event whose durable identity was evicted.
        let retained = snapshot
            .notices
            .iter()
            .map(|notice| notice.id)
            .collect::<BTreeSet<_>>();
        delivery.retain(|_, notice| !notice.acknowledged && retained.contains(&notice.id));
        // A delayed completion stops being a desktop alert once the user has
        // returned to that pane or disabled its category.
        delivery.retain(|_, notice| {
            let focused = presence.values().any(|window| {
                notice
                    .event
                    .source_session()
                    .is_some_and(|session| window.focused == Some(session))
                    || window.focused_target.as_ref() == Some(&notice.event.target)
            });
            policy.allows(notice.event.evidence.kind, focused)
        });
        let now = Instant::now();
        let backlog_due =
            (delivery_deadline(&delivery, &delivery_schedule, &pending.review_updates)
                .is_some_and(|due| due <= now)
                || !withdrawals.is_empty())
                && now >= next_attempt;
        // A test is a person asking right now, so it retries a portal that
        // the backlog is still backing off from. Its failure is answered to
        // that person rather than raised as every window's diagnostic, and
        // leaves the backlog's schedule alone.
        if proxy.is_none() && (backlog_due || !pending.tests.is_empty()) {
            match connect(shared.clone(), application, &mut activation).await {
                Ok(connected) => {
                    proxy = Some(connected);
                    snapshot.diagnostic = None;
                    retry_delay = RETRY_DELAY;
                }
                Err(error) => {
                    answer_tests(&pending.tests, &TestOutcome::Unavailable(error.clone()));
                    if backlog_due {
                        snapshot.diagnostic = Some(error);
                        next_attempt = Instant::now() + retry_delay;
                        retry_delay = (retry_delay * 2).min(Duration::from_secs(300));
                    }
                }
            }
        }
        if let Some(backend) = &proxy {
            let mut failed = false;
            // Ahead of the backlog, so the answer is not held behind it.
            if !pending.tests.is_empty() {
                let outcome = send_test(backend).await;
                failed = outcome != TestOutcome::Sent;
                answer_tests(&pending.tests, &outcome);
            }
            // Yield to coalesced lifecycle/focus/acknowledgment work between
            // bounded batches, rather than delaying it behind thousands of alerts.
            // A portal that just failed the test is dropped below, not
            // handed the backlog as well.
            let batch: Vec<_> = if failed {
                Vec::new()
            } else {
                delivery_batch(
                    &delivery,
                    &delivery_schedule,
                    &withdrawals,
                    &pending.review_updates,
                    Instant::now(),
                )
            };
            for notice in batch {
                if settle_attention_at_shutdown(&shared, journal, &mut pending) {
                    return;
                }
                // Portal connection and test delivery can each await.
                // Check for settlements accepted while this batch was paused.
                if shared_review_census_pending(&shared, &notice.event.target) {
                    // Its queued wake will apply the census, or defer this
                    // target until the journal retry. Leave delivery pending.
                    continue;
                }
                if session_settlement_admitted(&shared, &notice.event) {
                    delivery.remove(&notice.event.target);
                    continue;
                }
                // A retained marker can be a replay. Let the journal admit it
                // before sending, without discarding a potentially newer toast.
                if retained_turn_pending(&shared, &notice.event) {
                    continue;
                }
                if let Target::Automation { id, sequence } = notice.event.target {
                    let resolved = host
                        .upgrade()
                        .ok_or_else(|| "The daemon has stopped".to_owned())
                        .and_then(|host| super::agent_execution::registry(&host))
                        .and_then(|registry| {
                            registry
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .automation_attention_resolved(id, sequence)
                        });
                    match resolved {
                        Ok(true) => {
                            let result = discard_resolved_delivery(
                                journal,
                                &mut snapshot,
                                &mut withdrawals,
                                &mut delivery,
                                &notice,
                            );
                            if result.is_err() {
                                automation_retry.update(result, &mut snapshot, Instant::now());
                            }
                            continue;
                        }
                        Ok(false) => {}
                        Err(error) => {
                            snapshot.diagnostic = Some(error);
                            failed = true;
                            break;
                        }
                    }
                }
                match send(backend, &notice, activation.receives_actions()).await {
                    Ok(()) => {
                        delivery.remove(&notice.event.target());
                        if let Err(error) = journal.delivered(notice.id) {
                            snapshot.diagnostic = Some(error);
                        }
                    }
                    Err(error) => {
                        snapshot.diagnostic = Some(error);
                        failed = true;
                        break;
                    }
                }
            }
            // Cleanup is bounded like delivery. A desktop that stops answering
            // cannot hold daemon shutdown behind thousands of remove calls.
            for _ in 0..8 {
                if settle_attention_at_shutdown(&shared, journal, &mut pending) {
                    return;
                }
                if failed {
                    break;
                }
                let Some(entry) = withdrawals.pop_first() else {
                    break;
                };
                let result = remove(backend, entry.id())
                    .await
                    .and_then(|()| journal.finish_group_withdrawal(entry.id()));
                if let Err(error) = result {
                    withdrawals.requeue(entry);
                    snapshot.diagnostic = Some(error);
                    failed = true;
                    break;
                }
            }
            if failed {
                proxy = None;
                next_attempt = Instant::now() + retry_delay;
                retry_delay = (retry_delay * 2).min(Duration::from_secs(300));
            } else if (delivery_deadline(&delivery, &delivery_schedule, &pending.review_updates)
                .is_some_and(|due| due <= Instant::now())
                || !withdrawals.is_empty())
                && let Some(shared) = shared.upgrade()
            {
                let _ = shared.wake.try_send(());
            }
        }
        match journal.list() {
            Ok(notices) => snapshot.notices = notices,
            Err(error) => snapshot.diagnostic = Some(error),
        }
        restore_automation_attention(
            journal,
            &host,
            &mut snapshot,
            &mut withdrawals,
            false,
            &mut automation_retry,
        );
        if !withdrawals.is_empty()
            && proxy.is_some()
            && let Some(shared) = shared.upgrade()
        {
            let _ = shared.wake.try_send(());
        }
        if !policy.message_previews {
            for notice in &mut snapshot.notices {
                notice.event.content = None;
            }
        }
        for (action, token) in pending.actions {
            if let Some(notice) = action_notice(&snapshot, action)
                && let Some(host) = host.upgrade()
            {
                let window = match route_notification_action(&host, notice) {
                    Ok(Some(window)) => window,
                    Ok(None) => continue,
                    Err(error) => {
                        snapshot.diagnostic = Some(error);
                        continue;
                    }
                };
                // Activation and the matched executable handshake can wait;
                // they never run under the daemon's session lock.
                match crate::instance::activate_destination(&window, token.as_deref()) {
                    Ok(true) => {}
                    Ok(false) => {
                        if let Err(error) = crate::instance::launch_notification_destination(
                            &window,
                            token.as_deref(),
                        ) {
                            tracing::warn!(%error, "could not launch notification destination");
                            snapshot.diagnostic =
                                Some(format!("Could not open the notification's window: {error}"));
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not activate notification destination");
                        snapshot.diagnostic = Some(format!(
                            "Could not activate the notification's window: {error}"
                        ))
                    }
                }
            }
        }
        let delivery_groups = delivery
            .keys()
            .map(batching::desktop_id)
            .collect::<BTreeSet<_>>();
        delivery_schedule.retain(|group| delivery_groups.contains(group));
        // Keep the unfinished FIFO in this batch until all shutdown checks are done.
        deferred.review_updates = std::mem::take(&mut pending.review_updates);
        publish(&shared, &host, snapshot.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attention_event(session: TerminalId, key: &str) -> Event {
        Event::session(
            session,
            crate::agent::Provider::Claude,
            "/repo".into(),
            crate::notifications::Evidence {
                key: key.into(),
                kind: crate::notifications::Kind::Succeeded,
            },
        )
    }

    #[test]
    fn predecessor_read_uses_committed_ids_and_preserves_a_later_replacement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let (handle, mut bridge) = JournalTestBridge::new(&path).unwrap();
        let predecessor = TerminalId::new();
        let replaced_predecessor = TerminalId::new();
        let unrelated = TerminalId::new();
        let receipt = handle
            .record_committed(attention_event(predecessor, "first"))
            .unwrap();
        let previous = bridge
            .commit_next(&Preferences::default(), false)
            .unwrap()
            .unwrap();
        receipt.recv().unwrap().unwrap();
        assert!(handle.snapshot().notices.is_empty());
        let replaced = bridge
            .journal
            .record(attention_event(replaced_predecessor, "earlier"), false)
            .unwrap()
            .unwrap();
        let unrelated = bridge
            .journal
            .record(attention_event(unrelated, "outside"), false)
            .unwrap()
            .unwrap();
        let mut reads = Journal::open_reader(&path)
            .unwrap()
            .unread_session_ids(&[predecessor, replaced_predecessor].into())
            .unwrap();
        assert_eq!(
            reads,
            [
                (previous.id, predecessor),
                (replaced.id, replaced_predecessor)
            ]
            .into()
        );
        let later = bridge
            .journal
            .record(attention_event(replaced_predecessor, "later"), false)
            .unwrap()
            .unwrap();
        let mut retired = Vec::new();
        apply_predecessor_reads(
            &mut bridge.journal,
            &mut reads,
            &mut BTreeMap::new(),
            |id, _| retired.push(id),
        )
        .unwrap();
        assert_eq!(retired, [previous.id]);
        let visible = bridge.list().unwrap();
        assert!(
            visible
                .iter()
                .find(|notice| notice.id == previous.id)
                .unwrap()
                .acknowledged
        );
        assert!(visible.contains(&unrelated));
        assert!(visible.contains(&later));
    }

    #[test]
    fn retry_after_a_failed_predecessor_write_keeps_its_original_ids() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let predecessor = TerminalId::new();
        let previous = journal
            .record(attention_event(predecessor, "before"), false)
            .unwrap()
            .unwrap();
        let mut reads = [(previous.id, predecessor)].into();
        let unrelated = journal
            .record(attention_event(TerminalId::new(), "unrelated"), false)
            .unwrap()
            .unwrap();
        let mut delivery = [
            (previous.event.target(), previous.clone()),
            (unrelated.event.target(), unrelated.clone()),
        ]
        .into();
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TRIGGER refuse_ack BEFORE UPDATE OF acknowledged ON notices BEGIN SELECT raise(ABORT, 'temporarily cannot write'); END;").unwrap();
        assert!(
            apply_predecessor_reads(
                &mut journal,
                &mut reads,
                &mut delivery,
                |_, _| unreachable!()
            )
            .is_err()
        );
        assert_eq!(reads, [(previous.id, predecessor)].into());
        assert_eq!(
            delivery,
            [(unrelated.event.target(), unrelated.clone())].into()
        );
        db.execute_batch("DROP TRIGGER refuse_ack").unwrap();
        let replacement = journal
            .record(attention_event(predecessor, "after"), false)
            .unwrap()
            .unwrap();
        apply_predecessor_reads(
            &mut journal,
            &mut reads,
            &mut delivery,
            |_, _| unreachable!(),
        )
        .unwrap();
        assert!(reads.is_empty());
        assert_eq!(journal.list().unwrap(), [replacement, unrelated]);
    }

    #[test]
    fn shutdown_settles_only_the_exact_predecessor_reads_already_accepted() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let first = TerminalId::new();
        let second = TerminalId::new();
        let replaced = TerminalId::new();
        let before_first = journal
            .record(attention_event(first, "old:first"), false)
            .unwrap()
            .unwrap();
        let before_second = journal
            .record(attention_event(second, "old:second"), false)
            .unwrap()
            .unwrap();
        let old = journal
            .record(attention_event(replaced, "old:replaced"), false)
            .unwrap()
            .unwrap();
        let later = journal
            .record(attention_event(replaced, "new:replaced"), false)
            .unwrap()
            .unwrap();
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        handle.change(|pending| {
            pending.read_predecessors([(before_first.id, first), (old.id, replaced)].into())
        });
        let mut in_flight = take_pending(&shared);
        handle.change(|pending| pending.read_predecessors([(before_second.id, second)].into()));
        shared.pending.lock().unwrap().stopped = true;
        assert!(settle_attention_at_shutdown(
            &Arc::downgrade(&shared),
            &mut journal,
            &mut in_flight,
        ));
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        let notices = journal.list().unwrap();
        assert!(notices.contains(&later));
        assert!(
            notices
                .iter()
                .filter(|notice| notice.id == before_first.id || notice.id == before_second.id)
                .all(|notice| notice.acknowledged)
        );
    }

    #[test]
    fn review_admission_counts_detached_and_deferred_updates() {
        use crate::notifications::review::{Census, MonitorUpdate};
        use review_monitor::tests::{point, repo};

        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let census = |revision| {
            MonitorUpdate::Completed(Census {
                repository: repo(None),
                start: point(revision, 100),
                viewer: "alice".into(),
                current: BTreeMap::new(),
            })
        };
        let limit = MAX_REVIEW_UPDATES as u64;
        for revision in 1..=limit / 2 {
            handle.observe_review_monitor(census(revision)).unwrap();
        }
        let in_flight = take_pending(&shared);
        for revision in limit / 2 + 1..=limit {
            handle.observe_review_monitor(census(revision)).unwrap();
        }
        assert!(handle.observe_review_monitor(census(limit + 1)).is_err());

        let mut pending = in_flight.deferred().followed_by(take_pending(&shared));
        assert!(handle.observe_review_monitor(census(limit + 1)).is_err());
        assert_eq!(
            pending
                .review_updates
                .iter()
                .map(|queued| match &queued.update {
                    ReviewUpdate::Monitor(MonitorUpdate::Completed(census)) =>
                        census.start.revision,
                    _ => panic!("expected an admitted census"),
                })
                .collect::<Vec<_>>(),
            (1..=limit).collect::<Vec<_>>()
        );

        pending.review_updates.pop_front();
        handle.observe_review_monitor(census(limit + 1)).unwrap();
        pending = pending.followed_by(take_pending(&shared));
        assert_eq!(pending.review_updates.len(), MAX_REVIEW_UPDATES);
        assert!(matches!(
            &pending.review_updates.back().unwrap().update,
            ReviewUpdate::Monitor(MonitorUpdate::Completed(census))
                if census.start.revision == limit + 1
        ));
    }

    #[test]
    fn a_failed_accepted_census_retries_in_order_without_a_live_sender() {
        exercise_accepted_review_retry(false);
    }

    #[test]
    fn shutdown_retries_a_detached_census_before_newer_shared_review_work() {
        exercise_accepted_review_retry(true);
    }

    fn exercise_accepted_review_retry(shutdown: bool) {
        use crate::notifications::review::{Census, MonitorUpdate, ObservedRequests};
        use review_monitor::tests::{observed, point, repo};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER interrupt_review_census BEFORE INSERT ON review_censuses
                 BEGIN SELECT RAISE(ABORT, 'temporary census write failure'); END;",
            )
            .unwrap();

        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let repository = repo(None);
        let seconds = review_clock() as u64;
        let legacy = |key, number| {
            let request = observed(&repository, "alice", key, number, point(1, seconds));
            ObservedRequests {
                destination: request.destination,
                requests: request
                    .requests
                    .into_iter()
                    .map(|entry| entry.request)
                    .collect(),
            }
        };
        handle.observe_review_requests(legacy("before", 1)).unwrap();
        handle
            .observe_review_monitor(MonitorUpdate::Completed(Census {
                repository: repository.clone(),
                start: point(1, seconds),
                viewer: "alice".into(),
                current: BTreeMap::new(),
            }))
            .unwrap();
        if !shutdown {
            handle.observe_review_requests(legacy("after", 2)).unwrap();
        }
        let mut pending = take_pending(&shared);
        let handle = if shutdown {
            Some(handle)
        } else {
            drop(handle);
            None
        };

        let mut monitor = ReviewMonitor::default();
        let mut snapshot = Snapshot::default();
        let mut withdrawals = Withdrawals::default();
        let failed = apply_review_updates(
            &mut monitor,
            &mut journal,
            &mut snapshot,
            &mut withdrawals,
            &mut pending.review_updates,
            ReviewDelivery {
                pending: &mut BTreeMap::new(),
                schedule: &mut batching::Schedule::default(),
                presence: &BTreeMap::new(),
                policy: &Preferences::default(),
            },
        );
        assert!(
            failed
                .unwrap_err()
                .contains("temporary census write failure")
        );
        assert_eq!(pending.review_updates.len(), if shutdown { 1 } else { 2 });
        let before = journal.list().unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].event.evidence.key, "before");
        assert!(!before[0].acknowledged);

        connection
            .execute_batch("DROP TRIGGER interrupt_review_census")
            .unwrap();
        if let Some(handle) = handle {
            let session = TerminalId::new();
            let prior_turn = journal
                .record(attention_event(session, "prior turn"), false)
                .unwrap()
                .unwrap();
            handle.observe_review_requests(legacy("after", 2)).unwrap();
            handle.session_started_turn(session, BTreeSet::new());
            handle.change(|pending| pending.stopped = true);
            drop(handle);
            assert!(settle_attention_at_shutdown(
                &Arc::downgrade(&shared),
                &mut journal,
                &mut pending,
            ));
            assert!(shared.pending.lock().unwrap().review_updates.is_empty());
            drop(journal);
            journal = Journal::open(&path).unwrap();
            assert!(journal.list().unwrap().iter().any(|notice| {
                notice.event.evidence.key == "after" && notice.delivered && !notice.acknowledged
            }));
            assert!(
                journal
                    .list()
                    .unwrap()
                    .iter()
                    .any(|notice| notice.id == prior_turn.id && notice.acknowledged)
            );
        } else {
            drop(shared);
            apply_review_updates(
                &mut monitor,
                &mut journal,
                &mut snapshot,
                &mut withdrawals,
                &mut pending.review_updates,
                ReviewDelivery {
                    pending: &mut BTreeMap::new(),
                    schedule: &mut batching::Schedule::default(),
                    presence: &BTreeMap::new(),
                    policy: &Preferences::default(),
                },
            )
            .unwrap();
            assert!(withdrawals.contains(&batching::desktop_id(&before[0].event.target)));
        }
        assert!(pending.review_updates.is_empty());
        assert!(journal.group_withdrawals().unwrap().contains(&before[0].id));
        assert_eq!(
            journal
                .list()
                .unwrap()
                .into_iter()
                .filter(|notice| matches!(notice.event.target, Target::PullRequest(_)))
                .map(|notice| (notice.event.evidence.key, notice.acknowledged))
                .collect::<BTreeMap<_, _>>(),
            BTreeMap::from([("after".into(), false), ("before".into(), true)])
        );
    }

    #[test]
    fn a_pending_review_census_blocks_only_its_missing_requests_from_portal_delivery() {
        use crate::notifications::review::{Census, MonitorUpdate};
        use review_monitor::tests::{observed, point, repo};

        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let repository = repo(None);
        let start = point(1, review_clock() as u64);
        let make = |repository, viewer, key, number| {
            let (event, _) = observed(repository, viewer, key, number, start.clone())
                .events()
                .next()
                .unwrap();
            Notice {
                id: Uuid::new_v4(),
                event,
                delivered: false,
                acknowledged: false,
                window: None,
            }
        };
        let missing = make(&repository, "alice", "missing", 1);
        let current = make(&repository, "alice", "current", 2);
        let other_viewer = make(&repository, "bob", "missing", 1);
        let other_repository = make(&repo(Some(1234)), "alice", "missing", 1);
        handle
            .observe_review_monitor(MonitorUpdate::Completed(Census {
                repository,
                start,
                viewer: "alice".into(),
                current: [(2, "current".into())].into(),
            }))
            .unwrap();

        let shared = Arc::downgrade(&shared);
        for notice in [&current, &other_viewer, &other_repository] {
            assert!(!shared_review_census_pending(&shared, &notice.event.target));
        }
        assert!(shared_review_census_pending(&shared, &missing.event.target));
        let detached = take_pending(&handle.0);
        assert!(!shared_review_census_pending(
            &shared,
            &missing.event.target
        ));

        let now = Instant::now();
        let mut schedule = batching::Schedule::default();
        schedule.update(missing.event.target(), Kind::ReviewRequested, now);
        let mut delivery = BTreeMap::from([(missing.event.target(), missing)]);
        assert_eq!(
            delivery_deadline(&delivery, &schedule, &detached.review_updates),
            None
        );

        for notice in [&current, &other_viewer, &other_repository] {
            schedule.update(
                notice.event.target(),
                Kind::ReviewRequested,
                now + Duration::from_secs(1),
            );
            delivery.insert(notice.event.target(), notice.clone());
        }
        assert_eq!(
            delivery_deadline(&delivery, &schedule, &detached.review_updates),
            Some(now + Duration::from_millis(1600)),
        );
        let eligible = delivery_batch(
            &delivery,
            &schedule,
            &Withdrawals::default(),
            &detached.review_updates,
            now + Duration::from_secs(2),
        )
        .into_iter()
        .map(|notice| notice.id)
        .collect::<BTreeSet<_>>();
        assert_eq!(
            eligible,
            [current.id, other_viewer.id, other_repository.id].into()
        );
    }

    #[test]
    fn an_old_client_positive_after_an_accepted_census_survives_its_replay() {
        use crate::notifications::review::{Census, MonitorUpdate, ObservedRequests};
        use review_monitor::tests::{observed, point, repo};
        let directory = tempfile::tempdir().unwrap();
        let (handle, mut bridge) =
            JournalTestBridge::new(&directory.path().join("attention.sqlite")).unwrap();
        let repository = repo(None);
        let seconds = review_clock() as u64;
        for (number, key) in [(1, "before"), (2, "after")] {
            let stamped = observed(&repository, "alice", key, number, point(1, seconds));
            handle
                .observe_review_requests(ObservedRequests {
                    destination: stamped.destination,
                    requests: stamped
                        .requests
                        .into_iter()
                        .map(|entry| entry.request)
                        .collect(),
                })
                .unwrap();
            handle
                .observe_review_monitor(MonitorUpdate::Completed(Census {
                    repository: repository.clone(),
                    start: point(1, seconds),
                    viewer: "alice".into(),
                    current: BTreeMap::new(),
                }))
                .unwrap();
        }
        let mut pending = take_pending(&handle.0);
        apply_review_updates(
            &mut ReviewMonitor::default(),
            &mut bridge.journal,
            &mut Snapshot::default(),
            &mut Withdrawals::default(),
            &mut pending.review_updates,
            ReviewDelivery {
                pending: &mut BTreeMap::new(),
                schedule: &mut batching::Schedule::default(),
                presence: &BTreeMap::new(),
                policy: &Preferences::default(),
            },
        )
        .unwrap();
        let exact = bridge
            .journal
            .list()
            .unwrap()
            .into_iter()
            .map(|notice| (notice.event.evidence.key, notice.acknowledged))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            exact,
            BTreeMap::from([("after".into(), false), ("before".into(), true)])
        );
    }

    #[test]
    fn predecessor_read_keeps_withdrawals_when_new_attention_replaces_the_row() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let earlier_owner = TerminalId::new();
        let before = journal
            .record(attention_event(earlier_owner, "before"), false)
            .unwrap()
            .unwrap();
        let mut reads = [(before.id, earlier_owner)].into();
        apply_predecessor_reads(&mut journal, &mut reads, &mut BTreeMap::new(), |_, _| {}).unwrap();
        let replacement = journal
            .record(attention_event(earlier_owner, "later"), false)
            .unwrap()
            .unwrap();
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        assert!(journal.list().unwrap().contains(&replacement));
        let withdrawals = journal.group_withdrawals_for(&[]).unwrap();
        assert!(withdrawals.contains(&before.id));
        assert!(withdrawals.contains(&batching::desktop_id(&before.event.target())));
    }

    #[test]
    fn an_explicit_attention_read_sees_committed_notice_ids_before_publication() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let session = TerminalId::new();
        let mut published = Snapshot::default();
        let old = journal
            .record(attention_event(session, "old"), false)
            .unwrap()
            .unwrap();
        published.notices.push(old.clone());
        let current = journal
            .record(attention_event(session, "new"), false)
            .unwrap()
            .unwrap();

        combine_committed_notices(&mut published, Journal::committed_notices(&path).unwrap());

        assert_eq!(published.notices, [current]);
        assert!(published.notices.iter().all(|notice| notice.id != old.id));
    }

    #[test]
    fn queued_review_observation_is_cleared_by_the_same_batch_and_delayed_earlier_evidence_is_refused()
     {
        use crate::notifications::review::{Census, MonitorUpdate};
        use review_monitor::tests::{observed, point, repo};
        let directory = tempfile::tempdir().unwrap();
        let (handle, mut bridge) =
            JournalTestBridge::new(&directory.path().join("attention.sqlite")).unwrap();
        let repository = repo(None);
        let seconds = review_clock() as u64;
        handle
            .observe_review_monitor(MonitorUpdate::Observations(observed(
                &repository,
                "alice",
                "queued",
                1,
                point(1, seconds),
            )))
            .unwrap();
        handle
            .observe_review_monitor(MonitorUpdate::Completed(Census {
                repository: repository.clone(),
                start: point(1, seconds),
                viewer: "alice".into(),
                current: BTreeMap::new(),
            }))
            .unwrap();
        handle
            .observe_review_monitor(MonitorUpdate::Observations(observed(
                &repository,
                "alice",
                "delayed",
                2,
                point(1, seconds),
            )))
            .unwrap();
        handle
            .observe_review_monitor(MonitorUpdate::Observations(observed(
                &repository,
                "alice",
                "fresh",
                3,
                point(2, seconds),
            )))
            .unwrap();
        let mut pending = take_pending(&handle.0);
        let mut delivery = BTreeMap::new();
        apply_review_updates(
            &mut ReviewMonitor::default(),
            &mut bridge.journal,
            &mut Snapshot::default(),
            &mut Withdrawals::default(),
            &mut pending.review_updates,
            ReviewDelivery {
                pending: &mut delivery,
                schedule: &mut batching::Schedule::default(),
                presence: &BTreeMap::new(),
                policy: &Preferences::default(),
            },
        )
        .unwrap();
        let exact = bridge
            .journal
            .list()
            .unwrap()
            .into_iter()
            .map(|notice| (notice.event.evidence.key, notice.acknowledged))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            exact,
            BTreeMap::from([("fresh".into(), false), ("queued".into(), true)])
        );
        assert_eq!(
            delivery
                .into_values()
                .map(|notice| notice.event.evidence.key)
                .collect::<Vec<_>>(),
            ["fresh"]
        );
    }

    #[test]
    fn work_deferred_for_an_unavailable_journal_keeps_its_order_with_later_work() {
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let (retired, replaced, kept) = (TerminalId::new(), TerminalId::new(), TerminalId::new());
        let acknowledged = Uuid::new_v4();
        handle.record(attention_event(retired, "before-retirement"));
        handle.record(attention_event(replaced, "older"));
        handle.record(attention_event(kept, "kept"));
        handle.acknowledge(acknowledged);
        handle.presence(1, None);
        let receipt = handle
            .record_committed(attention_event(kept, "receipt"))
            .unwrap();
        let test = handle.test();
        let mut first = take_pending(&shared);
        // As the broker does while its journal is unavailable.
        for (_, receipt) in first.committed_events.drain(..) {
            receipt.finish(Err("journal unavailable".into()));
        }
        answer_tests(&first.tests, &TestOutcome::Unavailable("portal".into()));
        first.actions.push_back((Uuid::new_v4(), None));
        let deferred = first.deferred();
        assert_eq!(
            receipt.try_recv().unwrap(),
            Err("journal unavailable".into())
        );
        assert!(test.try_recv().is_ok());
        assert!(deferred.actions.is_empty() && deferred.tests.is_empty());

        handle.retire(retired);
        handle.record(attention_event(replaced, "newer"));
        let merged = deferred.followed_by(take_pending(&shared));
        // A deferred retirement still precedes a later worker's event.
        handle.record(attention_event(retired, "after-retirement"));
        let merged = merged.deferred().followed_by(take_pending(&shared));
        let keys = merged
            .events
            .values()
            .map(|event| event.evidence.key.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(keys, BTreeSet::from(["kept", "newer", "after-retirement"]));
        assert!(merged.retired.contains(&Target::Session(retired)));
        assert!(merged.acknowledged.contains(&acknowledged));
        assert!(merged.presence_changed);
    }

    #[test]
    fn helper_commit_queue_refuses_overflow_without_coalescing_accepted_events() {
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let event = attention_event(TerminalId::new(), "one-target");
        let receipts = (0..MAX_PENDING)
            .map(|_| handle.record_committed(event.clone()).unwrap())
            .collect::<Vec<_>>();
        assert!(handle.record_committed(event).unwrap_err().contains("full"));
        assert_eq!(
            shared.pending.lock().unwrap().committed_events.len(),
            MAX_PENDING
        );
        assert!(receipts.iter().all(|receipt| matches!(
            receipt.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        )));
        drop(take_pending(&shared));
        for receipt in receipts {
            assert!(
                receipt
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap()
                    .is_err()
            );
        }
    }

    #[test]
    fn an_automation_toast_is_withdrawn_again_after_acknowledgement_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("automation.sqlite3");
        let automation = Uuid::new_v4();
        let target = Target::Automation {
            id: automation,
            sequence: 4,
        };
        let resolved = |id, sequence| Ok(id == automation && sequence <= 4);
        let event = Event {
            target: target.clone(),
            source_session: None,
            provider: None,
            directory: None,
            destination: None,
            evidence: crate::notifications::Evidence {
                key: format!("automation:{automation}:lifecycle:4"),
                kind: crate::notifications::Kind::AutomationPaused,
            },
            content: None,
        };
        let mut journal = Journal::open(&path).unwrap();
        let notice = journal.record(event, false).unwrap().unwrap();
        journal.delivered(notice.id).unwrap();
        let mut snapshot = Snapshot {
            notices: journal.list().unwrap(),
            ..Snapshot::default()
        };
        let mut withdrawals = Withdrawals::default();
        reconcile_automation_attention(
            &mut journal,
            &mut snapshot,
            &mut withdrawals,
            false,
            resolved,
        )
        .unwrap();
        let desktop = batching::desktop_id(&target);
        assert!(withdrawals.contains(&desktop));

        // Lose the process-local queue after SQLite committed the acknowledgement.
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        snapshot.notices = journal.list().unwrap();
        assert!(snapshot.notices[0].acknowledged);
        let mut withdrawals = Withdrawals::default();
        reconcile_automation_attention(
            &mut journal,
            &mut snapshot,
            &mut withdrawals,
            false,
            resolved,
        )
        .unwrap();
        assert!(withdrawals.is_empty());
        reconcile_automation_attention(
            &mut journal,
            &mut snapshot,
            &mut withdrawals,
            true,
            resolved,
        )
        .unwrap();
        assert_eq!(withdrawals.pop_first().unwrap().id(), desktop);
        assert_eq!(withdrawals.pop_first().unwrap().id(), notice.id);
        assert!(withdrawals.is_empty());
    }

    #[test]
    fn failed_automation_acknowledgement_retries_even_with_no_portal_work() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite3");
        let mut journal = Journal::open(&path).unwrap();
        let id = Uuid::new_v4();
        let event = Event {
            target: Target::Automation { id, sequence: 1 },
            source_session: None,
            provider: None,
            directory: None,
            destination: None,
            evidence: crate::notifications::Evidence {
                key: format!("automation:{id}:lifecycle:1"),
                kind: crate::notifications::Kind::AutomationPaused,
            },
            content: None,
        };
        let notice = journal.record(event, false).unwrap().unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TRIGGER refuse_ack BEFORE UPDATE OF acknowledged ON notices BEGIN SELECT raise(ABORT, 'temporarily cannot write'); END;").unwrap();
        let mut snapshot = Snapshot {
            notices: vec![notice.clone()],
            ..Default::default()
        };
        let mut withdrawals = Withdrawals::default();
        let mut retry = AutomationRetry::default();
        let now = Instant::now();
        let mut delivery = [(notice.event.target(), notice.clone())].into();
        let result = discard_resolved_delivery(
            &mut journal,
            &mut snapshot,
            &mut withdrawals,
            &mut delivery,
            &notice,
        );
        assert!(
            delivery.is_empty(),
            "no obsolete due delivery may drive immediate retries"
        );
        assert!(result.is_err());
        retry.update(result, &mut snapshot, now);
        assert_eq!(retry.deadline(), Some(now + RETRY_DELAY));
        assert!(!snapshot.notices[0].acknowledged);
        assert!(withdrawals.is_empty());

        db.execute_batch("DROP TRIGGER refuse_ack").unwrap();
        let result = reconcile_automation_attention(
            &mut journal,
            &mut snapshot,
            &mut withdrawals,
            false,
            |_, _| Ok(true),
        );
        retry.update(result, &mut snapshot, now + RETRY_DELAY);
        assert_eq!(retry.deadline(), None);
        assert!(snapshot.notices[0].acknowledged);
        assert!(snapshot.diagnostic.is_none());
        assert!(!withdrawals.is_empty());
    }

    #[test]
    fn a_returning_automation_reserves_its_withdrawal_even_with_a_full_old_backlog() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&root.path().join("attention.sqlite3")).unwrap();
        let id = Uuid::new_v4();
        let target = Target::Automation { id, sequence: 6 };
        let event = Event {
            target: target.clone(),
            source_session: None,
            provider: None,
            directory: None,
            destination: None,
            evidence: crate::notifications::Evidence {
                key: format!("automation:{id}:lifecycle:6"),
                kind: crate::notifications::Kind::AutomationPaused,
            },
            content: None,
        };
        let current = journal.record(event, false).unwrap().unwrap();
        journal
            .remember_notice_withdrawal(&target, Uuid::new_v4())
            .unwrap();
        let old = (0..MAX_PENDING).map(|index| Uuid::from_u128(index as u128));
        let mut withdrawals = Withdrawals::default();
        withdrawals.protect(old.clone());
        for id in old {
            withdrawals.group(id);
        }
        let mut snapshot = Snapshot::default();
        restore(
            &mut journal,
            &Preferences::default(),
            &mut snapshot,
            &mut withdrawals,
        )
        .unwrap();
        assert_eq!(snapshot.notices, [current]);
        assert!(withdrawals.contains(&batching::desktop_id(&target)));
    }

    #[test]
    fn replacing_a_resolved_automation_notice_keeps_its_toast_withdrawal_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("automation.sqlite3");
        let id = Uuid::new_v4();
        let event = |sequence| Event {
            target: Target::Automation { id, sequence },
            source_session: None,
            provider: None,
            directory: None,
            destination: None,
            evidence: crate::notifications::Evidence {
                key: format!("automation:{id}:lifecycle:{sequence}"),
                kind: crate::notifications::Kind::AutomationPaused,
            },
            content: None,
        };
        let mut journal = Journal::open(&path).unwrap();
        let previous = journal.record(event(4), false).unwrap().unwrap();
        journal.delivered(previous.id).unwrap();
        let newer = event(6);
        let group = batching::desktop_id(&newer.target);
        protect_automation_toast(
            &mut journal,
            id,
            &newer.target,
            |sequence| Ok(sequence <= 5),
        )
        .unwrap();
        let replacement = journal.record(newer, false).unwrap().unwrap();
        assert!(
            journal
                .list()
                .unwrap()
                .iter()
                .all(|notice| notice.id != previous.id)
        );
        drop(journal);

        let mut journal = Journal::open(&path).unwrap();
        let mut snapshot = Snapshot::default();
        let mut withdrawals = Withdrawals::default();
        restore(
            &mut journal,
            &Preferences::default(),
            &mut snapshot,
            &mut withdrawals,
        )
        .unwrap();
        assert_eq!(snapshot.notices, [replacement]);
        assert!(action_notice(&snapshot, group).is_none());
        assert!(action_notice(&snapshot, previous.id).is_none());
        let mut removed = BTreeSet::new();
        while let Some(entry) = withdrawals.pop_first() {
            removed.insert(entry.id());
            journal.finish_group_withdrawal(entry.id()).unwrap();
        }
        assert_eq!(removed, [group, previous.id].into());
        assert!(journal.group_withdrawals().unwrap().is_empty());
    }

    #[test]
    fn helper_commit_retirement_fails_prior_evidence_but_preserves_a_later_worker() {
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let session = TerminalId::new();
        let old = handle
            .record_committed(attention_event(session, "old"))
            .unwrap();
        handle.retire(session);
        assert!(old.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
        let later = handle
            .record_committed(attention_event(session, "later"))
            .unwrap();
        let pending = take_pending(&shared);
        assert!(pending.retired.contains(&Target::Session(session)));
        assert_eq!(pending.committed_events.len(), 1);
        assert_eq!(pending.committed_events[0].0.evidence.key, "later");
        assert!(matches!(
            later.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        drop(pending);
        assert!(later.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
    }

    #[test]
    fn helper_commit_stop_remains_closed_during_final_drain_and_inflight_drop_fails() {
        let (shared, receiver) = shared();
        let handle = Handle(shared.clone());
        let event = attention_event(TerminalId::new(), "stopping");
        let waiting = handle.record_committed(event.clone()).unwrap();
        handle.change(|pending| pending.stopped = true);
        let pending = take_pending(&shared);
        assert!(pending.stopped);
        assert!(shared.pending.lock().unwrap().stopped);
        assert!(handle.record_committed(event.clone()).is_err());
        drop(pending);
        assert!(
            waiting
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_err()
        );
        shared.pending.lock().unwrap().stopped = false;
        drop(receiver);
        assert!(handle.record_committed(event).is_err());
        assert!(shared.pending.lock().unwrap().committed_events.is_empty());
    }

    #[test]
    fn suppressed_helper_commit_succeeds_and_acknowledged_replay_stays_dismissed() {
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let event = attention_event(TerminalId::new(), "suppressed-helper");
        let policy = Preferences::default();
        let waiting = handle.record_committed(event.clone()).unwrap();
        let (queued, receipt) = shared
            .pending
            .lock()
            .unwrap()
            .committed_events
            .pop_front()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("suppressed.sqlite3");
        let mut journal = Journal::open(&path).unwrap();
        let notice = record_attention_committed_policy(
            &mut journal,
            queued,
            true,
            Some(receipt),
            &policy,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            waiting.recv_timeout(Duration::from_secs(1)).unwrap(),
            Ok(())
        );
        assert!(notice.delivered);
        drop(journal);
        let mut reopened = Journal::open(&path).unwrap();
        assert_eq!(reopened.list().unwrap(), vec![notice.clone()]);
        reopened.acknowledge(notice.id).unwrap();

        let replay = handle.record_committed(event).unwrap();
        let (queued, receipt) = shared
            .pending
            .lock()
            .unwrap()
            .committed_events
            .pop_front()
            .unwrap();
        assert!(
            record_attention_committed_policy(
                &mut reopened,
                queued,
                false,
                Some(receipt),
                &policy,
                false,
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(replay.recv_timeout(Duration::from_secs(1)).unwrap(), Ok(()));
        drop(reopened);
        let mut final_journal = Journal::open(&path).unwrap();
        let notices = final_journal.list().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].id, notice.id);
        assert!(notices[0].acknowledged);
        assert!(notices[0].delivered);
    }

    #[test]
    fn private_default_strips_ordinary_and_receipted_actor_and_automation_events_before_commit() {
        for previews in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("privacy.sqlite3");
            let mut journal = Journal::open(&path).unwrap();
            let policy = Preferences {
                message_previews: previews,
                ..Default::default()
            };
            for path in ["ordinary", "retained", "automation"] {
                let mut event = Event::session(
                    TerminalId::new(),
                    crate::agent::Provider::Claude,
                    "/repo".into(),
                    crate::notifications::Evidence {
                        key: path.into(),
                        kind: crate::notifications::Kind::Succeeded,
                    },
                );
                if path == "automation" {
                    event.target = Target::AgentRun(crate::model::RunId::new());
                }
                event.content = Some(crate::notifications::Content {
                    conversation: "Private conversation".into(),
                    preview: "Private response".into(),
                    request: None,
                });
                let original = event.clone();
                let (receipt, result) = std::sync::mpsc::channel();
                let receipt = (path != "ordinary").then_some(CommitReceipt::new(receipt));
                let notice = record_attention_committed_policy(
                    &mut journal,
                    event,
                    false,
                    receipt,
                    &policy,
                    false,
                )
                .unwrap()
                .unwrap();
                if path != "ordinary" {
                    assert_eq!(result.recv().unwrap(), Ok(()));
                }
                assert_eq!(notice.event.evidence, original.evidence);
                assert_eq!(notice.event.target, original.target);
                assert_eq!(notice.event.content.is_some(), previews);
            }
            drop(journal);
            let mut reopened = Journal::open(&path).unwrap();
            let notices = reopened.list().unwrap();
            assert_eq!(notices.len(), 3);
            assert!(
                notices
                    .iter()
                    .all(|notice| notice.event.content.is_some() == previews)
            );
            if previews {
                let before = notices
                    .iter()
                    .map(|notice| (notice.id, notice.acknowledged))
                    .collect::<Vec<_>>();
                let mut snapshot = Snapshot {
                    notices,
                    diagnostic: None,
                };
                let mut withdrawals = Withdrawals::default();
                revoke_previews(&mut snapshot, &mut withdrawals);
                reopened.clear_content().unwrap();
                assert!(
                    snapshot
                        .notices
                        .iter()
                        .all(|notice| notice.event.content.is_none())
                );
                assert_eq!(
                    snapshot
                        .notices
                        .iter()
                        .map(|notice| (notice.id, notice.acknowledged))
                        .collect::<Vec<_>>(),
                    before
                );
                assert!(snapshot.notices.iter().all(|notice| {
                    withdrawals.contains(&batching::desktop_id(&notice.event.target))
                }));
                assert!(
                    reopened
                        .list()
                        .unwrap()
                        .iter()
                        .all(|notice| notice.event.content.is_none())
                );
            }
        }
    }

    #[test]
    fn helper_attention_receipt_waits_for_the_actual_durable_journal_commit() {
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let mut event = Event::session(
            TerminalId::new(),
            crate::agent::Provider::Claude,
            "/repo".into(),
            crate::notifications::Evidence {
                key: "stable-helper-turn".into(),
                kind: crate::notifications::Kind::Succeeded,
            },
        );
        event.content = Some(crate::notifications::Content {
            conversation: "Original turn".into(),
            preview: "Immutable response captured before daemon replacement".into(),
            request: None,
        });
        let receipt = handle.record_committed(event.clone()).unwrap();
        assert!(
            matches!(
                receipt.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "queueing alone never confirms delivery"
        );
        let (queued, reply) = shared
            .pending
            .lock()
            .unwrap()
            .committed_events
            .pop_front()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let mut journal = Journal::open(&path).unwrap();
        record_attention_committed(&mut journal, queued, false, Some(reply)).unwrap();
        assert_eq!(receipt.recv().unwrap(), Ok(()));
        drop(journal);
        let mut reopened = Journal::open(&path).unwrap();
        assert!(
            reopened
                .list()
                .unwrap()
                .iter()
                .any(|notice| notice.event == event)
        );
        let replay = handle.record_committed(event.clone()).unwrap();
        let (queued, reply) = shared
            .pending
            .lock()
            .unwrap()
            .committed_events
            .pop_front()
            .unwrap();
        record_attention_committed(&mut reopened, queued, false, Some(reply)).unwrap();
        assert_eq!(replay.recv().unwrap(), Ok(()));
        assert_eq!(
            reopened.list().unwrap().len(),
            1,
            "stable evidence replay is idempotent"
        );
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute("DROP TABLE seen", [])
            .unwrap();
        let mut next = event.clone();
        next.evidence.key = "journal-write-fails".into();
        let failed = handle.record_committed(next).unwrap();
        let (queued, reply) = shared
            .pending
            .lock()
            .unwrap()
            .committed_events
            .pop_front()
            .unwrap();
        assert!(record_attention_committed(&mut reopened, queued, false, Some(reply)).is_err());
        assert!(
            failed.recv().unwrap().is_err(),
            "a failed durable write cannot ACK helper delivery"
        );
        shared.pending.lock().unwrap().stopped = true;
        assert!(handle.record_committed(event).is_err());
    }

    #[test]
    fn inbox_reconciliation_coalesces_independently_from_lifecycle_attention() {
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let session = TerminalId::new();
        let lineage = crate::agent_execution::Lineage(session);
        let inbox = Event {
            content: None,
            target: Target::SessionInbox(lineage),
            source_session: Some(session),
            provider: None,
            directory: None,
            destination: None,
            evidence: crate::notifications::Evidence {
                key: crate::model::MessageId::new().to_string(),
                kind: crate::notifications::Kind::MessageReceived,
            },
        };
        let lifecycle = Event::session(
            session,
            crate::agent::Provider::Claude,
            "/repo".into(),
            crate::notifications::Evidence {
                key: "finished".into(),
                kind: crate::notifications::Kind::Succeeded,
            },
        );
        handle.record(lifecycle.clone());
        handle.reconcile_inboxes(BTreeSet::from([lineage]), vec![inbox.clone()]);
        handle.reconcile_inboxes(BTreeSet::from([lineage]), vec![]);
        let pending = shared.pending.lock().unwrap();
        assert_eq!(pending.events[&Target::Session(session)], lifecycle);
        assert_eq!(
            pending.inboxes.as_ref().unwrap().pending,
            BTreeSet::from([lineage])
        );
        assert!(pending.inboxes.as_ref().unwrap().events.is_empty());
        drop(pending);
        // A newer batch wins, including a fully drained authoritative inbox.
        handle.reconcile_inboxes(BTreeSet::new(), vec![]);
        assert!(
            shared
                .pending
                .lock()
                .unwrap()
                .inboxes
                .as_ref()
                .unwrap()
                .pending
                .is_empty()
        );
    }

    #[test]
    fn notification_panic_publishes_failure_preserves_attention_and_releases_requesters() {
        let (wake, _receiver) = mpsc::channel(1);
        let handle = Handle(Arc::new(Shared {
            pending: Mutex::default(),
            review_slots: Arc::new(Semaphore::new(MAX_REVIEW_UPDATES)),
            retained_turn_slots: Arc::new(Semaphore::new(MAX_PENDING)),
            presence: Mutex::default(),
            snapshot: Mutex::default(),
            wake,
        }));
        let host = crate::host::service::test_host();
        let notice = Notice {
            id: Uuid::new_v4(),
            event: Event {
                content: None,
                target: Target::Session(TerminalId::new()),
                source_session: None,
                provider: None,
                directory: None,
                destination: None,
                evidence: crate::notifications::Evidence {
                    key: "collected-before-panic".into(),
                    kind: crate::notifications::Kind::Succeeded,
                },
            },
            delivered: false,
            acknowledged: false,
            window: None,
        };
        handle
            .0
            .snapshot
            .lock()
            .unwrap()
            .notices
            .push(notice.clone());
        let pending_test = handle.test();
        let pending_commit = handle.record_committed(notice.event.clone()).unwrap();
        let shared = Arc::downgrade(&handle.0);
        let thread = std::thread::spawn(move || {
            supervise(&shared, &Arc::downgrade(&host), || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let _guard = host.lock().unwrap();
                        let _reentrant = host.lock().unwrap();
                        unreachable!();
                    })
            });
        });
        thread.join().unwrap();
        assert!(pending_commit.recv().unwrap().is_err());
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.notices, vec![notice]);
        assert!(
            snapshot
                .diagnostic
                .unwrap()
                .starts_with("Notifications unavailable:")
        );
        assert!(matches!(
            pending_test.recv_timeout(Duration::from_secs(1)).unwrap(),
            TestOutcome::Unavailable(_)
        ));
        assert!(matches!(
            handle.test().recv_timeout(Duration::from_secs(1)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn waiting_resolution_keeps_a_newer_open_request() {
        let session = TerminalId::new();
        let target = Target::Session(session);
        let request = |key: &str| {
            Event::session(
                session,
                crate::agent::Provider::Claude,
                "/work".into(),
                crate::notifications::Evidence {
                    key: key.into(),
                    kind: Kind::Waiting,
                },
            )
        };
        let pending = |event: Event| Pending {
            events: [(target.clone(), event)].into(),
            ..Pending::default()
        };

        let mut settled = Pending::default();
        settled.clear_waiting(target.clone());
        let combined = pending(request("old")).followed_by(settled);
        assert!(combined.events.is_empty());
        assert!(combined.cleared_waiting.contains(&target));

        let combined = combined.followed_by(pending(request("new")));
        assert!(combined.cleared_waiting.contains(&target));
        assert_eq!(combined.events.get(&target), Some(&request("new")));

        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let older = handle.record_committed(request("old")).unwrap();
        handle.clear_session_waiting(session);
        let newer = handle.record_committed(request("new")).unwrap();
        let mut pending = take_pending(&shared);
        let old = pending.committed_events.pop_front().unwrap();
        let new = pending.committed_events.pop_front().unwrap();
        assert!(old.1.resolved);
        assert!(!new.1.resolved);

        // The resolved older event can commit after the newer request is
        // available, without replacing it or being advertised as unread.
        let root = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&root.path().join("attention.sqlite")).unwrap();
        for (event, receipt) in [new, old] {
            let resolved = receipt.resolved;
            record_attention_committed_policy(
                &mut journal,
                event,
                false,
                Some(receipt),
                &Preferences::default(),
                resolved,
            )
            .unwrap();
        }
        assert_eq!(older.recv().unwrap(), Ok(()));
        assert_eq!(newer.recv().unwrap(), Ok(()));
        let retained = journal.list().unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].event.evidence.key, "new");
        assert!(!retained[0].acknowledged);
    }

    #[test]
    fn shutdown_commits_waiting_clears_even_after_taking_the_broker_batch() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&root.path().join("attention.sqlite")).unwrap();
        let (first, second) = (TerminalId::new(), TerminalId::new());
        for session in [first, second] {
            let mut event = attention_event(session, "question");
            event.evidence.kind = Kind::Waiting;
            journal.record(event, false).unwrap().unwrap();
        }
        let (wake, _receiver) = mpsc::channel(1);
        let shared = Arc::new(Shared {
            pending: Mutex::default(),
            review_slots: Arc::new(Semaphore::new(MAX_REVIEW_UPDATES)),
            retained_turn_slots: Arc::new(Semaphore::new(MAX_PENDING)),
            presence: Mutex::default(),
            snapshot: Mutex::default(),
            wake,
        });
        let handle = Handle(shared.clone());
        handle.clear_session_waiting(first);
        let mut batch = take_pending(&shared);
        handle.clear_session_waiting(second);
        handle.change(|pending| pending.stopped = true);

        assert!(settle_attention_at_shutdown(
            &Arc::downgrade(&shared),
            &mut journal,
            &mut batch,
        ));
        let notices = journal.list().unwrap();
        assert_eq!(notices.len(), 2);
        assert!(notices.iter().all(|notice| notice.acknowledged));
    }

    #[test]
    fn journal_retry_preserves_direct_and_retained_waiting_settlements_across_batches() {
        let root = tempfile::tempdir().unwrap();
        let (handle, mut bridge) =
            JournalTestBridge::new(&root.path().join("attention.sqlite")).unwrap();
        let waiting = |session, key| {
            let mut event = attention_event(session, key);
            event.evidence.kind = Kind::Waiting;
            event
        };
        let direct = TerminalId::new();
        let retained = waiting(TerminalId::new(), "retained");
        let before_direct = bridge
            .journal
            .record(waiting(direct, "direct"), false)
            .unwrap()
            .unwrap();
        let before_retained = bridge
            .journal
            .record(retained.clone(), false)
            .unwrap()
            .unwrap();

        let deferred = Pending {
            cleared_waiting: (0..MAX_PENDING)
                .map(|_| Target::Session(TerminalId::new()))
                .collect(),
            ..Pending::default()
        };
        handle.clear_session_waiting(direct);
        let receipt = handle
            .record_retained(retained, Uuid::new_v4(), 1, true)
            .unwrap();
        let merged = deferred.followed_by(take_pending(&handle.0));
        assert_eq!(merged.cleared_waiting.len(), MAX_PENDING + 2);
        assert_eq!(
            receipt.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );
        *handle.0.pending.lock().unwrap() = merged;

        bridge.commit_next(&Preferences::default(), false).unwrap();
        receipt.try_recv().unwrap().unwrap();
        for prior in [before_direct, before_retained] {
            assert!(
                bridge
                    .list()
                    .unwrap()
                    .iter()
                    .any(|notice| notice.id == prior.id && notice.acknowledged)
            );
        }
    }

    #[test]
    fn a_wait_settled_during_delivery_cannot_be_announced_after_the_portal_await() {
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let session = TerminalId::new();
        let mut waiting = attention_event(session, "pending");
        waiting.evidence.kind = Kind::Waiting;
        let reference = Arc::downgrade(&shared);
        assert!(!session_settlement_admitted(&reference, &waiting));

        handle.clear_session_waiting(session);
        assert!(session_settlement_admitted(&reference, &waiting));
        assert!(!session_settlement_admitted(
            &reference,
            &attention_event(session, "done")
        ));
    }

    #[test]
    fn a_failed_waiting_clear_is_retried_before_a_later_request_commits() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let session = TerminalId::new();
        let target = Target::Session(session);
        let request = |key: &str| {
            let mut event = attention_event(session, key);
            event.evidence.kind = Kind::Waiting;
            event
        };
        let mut store = journal::Reopening::open(Some(path.clone()), Instant::now());
        let journal = store.journal().unwrap();
        let old = journal.record(request("old"), false).unwrap().unwrap();
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch(
            "CREATE TRIGGER fail_ack BEFORE UPDATE ON notices
             WHEN NEW.acknowledged=1 BEGIN SELECT RAISE(FAIL, 'fixture write failure'); END;",
        )
        .unwrap();
        let mut pending = Pending::default();
        pending.clear_waiting(target.clone());
        let failure =
            commit_waiting_clears(journal, &pending.cleared_waiting, |_, _| {}).unwrap_err();
        let next = store.retry_failed_write(failure, Instant::now());
        let mut newer = Pending::default();
        newer.events.insert(target.clone(), request("new"));
        let deferred = pending.deferred().followed_by(newer);

        db.execute_batch("DROP TRIGGER fail_ack").unwrap();
        assert!(store.reopen_if_due(next).is_some());
        let journal = store.journal().unwrap();
        let mut cleared = Vec::new();
        commit_waiting_clears(journal, &deferred.cleared_waiting, |_, id| cleared.push(id))
            .unwrap();
        assert_eq!(cleared, [old.id]);
        let new = journal
            .record(deferred.events[&target].clone(), false)
            .unwrap()
            .unwrap();
        assert_eq!(journal.list().unwrap(), [new]);
    }

    #[test]
    fn a_current_waiting_group_is_reserved_ahead_of_the_durable_backlog() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite3");
        let mut journal = Journal::open(&path).unwrap();
        let target = Target::Session(TerminalId::new());
        let notice = journal
            .record(
                Event {
                    target: target.clone(),
                    source_session: None,
                    provider: None,
                    directory: None,
                    destination: None,
                    evidence: crate::notifications::Evidence {
                        key: "current waiting".into(),
                        kind: Kind::Waiting,
                    },
                    content: None,
                },
                false,
            )
            .unwrap()
            .unwrap();
        rusqlite::Connection::open(path).unwrap().execute(
            "WITH RECURSIVE rows(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM rows WHERE n<?1) INSERT INTO portal_group_withdrawals SELECT printf('00000000-0000-0000-0000-%012x', n) FROM rows",
            [MAX_PENDING],
        ).unwrap();
        let current = batching::desktop_id(&target);
        journal
            .remember_notice_withdrawal(&target, Uuid::new_v4())
            .unwrap();
        let mut withdrawals = Withdrawals::default();
        let mut snapshot = Snapshot::default();
        restore(
            &mut journal,
            &Preferences::default(),
            &mut snapshot,
            &mut withdrawals,
        )
        .unwrap();
        assert_eq!(snapshot.notices, [notice]);
        assert!(withdrawals.contains(&current));
    }

    #[test]
    fn shutdown_reopens_a_recovered_journal_before_the_waiting_retry_deadline() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let session = TerminalId::new();
        let mut store = journal::Reopening::open(Some(path), Instant::now());
        let mut event = attention_event(session, "question");
        event.evidence.kind = Kind::Waiting;
        store.journal().unwrap().record(event, false).unwrap();
        let failed = Instant::now();
        store.retry_failed_write("temporary fixture failure".into(), failed);
        assert!(store.reopen_if_due(failed).is_none());
        let journal = store.journal_for_shutdown().unwrap();
        commit_waiting_clears(journal, &[Target::Session(session)].into(), |_, _| {}).unwrap();
        assert!(journal.list().unwrap()[0].acknowledged);
    }

    #[test]
    fn a_resolved_waiting_toast_is_withdrawn_after_a_new_request_overwrites_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let session = TerminalId::new();
        let request = |key: &str| {
            let mut event = attention_event(session, key);
            event.evidence.kind = Kind::Waiting;
            event
        };
        let prior = journal.record(request("old"), false).unwrap().unwrap();
        journal.delivered(prior.id).unwrap();
        let target = Target::Session(session);
        assert_eq!(
            journal.acknowledge_waiting(&target).unwrap(),
            Some(prior.id)
        );
        let newer = journal.record(request("new"), false).unwrap().unwrap();
        drop(journal);

        let mut journal = Journal::open(&path).unwrap();
        let mut snapshot = Snapshot::default();
        let mut withdrawals = Withdrawals::default();
        restore(
            &mut journal,
            &Preferences::default(),
            &mut snapshot,
            &mut withdrawals,
        )
        .unwrap();
        assert_eq!(snapshot.notices, [newer]);
        let group = batching::desktop_id(&target);
        let mut expected = [group, prior.id];
        expected.sort();
        for id in expected {
            assert_eq!(withdrawals.pop_first().unwrap().id(), id);
            journal.finish_group_withdrawal(id).unwrap();
        }
        assert!(withdrawals.pop_first().is_none());
        assert!(journal.group_withdrawals().unwrap().is_empty());
    }

    #[test]
    fn retirement_coalescing_preserves_only_later_worker_evidence() {
        let (wake, _receiver) = mpsc::channel(1);
        let handle = Handle(Arc::new(Shared {
            pending: Mutex::default(),
            review_slots: Arc::new(Semaphore::new(MAX_REVIEW_UPDATES)),
            retained_turn_slots: Arc::new(Semaphore::new(MAX_PENDING)),
            presence: Mutex::default(),
            snapshot: Mutex::default(),
            wake,
        }));
        let session = TerminalId::new();
        let event = |key: &str| Event {
            content: None,
            target: crate::notifications::Target::Session(session),
            source_session: Some(session),
            provider: Some(crate::agent::Provider::Codex),
            directory: Some("/fixture".into()),
            destination: None,
            evidence: crate::notifications::Evidence {
                key: key.into(),
                kind: crate::notifications::Kind::Succeeded,
            },
        };
        handle.record(event("old"));
        handle.retire(session);
        assert!(handle.0.pending.lock().unwrap().events.is_empty());
        handle.record(event("new"));
        let pending = handle.0.pending.lock().unwrap();
        assert!(pending.retired.contains(&Target::Session(session)));
        assert_eq!(
            pending.events[&Target::Session(session)].evidence.key,
            "new"
        );
    }

    #[test]
    fn every_test_requester_is_answered_or_released() {
        let (wake, _receiver) = mpsc::channel(1);
        let handle = Handle(Arc::new(Shared {
            pending: Mutex::default(),
            review_slots: Arc::new(Semaphore::new(MAX_REVIEW_UPDATES)),
            retained_turn_slots: Arc::new(Semaphore::new(MAX_PENDING)),
            presence: Mutex::default(),
            snapshot: Mutex::default(),
            wake,
        }));
        let first = handle.test();
        let second = handle.test();
        let pending = std::mem::take(&mut *handle.0.pending.lock().unwrap());
        answer_tests(&pending.tests, &TestOutcome::Sent);
        assert_eq!(first.recv(), Ok(TestOutcome::Sent));
        assert_eq!(second.recv(), Ok(TestOutcome::Sent));

        // A broker that stops with a test still queued releases its waiter
        // instead of leaving the request to time out.
        let orphaned = handle.test();
        drop(std::mem::take(&mut *handle.0.pending.lock().unwrap()));
        assert!(orphaned.recv().is_err());
    }

    #[test]
    fn portal_retry_wakes_without_another_ui_or_agent_event() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let (_sender, mut receiver) = mpsc::channel(1);
                assert!(
                    tokio::time::timeout(
                        Duration::from_secs(1),
                        wait_for_work(&mut receiver, Some(Instant::now()))
                    )
                    .await
                    .unwrap()
                );
                // The deadline, not a synthetic producer message, woke the broker.
                assert!(receiver.try_recv().is_err());
            });
    }

    fn shared() -> (Arc<Shared>, mpsc::Receiver<()>) {
        let (wake, receiver) = mpsc::channel(1);
        let shared = Arc::new(Shared {
            pending: Mutex::default(),
            review_slots: Arc::new(Semaphore::new(MAX_REVIEW_UPDATES)),
            retained_turn_slots: Arc::new(Semaphore::new(MAX_PENDING)),
            presence: Mutex::default(),
            snapshot: Mutex::default(),
            wake,
        });
        (shared, receiver)
    }

    #[test]
    fn a_failed_turn_boundary_retries_before_admitting_its_later_outcome() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let now = Instant::now();
        let mut store = journal::Reopening::open(Some(path.clone()), now);
        let session = TerminalId::new();
        let prior = store
            .journal()
            .unwrap()
            .record(attention_event(session, "earlier"), false)
            .unwrap()
            .unwrap();
        let control = rusqlite::Connection::open(&path).unwrap();
        control.execute_batch("CREATE TRIGGER refuse_turn_clear BEFORE UPDATE OF acknowledged ON notices BEGIN SELECT RAISE(FAIL, 'write refused'); END;").unwrap();
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        handle.session_started_turn(session, BTreeSet::new());
        handle.record(attention_event(session, "later"));
        let mut pending = take_pending(&shared);
        let error = pending
            .flush_started_turns(store.journal().unwrap())
            .unwrap_err();
        assert!(error.contains("write refused"));
        store.suspend(error, now);
        let deferred = pending.deferred();
        assert_eq!(store.reopen_if_due(now), None);
        assert_eq!(deferred.started_turns, [session].into());
        assert_eq!(deferred.events.len(), 1);
        control
            .execute_batch("DROP TRIGGER refuse_turn_clear;")
            .unwrap();
        assert!(store.reopen_if_due(store.retry_at().unwrap()).is_some());

        let mut pending = deferred.followed_by(take_pending(&shared));
        let journal = store.journal().unwrap();
        assert_eq!(
            pending.flush_started_turns(journal).unwrap(),
            [(session.to_string(), prior.id.to_string())]
        );
        assert!(pending.started_turns.is_empty());
        let outcome = pending.events.remove(&Target::Session(session)).unwrap();
        let current = journal.record(outcome, false).unwrap().unwrap();
        assert_eq!(journal.list().unwrap(), [current]);
    }

    #[test]
    fn confirmed_turn_withdrawal_survives_replacement_before_portal_delivery() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let session = TerminalId::new();
        let old = journal
            .record(attention_event(session, "old"), false)
            .unwrap()
            .unwrap();
        journal.acknowledge_sessions(&[session].into()).unwrap();
        let later = journal
            .record(attention_event(session, "later"), false)
            .unwrap()
            .unwrap();
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        assert!(journal.list().unwrap().contains(&later));
        let withdrawals = journal.group_withdrawals_for(&[]).unwrap();
        assert!(withdrawals.contains(&old.id));
        assert!(withdrawals.contains(&batching::desktop_id(&old.event.target())));
    }

    #[test]
    fn a_turn_started_during_delivery_suppresses_only_its_earlier_notice() {
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let source = TerminalId::new();
        let predecessor = TerminalId::new();
        let earlier = attention_event(predecessor, "previous");
        let unrelated = attention_event(TerminalId::new(), "unrelated");
        let reference = Arc::downgrade(&shared);
        assert!(!session_settlement_admitted(&reference, &earlier));

        handle.session_started_turn(source, [predecessor].into());
        assert!(session_settlement_admitted(&reference, &earlier));
        assert!(!session_settlement_admitted(&reference, &unrelated));
    }

    #[test]
    fn retained_turn_before_the_portal_send_defers_only_its_lineage() {
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let session = TerminalId::new();
        let predecessor = TerminalId::new();
        let unrelated = attention_event(TerminalId::new(), "unrelated");
        let reference = Arc::downgrade(&shared);
        let prior = attention_event(predecessor, "earlier");
        let previous = attention_event(session, "previous");
        assert!(!retained_turn_pending(&reference, &prior));

        let receipt = handle
            .commit_retained_turn(session, Uuid::new_v4(), 1, 2, [predecessor].into())
            .unwrap();
        assert!(retained_turn_pending(&reference, &prior));
        assert!(retained_turn_pending(&reference, &previous));
        assert!(!retained_turn_pending(&reference, &unrelated));
        assert!(!session_settlement_admitted(&reference, &prior));
        let batch = take_pending(&shared);
        assert_eq!(batch.retained_turns.len(), 1);
        assert!(!retained_turn_pending(&reference, &prior));
        assert_eq!(
            receipt.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );
    }

    #[test]
    fn retained_turn_keeps_its_delivery_fence_across_failed_and_suspended_writes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let now = Instant::now();
        let mut store = journal::Reopening::open(Some(path.clone()), now);
        let session = TerminalId::new();
        let predecessor = TerminalId::new();
        let prior = store
            .journal()
            .unwrap()
            .record(attention_event(predecessor, "old"), false)
            .unwrap()
            .unwrap();
        let unrelated = store
            .journal()
            .unwrap()
            .record(attention_event(TerminalId::new(), "unrelated"), false)
            .unwrap()
            .unwrap();
        let control = rusqlite::Connection::open(&path).unwrap();
        control.execute_batch("CREATE TRIGGER refuse_retained_turn BEFORE INSERT ON retained_attention_turns BEGIN SELECT RAISE(FAIL, 'write refused'); END;").unwrap();
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let actor = Uuid::new_v4();
        let receipt = handle
            .commit_retained_turn(session, actor, 1, 2, [predecessor].into())
            .unwrap();

        let mut pending = take_pending(&shared);
        let error = pending
            .flush_retained_turns(store.journal().unwrap(), false, |_| panic!("uncommitted"))
            .unwrap_err();
        assert!(error.contains("write refused"));
        store.retry_failed_write(error, now);
        let mut pending = pending.deferred().followed_by(take_pending(&shared));
        assert!(store.journal().is_err());
        assert_eq!(
            shared.retained_turn_slots.available_permits(),
            MAX_PENDING - 1
        );
        assert_eq!(
            receipt.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );

        let mut delivery = BTreeMap::from([
            (prior.event.target(), prior.clone()),
            (unrelated.event.target(), unrelated.clone()),
        ]);
        let mut withdrawals = Withdrawals::default();
        control
            .execute_batch("DROP TRIGGER refuse_retained_turn;")
            .unwrap();
        assert!(store.reopen_if_due(store.retry_at().unwrap()).is_some());
        let journal = store.journal().unwrap();
        pending
            .flush_retained_turns(journal, false, |rows| {
                withdraw_sessions(&mut withdrawals, &mut delivery, rows);
            })
            .unwrap();
        assert_eq!(receipt.try_recv(), Ok(Ok(())));
        assert_eq!(shared.retained_turn_slots.available_permits(), MAX_PENDING);
        assert_eq!(
            delivery,
            BTreeMap::from([(unrelated.event.target(), unrelated.clone())])
        );
        let notices = journal.list().unwrap();
        assert!(
            notices
                .iter()
                .any(|notice| notice.id == prior.id && notice.acknowledged)
        );
        assert!(notices.contains(&unrelated));

        let newer = journal
            .record(attention_event(predecessor, "new"), false)
            .unwrap()
            .unwrap();
        let retry = handle
            .commit_retained_turn(session, actor, 1, 2, [predecessor].into())
            .unwrap();
        take_pending(&shared)
            .flush_retained_turns(journal, false, |_| {
                panic!("duplicate turn withdrew a newer notice")
            })
            .unwrap();
        assert_eq!(retry.try_recv(), Ok(Ok(())));
        assert!(journal.list().unwrap().contains(&newer));
    }

    #[test]
    fn a_detached_retained_turn_settles_during_ordinary_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&root.path().join("attention.sqlite")).unwrap();
        let session = TerminalId::new();
        let prior = journal
            .record(attention_event(session, "old"), false)
            .unwrap()
            .unwrap();
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        drop(
            handle
                .commit_retained_turn(session, Uuid::new_v4(), 1, 2, BTreeSet::new())
                .unwrap(),
        );
        let mut pending = take_pending(&shared).deferred();
        handle.change(|pending| pending.stopped = true);
        assert!(settle_attention_at_shutdown(
            &Arc::downgrade(&shared),
            &mut journal,
            &mut pending,
        ));
        let notices = journal.list().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].id, prior.id);
        assert!(notices[0].acknowledged);
        assert_eq!(shared.retained_turn_slots.available_permits(), MAX_PENDING);
    }

    #[test]
    fn retirement_settles_older_retained_markers_before_releasing_their_replay_fence() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&root.path().join("attention.sqlite")).unwrap();
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let session = TerminalId::new();
        let predecessor = TerminalId::new();
        let old_actor = Uuid::new_v4();
        journal
            .acknowledge_retained_turn(session, old_actor, 1, 2, &[predecessor].into())
            .unwrap();
        let latest = journal
            .record(attention_event(predecessor, "later"), false)
            .unwrap()
            .unwrap();

        let queued = handle
            .commit_retained_turn(session, old_actor, 1, 2, [predecessor].into())
            .unwrap();
        let deferred = take_pending(&shared).deferred();
        let shared_duplicate = handle
            .commit_retained_turn(session, old_actor, 1, 2, [predecessor].into())
            .unwrap();
        // A first genuine start from another retiring actor also has to
        // commit even though the original actor no longer owns a pane.
        let another_session = TerminalId::new();
        let another_predecessor = TerminalId::new();
        let previous = journal
            .record(attention_event(another_predecessor, "previous"), false)
            .unwrap()
            .unwrap();
        let first_turn = handle
            .commit_retained_turn(
                another_session,
                Uuid::new_v4(),
                1,
                2,
                [another_predecessor].into(),
            )
            .unwrap();
        handle.retire(another_session);
        handle.retire(session);
        let replacement = handle
            .commit_retained_turn(session, Uuid::new_v4(), 1, 2, BTreeSet::new())
            .unwrap();

        let mut pending = deferred.followed_by(take_pending(&shared));
        pending
            .flush_retained_turns(&mut journal, true, |_| {})
            .unwrap();
        for finished in [queued, shared_duplicate, first_turn] {
            assert_eq!(finished.try_recv(), Ok(Ok(())));
        }
        assert_eq!(
            replacement.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );
        for target in std::mem::take(&mut pending.retired) {
            journal.retire(target).unwrap();
        }
        pending
            .flush_retained_turns(&mut journal, false, |_| {})
            .unwrap();
        assert_eq!(replacement.try_recv(), Ok(Ok(())));
        let rows = journal.list().unwrap();
        assert!(rows.contains(&latest));
        assert!(
            rows.iter()
                .any(|notice| notice.id == previous.id && notice.acknowledged)
        );
    }

    #[test]
    fn shutdown_finishes_every_retirement_still_in_the_broker_batch() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&root.path().join("attention.sqlite")).unwrap();
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let notices: Vec<_> = [TerminalId::new(), TerminalId::new(), TerminalId::new()]
            .into_iter()
            .map(|session| {
                let notice = journal
                    .record(attention_event(session, "prior"), false)
                    .unwrap()
                    .unwrap();
                handle.retire(session);
                notice
            })
            .collect();
        let mut pending = take_pending(&shared);
        assert!(
            pending
                .commit_next_retirement(&mut journal)
                .unwrap()
                .is_some()
        );
        assert_eq!(pending.retired.len(), 2);
        handle.change(|pending| pending.stopped = true);
        assert!(settle_attention_at_shutdown(
            &Arc::downgrade(&shared),
            &mut journal,
            &mut pending,
        ));
        assert!(pending.retired.is_empty());
        assert!(journal.list().unwrap().is_empty());
        let withdrawals = journal.group_withdrawals_for(&[]).unwrap();
        for notice in notices {
            assert!(withdrawals.contains(&notice.id));
            assert!(withdrawals.contains(&batching::desktop_id(&notice.event.target())));
        }
    }

    #[test]
    fn a_direct_outcome_retries_when_its_notice_insert_fails_after_turn_settlement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let session = TerminalId::new();
        let earlier = journal
            .record(attention_event(session, "old"), false)
            .unwrap()
            .unwrap();
        let mut pending = Pending::default();
        pending.start_turns([session].into());
        pending.flush_started_turns(&mut journal).unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TRIGGER refuse_new_notice BEFORE INSERT ON notices
            BEGIN SELECT raise(ABORT, 'transient fixture error'); END;",
        )
        .unwrap();

        assert!(
            record_session_outcome(
                &mut journal,
                &mut pending,
                attention_event(session, "new"),
                false,
                &Preferences::default(),
            )
            .is_err()
        );
        assert!(
            journal
                .list()
                .unwrap()
                .iter()
                .any(|notice| notice.id == earlier.id && notice.acknowledged)
        );
        db.execute_batch("DROP TRIGGER refuse_new_notice").unwrap();
        let mut pending = pending.deferred().followed_by(Pending::default());
        let event = pending.events.remove(&Target::Session(session)).unwrap();
        let outcome = record_session_outcome(
            &mut journal,
            &mut pending,
            event,
            false,
            &Preferences::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(outcome.event.evidence.key, "new");
        assert!(!outcome.acknowledged);
        assert!(pending.events.is_empty());
    }

    #[test]
    fn a_confirmed_turn_survives_a_full_deferred_batch_without_hiding_its_outcome() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&root.path().join("attention.sqlite")).unwrap();
        let session = TerminalId::new();
        let previous = journal
            .record(attention_event(session, "previous"), false)
            .unwrap()
            .unwrap();
        let deferred = Pending {
            started_turns: (0..MAX_PENDING).map(|_| TerminalId::new()).collect(),
            events: (0..MAX_PENDING)
                .map(|_| attention_event(TerminalId::new(), "unrelated"))
                .map(|event| (event.target(), event))
                .collect(),
            ..Pending::default()
        };
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        handle.session_started_turn(session, BTreeSet::new());
        handle.record(attention_event(session, "new"));
        let mut merged = deferred.followed_by(take_pending(&shared));

        merged.flush_started_turns(&mut journal).unwrap();
        assert!(
            journal
                .list()
                .unwrap()
                .iter()
                .any(|notice| { notice.id == previous.id && notice.acknowledged })
        );
        let new = merged.events.remove(&Target::Session(session)).unwrap();
        let next = journal.record(new, false).unwrap().unwrap();
        assert!(!next.acknowledged);
    }

    #[test]
    fn shutdown_flushes_local_and_queued_confirmed_turns() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let first = TerminalId::new();
        let second = TerminalId::new();
        journal
            .record(attention_event(first, "old one"), false)
            .unwrap();
        journal
            .record(attention_event(second, "old two"), false)
            .unwrap();
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        handle.session_started_turn(first, BTreeSet::new());
        let mut in_flight = take_pending(&shared);
        handle.session_started_turn(second, BTreeSet::new());
        shared.pending.lock().unwrap().stopped = true;
        assert!(settle_attention_at_shutdown(
            &Arc::downgrade(&shared),
            &mut journal,
            &mut in_flight,
        ));
        assert!(
            journal
                .list()
                .unwrap()
                .iter()
                .all(|notice| notice.acknowledged)
        );
    }

    #[test]
    fn shutdown_records_the_outcome_that_follows_an_accepted_turn() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let session = TerminalId::new();
        journal
            .record(attention_event(session, "old"), false)
            .unwrap();
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        handle.record(attention_event(session, "earlier queued"));
        let mut in_flight = take_pending(&shared);
        handle.session_started_turn(session, BTreeSet::new());
        handle.record(attention_event(session, "new"));
        shared.pending.lock().unwrap().stopped = true;

        assert!(settle_attention_at_shutdown(
            &Arc::downgrade(&shared),
            &mut journal,
            &mut in_flight
        ));
        drop(journal);
        let notices = Journal::open(&path).unwrap().list().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].event.evidence.key, "new");
        assert!(!notices[0].acknowledged);
    }

    #[test]
    fn shutdown_replacement_keeps_the_preexisting_toast_withdrawal() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("attention.sqlite");
        let mut journal = Journal::open(&path).unwrap();
        let session = TerminalId::new();
        let previous = journal
            .record(attention_event(session, "old delivered"), false)
            .unwrap()
            .unwrap();
        journal.delivered(previous.id).unwrap();

        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        handle.record(attention_event(session, "new"));
        shared.pending.lock().unwrap().stopped = true;
        assert!(settle_attention_at_shutdown(
            &Arc::downgrade(&shared),
            &mut journal,
            &mut Pending::default(),
        ));
        drop(journal);
        let mut reopened = Journal::open(&path).unwrap();
        let withdrawals = reopened.group_withdrawals_for(&[]).unwrap();
        assert!(withdrawals.contains(&previous.id));
        assert!(withdrawals.contains(&batching::desktop_id(&previous.event.target())));
        let notices = reopened.list().unwrap();
        assert_eq!(notices[0].event.evidence.key, "new");
        assert!(!notices[0].acknowledged);
    }

    #[test]
    fn a_started_turn_discards_earlier_pending_attention_but_keeps_its_new_outcome() {
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let (session, predecessor, unrelated) =
            (TerminalId::new(), TerminalId::new(), TerminalId::new());
        handle.record(attention_event(session, "earlier"));
        handle.record(attention_event(predecessor, "prior-owner"));
        handle.record(attention_event(unrelated, "unrelated"));
        let receipt = handle
            .record_retained(
                attention_event(predecessor, "prior-helper"),
                Uuid::new_v4(),
                4,
                false,
            )
            .unwrap();
        let deferred = take_pending(&shared);
        handle.session_started_turn(session, [predecessor].into());
        handle.record(attention_event(session, "this-turn"));
        let mut pending = deferred.followed_by(take_pending(&shared));
        assert_eq!(pending.started_turns, [session, predecessor].into());
        assert!(!pending.events.contains_key(&Target::Session(predecessor)));
        assert!(pending.events.contains_key(&Target::Session(unrelated)));
        assert_eq!(
            pending.events[&Target::Session(session)].evidence.key,
            "this-turn"
        );
        let root = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&root.path().join("attention.sqlite")).unwrap();
        let (event, commit) = pending.committed_events.pop_front().unwrap();
        assert!(
            record_attention_committed(&mut journal, event.clone(), false, Some(commit))
                .unwrap()
                .is_none()
        );
        assert_eq!(receipt.recv().unwrap(), Ok(()));
        journal
            .record(attention_event(unrelated, "new-event"), false)
            .unwrap();
        assert!(journal.record(event, false).unwrap().is_none());

        handle.session_started_turn(session, BTreeSet::new());
        let pending = pending.followed_by(take_pending(&shared));
        assert!(!pending.events.contains_key(&Target::Session(session)));
    }

    #[test]
    fn duplicate_click_deliveries_activate_once_and_keep_a_token() {
        let (shared, mut receiver) = shared();
        let id = Uuid::new_v4();
        // A signal without a token followed by an application action with one.
        shared.activate(id, None);
        shared.activate(id, Some("token".into()));
        shared.activate(id, Some("later".into()));
        assert!(receiver.try_recv().is_ok());
        assert_eq!(
            Vec::from(shared.pending.lock().unwrap().actions.clone()),
            [(id, Some("token".to_owned()))]
        );
        for index in 0..MAX_ACTIONS * 2 {
            shared.activate(Uuid::from_u128(index as u128), None);
        }
        assert_eq!(shared.pending.lock().unwrap().actions.len(), MAX_ACTIONS);
    }

    #[test]
    fn withdrawal_backlog_is_bounded_and_coalesced() {
        let mut queue = Withdrawals::default();
        for index in 0..MAX_PENDING + 10 {
            queue_withdrawal(&mut queue, Uuid::from_u128(index as u128));
        }
        assert_eq!(queue.len(), MAX_PENDING);
        let existing = Uuid::from_u128(MAX_PENDING as u128);
        queue_withdrawal(&mut queue, existing);
        assert_eq!(queue.len(), MAX_PENDING);
    }

    #[test]
    fn preview_revocation_withdraws_rich_notice_even_when_sqlite_cannot_rewrite_it() {
        use diesel::{Connection, connection::SimpleConnection};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let mut journal = journal::Journal::open(&path).unwrap();
        let mut event = Event::session(
            TerminalId::new(),
            crate::agent::Provider::Codex,
            "/repo".into(),
            crate::notifications::Evidence {
                key: "rich-response".into(),
                kind: crate::notifications::Kind::Succeeded,
            },
        );
        event.content = Some(crate::notifications::Content {
            conversation: "Private conversation".into(),
            preview: "Private response".into(),
            request: None,
        });
        let notice = journal.record(event, false).unwrap().unwrap();
        let mut snapshot = Snapshot {
            notices: journal.list().unwrap(),
            diagnostic: None,
        };
        let mut blocker =
            diesel::sqlite::SqliteConnection::establish(path.to_str().unwrap()).unwrap();
        blocker.batch_execute("BEGIN IMMEDIATE").unwrap();
        let mut withdrawals = Withdrawals::default();
        revoke_previews(&mut snapshot, &mut withdrawals);
        assert!(
            journal.clear_content().is_err(),
            "fixture must prevent the privacy rewrite"
        );
        assert!(withdrawals.contains(&batching::desktop_id(&notice.event.target)));
        assert!(snapshot.notices[0].event.content.is_none());
        assert_eq!(snapshot.notices[0].id, notice.id);
        assert!(!snapshot.notices[0].acknowledged);
        blocker.batch_execute("ROLLBACK").unwrap();
        assert!(
            journal.list().unwrap()[0].event.content.is_some(),
            "desktop/UI revocation must not rely on persisted content being cleared"
        );
    }

    #[test]
    fn grouped_delivery_routes_latest_notice_and_waits_for_older_withdrawal() {
        let make = |session, key: &str| Notice {
            id: Uuid::new_v4(),
            event: Event::session(
                session,
                crate::agent::Provider::Codex,
                "/repo".into(),
                crate::notifications::Evidence {
                    key: key.into(),
                    kind: crate::notifications::Kind::Waiting,
                },
            ),
            delivered: false,
            acknowledged: false,
            window: None,
        };
        let session = TerminalId::new();
        let old = make(session, "old");
        let current = make(session, "current");
        let other = make(TerminalId::new(), "other");
        let snapshot = Snapshot {
            notices: vec![current.clone(), other.clone()],
            diagnostic: None,
        };
        let group = batching::desktop_id(&old.event.target);
        assert_eq!(action_notice(&snapshot, group).unwrap().id, current.id);
        assert_eq!(
            action_notice(&snapshot, current.id).unwrap().id,
            current.id,
            "legacy portal UUIDs still route"
        );
        assert!(
            action_notice(&snapshot, old.id).is_none(),
            "an obsolete journal UUID is never retargeted"
        );
        let delivery = BTreeMap::from([
            (current.event.target(), current.clone()),
            (other.event.target(), other.clone()),
        ]);
        let now = Instant::now();
        let mut schedule = batching::Schedule::default();
        for target in delivery.keys() {
            schedule.update(target.clone(), crate::notifications::Kind::Waiting, now);
        }
        let mut withdrawals = Withdrawals::default();
        withdraw_notice(&mut withdrawals, &old);
        let due = now + Duration::from_secs(1);
        let batch = delivery_batch(&delivery, &schedule, &withdrawals, &VecDeque::new(), due);
        assert_eq!(batch.len(), 1);
        assert_eq!(
            batch[0].id, other.id,
            "unrelated conversations must not wait for this withdrawal"
        );
        withdrawals.remove_group(&group);
        assert_eq!(
            delivery_batch(&delivery, &schedule, &withdrawals, &VecDeque::new(), due).len(),
            2,
            "new delivery must follow withdrawal, otherwise cleanup would delete its replacement"
        );
    }

    #[test]
    fn automation_click_checks_the_exact_witness_even_if_projection_is_stale() {
        let id = Uuid::new_v4();
        let sequence = 18;
        let notice = Notice {
            id: Uuid::new_v4(),
            event: Event {
                target: Target::Automation { id, sequence },
                source_session: None,
                provider: None,
                directory: None,
                destination: None,
                evidence: crate::notifications::Evidence {
                    key: format!("automation:{id}:lifecycle:{sequence}"),
                    kind: crate::notifications::Kind::AutomationBlocked,
                },
                content: None,
            },
            acknowledged: false,
            delivered: true,
            window: None,
        };
        let snapshot = Snapshot {
            notices: vec![notice.clone()],
            ..Snapshot::default()
        };
        let still_visible = action_notice(&snapshot, notice.id).unwrap();
        for state in [
            Ok(true),
            Err("automation registry unavailable".into()),
            Ok(false),
        ] {
            let mut routed = false;
            let actual = action_if_current(
                still_visible,
                |current, witness| {
                    assert_eq!((current, witness), (id, sequence));
                    state.clone()
                },
                || {
                    routed = true;
                },
            );
            assert_eq!(routed, state == Ok(false));
            assert_eq!(actual, state.map(|resolved| (!resolved).then_some(())));
        }
    }

    #[test]
    fn no_run_commit_privacy_and_exact_group_activation_survive_reopen() {
        use crate::notifications::{Content, Evidence, Kind};
        for previews in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("no-run.sqlite3");
            let mut journal = Journal::open(&path).unwrap();
            let policy = Preferences {
                message_previews: previews,
                ..Default::default()
            };
            let id = Uuid::new_v4();
            let make = |sequence, kind| Event {
                target: Target::Automation { id, sequence },
                source_session: None,
                provider: None,
                directory: None,
                destination: None,
                evidence: Evidence {
                    key: format!("automation:{id}:lifecycle:{sequence}"),
                    kind,
                },
                content: Some(Content {
                    conversation: "PRIVATE_AUTOMATION_TITLE".into(),
                    preview: "PRIVATE_AUTOMATION_REASON".into(),
                    request: None,
                }),
            };
            let (shared, _wake) = shared();
            let handle = Handle(shared.clone());
            let mut committed = Vec::new();
            for event in [
                make(17, Kind::AutomationPaused),
                make(18, Kind::AutomationBlocked),
            ] {
                let exact = event.target.clone();
                let reply = handle.record_committed(event).unwrap();
                assert!(matches!(
                    reply.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ));
                let (queued, receipt) = shared
                    .pending
                    .lock()
                    .unwrap()
                    .committed_events
                    .pop_front()
                    .unwrap();
                let notice = record_attention_committed_policy(
                    &mut journal,
                    queued,
                    false,
                    Some(receipt),
                    &policy,
                    false,
                )
                .unwrap()
                .unwrap();
                assert_eq!(reply.recv_timeout(Duration::from_secs(1)).unwrap(), Ok(()));
                assert_eq!(notice.event.target, exact);
                assert_eq!(notice.event.content.is_some(), previews);
                committed.push(notice);
            }
            let old = &committed[0];
            let current = &committed[1];
            assert_eq!(
                batching::desktop_id(&old.event.target),
                batching::desktop_id(&current.event.target)
            );
            assert_ne!(old.event.target, current.event.target);
            drop(journal);
            let mut reopened = Journal::open(&path).unwrap();
            let snapshot = Snapshot {
                notices: reopened.list().unwrap(),
                diagnostic: None,
            };
            assert_eq!(snapshot.notices, vec![current.clone()]);
            let serialized = serde_json::to_string(&snapshot).unwrap();
            assert_eq!(serialized.contains("PRIVATE_AUTOMATION_TITLE"), previews);
            assert_eq!(serialized.contains("PRIVATE_AUTOMATION_REASON"), previews);
            assert!(
                action_notice(&snapshot, batching::desktop_id(&old.event.target)).is_none(),
                "a group-only click cannot select an automation lifecycle receipt"
            );
            assert!(
                action_notice(&snapshot, old.id).is_none(),
                "old journal UUID cannot silently activate a new lifecycle receipt"
            );
            assert_eq!(
                action_notice(&snapshot, current.id).unwrap().event.target,
                current.event.target
            );
            reopened.acknowledge(old.id).unwrap();
            assert!(
                !reopened.list().unwrap()[0].acknowledged,
                "old exact ACK does not consume successor attention"
            );
        }
    }
}
