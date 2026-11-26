//! One daemon broker: provider callbacks only coalesce bounded semantic records.
//! The broker alone touches SQLite and the desktop's notification service,
//! outside HostState. Linux delivers through the freedesktop portal; macOS,
//! where the daemon has no app bundle to post from, through `osascript`.
mod batching;
mod journal;
#[cfg(target_os = "macos")]
mod osascript;
#[cfg(target_os = "linux")]
mod portal;
mod withdrawals;
use journal::Journal;
#[cfg(target_os = "macos")]
use osascript::{Activation, PortalOwner, connect, remove, send, send_test};
#[cfg(target_os = "linux")]
use portal::{Activation, PortalOwner, connect, remove, send, send_test};
use withdrawals::Withdrawals;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, Weak},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use tokio::sync::mpsc;
use uuid::Uuid;

use super::state::{ClientId, HostLock, SharedHost};
use crate::{
    model::TerminalId,
    notifications::{Event, Notice, Preferences, Presence, Snapshot, Target, TestOutcome},
};

const MAX_PENDING: usize = 2048;
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

/// An in-flight commit is unacknowledged until the FULL journal transaction
/// returns. Early shutdown or unwinding must explicitly fail the waiter.
struct CommitReceipt(Option<std::sync::mpsc::Sender<Result<(), String>>>);

impl CommitReceipt {
    fn finish(mut self, result: Result<(), String>) {
        if let Some(reply) = self.0.take() {
            let _ = reply.send(result);
        }
    }
}

impl Drop for CommitReceipt {
    fn drop(&mut self) {
        if let Some(reply) = self.0.take() {
            let _ = reply.send(Err(
                "Notification journal did not commit; outcome remains unacknowledged".into(),
            ));
        }
    }
}

#[derive(Default)]
struct Pending {
    events: BTreeMap<Target, Event>,
    committed_events: VecDeque<(Event, CommitReceipt)>,
    inboxes: Option<InboxAttention>,
    retired: BTreeSet<Target>,
    acknowledged: BTreeSet<Uuid>,
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
    /// This batch's work with `newer`, a later batch, applied after it, as if
    /// both had been queued as one: a later retirement withdraws an earlier
    /// event for its session, and a later event replaces an earlier one.
    fn followed_by(mut self, newer: Pending) -> Pending {
        for target in &newer.retired {
            self.events.remove(target);
        }
        for target in newer.retired {
            if self.retired.len() < MAX_PENDING || self.retired.contains(&target) {
                self.retired.insert(target);
            }
        }
        for (target, event) in newer.events {
            if self.events.len() < MAX_PENDING || self.events.contains_key(&target) {
                self.events.insert(target, event);
            }
        }
        for id in newer.acknowledged {
            if self.acknowledged.len() < MAX_PENDING {
                self.acknowledged.insert(id);
            }
        }
        self.committed_events.extend(newer.committed_events);
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

    /// The work that waits for an unavailable journal rather than being lost:
    /// attention, retirements, acknowledgements and the settings and windows
    /// they are judged by. Receipts and tests are answered at once, and a
    /// click on a notice is not replayed long after the person made it.
    fn deferred(self) -> Pending {
        Pending {
            events: self.events,
            inboxes: self.inboxes,
            retired: self.retired,
            acknowledged: self.acknowledged,
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
    presence: Mutex<BTreeMap<ClientId, Presence>>,
    snapshot: Mutex<Snapshot>,
    wake: mpsc::Sender<()>,
}

#[derive(Clone)]
pub(super) struct Handle(Arc<Shared>);

impl Handle {
    /// Real bounded enqueue state without a broker, portal, or worker thread.
    #[cfg(test)]
    pub(in crate::host) fn inert_for_test() -> (Self, impl Fn() -> Vec<Event>) {
        let (wake, _receiver) = mpsc::channel(1);
        let shared = Arc::new(Shared {
            pending: Mutex::default(),
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

    pub fn record(&self, event: Event) {
        if event.validate().is_err() {
            return;
        }
        self.change(|pending| {
            // Producers fence worker identity before enqueueing. A newer
            // worker may reuse this session while retirement is still queued;
            // apply that retirement first, then its new event in the broker.
            if pending.events.len() < MAX_PENDING || pending.events.contains_key(&event.target()) {
                pending.events.insert(event.target(), event);
            }
        });
    }

    /// Receipt confirms the FULL journal transaction, never merely enqueueing.
    /// Callers wait outside HostState and keep helper outbox entries on error.
    pub(in crate::host) fn record_committed(
        &self,
        event: Event,
    ) -> Result<std::sync::mpsc::Receiver<Result<(), String>>, String> {
        event.validate()?;
        let (receipt, result) = std::sync::mpsc::channel();
        let mut pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.stopped || self.0.wake.is_closed() {
            return Err("Notification journal is stopping; outcome remains unacknowledged".into());
        }
        if pending.committed_events.len() >= MAX_PENDING {
            return Err("Notification commit queue is full; outcome remains unacknowledged".into());
        }
        pending
            .committed_events
            .push_back((event, CommitReceipt(Some(receipt))));
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

    pub fn retire(&self, session: TerminalId) {
        let session = Target::Session(session);
        self.change(|pending| {
            pending.events.remove(&session);
            pending
                .committed_events
                .retain(|(event, _)| event.target() != session);
            if pending.retired.len() < MAX_PENDING {
                pending.retired.insert(session);
            }
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

fn stopped(shared: &Weak<Shared>) -> bool {
    shared.upgrade().is_none_or(|shared| {
        shared
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopped
    })
}

fn queue_withdrawal(queue: &mut Withdrawals, id: Uuid) {
    queue.legacy(id);
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
) {
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
    if !policy.message_previews {
        revoke_previews(snapshot, withdrawals);
    }
    withdrawals.protect(
        snapshot
            .notices
            .iter()
            .map(|notice| batching::desktop_id(&notice.event.target)),
    );
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

fn action_notice(snapshot: &Snapshot, id: Uuid) -> Option<&Notice> {
    snapshot
        .notices
        .iter()
        .find(|notice| notice.id == id || batching::desktop_id(&notice.event.target) == id)
}

fn delivery_batch(
    delivery: &BTreeMap<Target, Notice>,
    schedule: &batching::Schedule,
    withdrawals: &Withdrawals,
    now: Instant,
) -> Vec<Notice> {
    delivery
        .values()
        .filter(|notice| {
            schedule.ready(&notice.event.target(), now)
                && !withdrawals.contains(&batching::desktop_id(&notice.event.target))
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
) -> Result<Option<Notice>, String> {
    if !policy.message_previews {
        event.content = None;
    }
    record_attention_committed(journal, event, suppressed, receipt)
}

fn record_broker_event(
    journal: &mut Journal,
    event: Event,
    suppressed: bool,
    receipt: Option<CommitReceipt>,
    policy: &Preferences,
    registry: impl FnOnce() -> Result<super::agent_execution::SharedRegistry, String>,
) -> Result<Option<Notice>, String> {
    if let Target::Automation { id, sequence } = event.target {
        let current = registry().and_then(|registry| {
            registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .automation_attention_resolved(id, sequence)
        });
        if !matches!(current, Ok(false)) {
            // The management transaction may have removed the outbox row
            // after its transfer already queued this event. Its resolution
            // is authoritative before the journal publishes any unread edge.
            if let Some(receipt) = receipt {
                receipt.finish(current.clone().map(|_| ()));
            }
            return current.map(|_| None);
        }
    }
    record_attention_committed_policy(journal, event, suppressed, receipt, policy)
}

fn restore_automation_attention(
    journal: &mut Journal,
    host: &Weak<HostLock>,
    snapshot: &mut Snapshot,
    withdrawals: &mut Withdrawals,
) {
    let Some(host) = host.upgrade() else {
        return;
    };
    let Ok(registry) = super::agent_execution::registry(&host) else {
        return;
    };
    let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    for notice in &mut snapshot.notices {
        if let Target::Automation { id, sequence } = notice.event.target
            && !notice.acknowledged
        {
            match registry.automation_attention_resolved(id, sequence) {
                Ok(true) => match journal.acknowledge(notice.id) {
                    Ok(_) => {
                        notice.acknowledged = true;
                        withdraw_notice(withdrawals, notice);
                    }
                    Err(error) => snapshot.diagnostic = Some(error),
                },
                Ok(false) => {}
                Err(error) => snapshot.diagnostic = Some(error),
            }
        }
    }
}

fn record_attention_committed(
    journal: &mut Journal,
    event: Event,
    suppressed: bool,
    receipt: Option<CommitReceipt>,
) -> Result<Option<crate::notifications::Notice>, String> {
    let result = journal.record(event, suppressed);
    if let Some(receipt) = receipt {
        let committed = result.as_ref().map(|_| ()).map_err(Clone::clone);
        receipt.finish(committed);
    }
    result
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
    pub(in crate::host) fn new(path: &std::path::Path) -> Result<(Handle, Self), String> {
        let journal = Journal::open(path)?;
        let (wake, receiver) = mpsc::channel(1);
        let handle = Handle(Arc::new(Shared {
            pending: Mutex::default(),
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
            record_attention_committed_policy(journal, event, suppressed, Some(receipt), policy)
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

    fn commit_next_with(
        &mut self,
        commit: impl FnOnce(&mut Journal, Event, CommitReceipt) -> Result<Option<Notice>, String>,
    ) -> Result<Option<Notice>, String> {
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

async fn run(
    shared: Weak<Shared>,
    host: Weak<HostLock>,
    mut receiver: mpsc::Receiver<()>,
    application: crate::notifications::Application,
) {
    let path =
        crate::instance::project_dirs().map(|dirs| dirs.data_dir().join("agent-attention.sqlite3"));
    let mut store = journal::Reopening::open(path, Instant::now());
    // Work held while the journal is unavailable, applied before the next batch.
    let mut deferred = Pending::default();
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
    let mut snapshot = Snapshot {
        diagnostic: settings_error.clone(),
        ..Default::default()
    };
    match store.journal() {
        Ok(journal) => {
            restore(journal, &policy, &mut snapshot, &mut withdrawals);
            restore_automation_attention(journal, &host, &mut snapshot, &mut withdrawals);
        }
        Err(error) => snapshot.diagnostic = Some(error.clone()),
    }
    publish(&shared, &host, snapshot.clone());
    while wait_for_work(
        &mut receiver,
        if proxy.is_none() {
            if !withdrawals.is_empty() {
                Some(next_attempt)
            } else {
                delivery_schedule
                    .deadline()
                    .map(|due| due.max(next_attempt))
            }
        } else {
            delivery_schedule.deadline()
        }
        .into_iter()
        .chain(store.retry_at())
        .min(),
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
            restore(journal, &policy, &mut snapshot, &mut withdrawals);
            restore_automation_attention(journal, &host, &mut snapshot, &mut withdrawals);
        }
        let journal = match store.journal() {
            Ok(journal) => journal,
            Err(error) => {
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
        if let Some(current) = current_presence {
            for (client, update) in &current {
                if stopped(&shared) {
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
        if pending.reload {
            match pending.preferences.map_or_else(preferences, Ok) {
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
        for session in pending.retired {
            if stopped(&shared) {
                return;
            }
            delivery.remove(&session);
            if let Some(old) = snapshot
                .notices
                .iter()
                .find(|notice| notice.event.target() == session)
            {
                withdraw_notice(&mut withdrawals, old);
            }
            if let Err(error) = journal.retire(session) {
                snapshot.diagnostic = Some(error);
            }
        }
        for id in pending.acknowledged {
            if stopped(&shared) {
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
        let events = pending
            .events
            .into_values()
            .map(|event| (event, None))
            .chain(
                pending
                    .committed_events
                    .into_iter()
                    .map(|(event, receipt)| (event, Some(receipt))),
            );
        for (event, receipt) in events {
            if stopped(&shared) {
                return;
            }
            let focused = presence.values().any(|window| {
                event
                    .source_session()
                    .is_some_and(|session| window.focused == Some(session))
                    || window.focused_target.as_ref() == Some(&event.target)
            });
            let suppressed = !policy.allows(event.evidence.kind, focused);
            match record_broker_event(journal, event, suppressed, receipt, &policy, || {
                let host = host.upgrade().ok_or("The daemon has stopped")?;
                super::agent_execution::registry(&host)
            }) {
                Ok(Some(notice)) => {
                    delivery.remove(&notice.event.target());
                    if let Some(old) = snapshot.notices.iter().find(|old| {
                        old.event.target.storage_key() == notice.event.target.storage_key()
                    }) {
                        if suppressed {
                            withdraw_notice(&mut withdrawals, old);
                        } else {
                            queue_withdrawal(&mut withdrawals, old.id);
                        }
                    }
                    if !suppressed {
                        delivery_schedule.update(
                            notice.event.target(),
                            notice.event.evidence.kind,
                            Instant::now(),
                        );
                        delivery.insert(notice.event.target(), notice);
                    }
                }
                Ok(None) => {}
                Err(error) => snapshot.diagnostic = Some(error),
            }
        }
        match journal.list() {
            Ok(notices) => snapshot.notices = notices,
            Err(error) => snapshot.diagnostic = Some(error),
        }
        if !policy.message_previews {
            for notice in &mut snapshot.notices {
                notice.event.content = None;
            }
        }
        withdrawals.protect(
            snapshot
                .notices
                .iter()
                .map(|notice| batching::desktop_id(&notice.event.target)),
        );
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
        delivery.retain(|_, notice| retained.contains(&notice.id));
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
        let backlog_due = (delivery
            .keys()
            .any(|target| delivery_schedule.ready(target, Instant::now()))
            || !withdrawals.is_empty())
            && Instant::now() >= next_attempt;
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
                delivery_batch(&delivery, &delivery_schedule, &withdrawals, Instant::now())
            };
            for notice in batch {
                if stopped(&shared) {
                    return;
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
                if stopped(&shared) {
                    return;
                }
                if failed {
                    break;
                }
                let Some(entry) = withdrawals.pop_first() else {
                    break;
                };
                if let Err(error) = remove(backend, entry.id()).await {
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
            } else if (delivery
                .keys()
                .any(|target| delivery_schedule.ready(target, Instant::now()))
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
        if !policy.message_previews {
            for notice in &mut snapshot.notices {
                notice.event.content = None;
            }
        }
        for (action, token) in pending.actions {
            if let Some(notice) = action_notice(&snapshot, action)
                && let Some(host) = host.upgrade()
            {
                let window = host
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .activate_notification_destination(
                        notice.event.target(),
                        notice.event.source_session(),
                        notice.window.as_deref(),
                        notice.event.destination.as_ref(),
                    );
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
        let notice =
            record_attention_committed_policy(&mut journal, queued, true, Some(receipt), &policy)
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
        assert!(record_attention_committed_policy(
            &mut reopened, queued, false, Some(receipt), &policy,
        ).unwrap().is_none());
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
                let receipt = (path != "ordinary").then_some(CommitReceipt(Some(receipt)));
                let notice =
                    record_attention_committed_policy(&mut journal, event, false, receipt, &policy)
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
    fn retirement_coalescing_preserves_only_later_worker_evidence() {
        let (wake, _receiver) = mpsc::channel(1);
        let handle = Handle(Arc::new(Shared {
            pending: Mutex::default(),
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
            presence: Mutex::default(),
            snapshot: Mutex::default(),
            wake,
        });
        (shared, receiver)
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
        let batch = delivery_batch(&delivery, &schedule, &withdrawals, due);
        assert_eq!(batch.len(), 1);
        assert_eq!(
            batch[0].id, other.id,
            "unrelated conversations must not wait for this withdrawal"
        );
        withdrawals.remove_group(&group);
        assert_eq!(
            delivery_batch(&delivery, &schedule, &withdrawals, due).len(),
            2,
            "new delivery must follow withdrawal, otherwise cleanup would delete its replacement"
        );
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
            assert_eq!(
                action_notice(&snapshot, batching::desktop_id(&old.event.target))
                    .unwrap()
                    .event
                    .target,
                current.event.target
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
