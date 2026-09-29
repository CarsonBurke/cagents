//! One daemon broker: provider callbacks only coalesce bounded semantic records.
//! The broker alone touches SQLite and the desktop portal, outside HostState.
mod journal;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, Weak},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use ashpd::desktop::notification::{Notification, NotificationProxy};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::state::{ClientId, HostState, SharedHost};
use crate::{
    model::TerminalId,
    notifications::{Event, Notice, Preferences, Presence, Snapshot, Target, TestOutcome},
};

const MAX_PENDING: usize = 2048;
const PORTAL_TIMEOUT: Duration = Duration::from_secs(3);
const RETRY_DELAY: Duration = Duration::from_secs(30);

struct Portal {
    proxy: NotificationProxy<'static>,
    listener: tokio::task::JoinHandle<()>,
}

impl Drop for Portal {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

#[derive(Default)]
struct Pending {
    events: BTreeMap<Target, Event>,
    retired: BTreeSet<Target>,
    acknowledged: BTreeSet<Uuid>,
    presence_changed: bool,
    actions: VecDeque<(Uuid, Option<String>)>,
    reload: bool,
    preferences: Option<Preferences>,
    /// Requesters waiting on a test notification, each answered once.
    tests: Vec<std::sync::mpsc::Sender<TestOutcome>>,
    stopped: bool,
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
    fn change(&self, change: impl FnOnce(&mut Pending)) {
        change(&mut self.0.pending.lock().unwrap_or_else(|p| p.into_inner()));
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

    pub fn retire(&self, session: TerminalId) {
        let session = Target::Session(session);
        self.change(|pending| {
            pending.events.remove(&session);
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
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => runtime.block_on(run(shared, host, receiver, application)),
                    Err(error) => publish(
                        &shared,
                        &host,
                        Snapshot {
                            diagnostic: Some(format!("Notifications unavailable: {error}")),
                            ..Default::default()
                        },
                    ),
                }
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

fn publish(shared: &Weak<Shared>, host: &Weak<Mutex<HostState>>, snapshot: Snapshot) {
    if let Some(shared) = shared.upgrade() {
        let mut current = shared.snapshot.lock().unwrap_or_else(|p| p.into_inner());
        if *current == snapshot {
            return;
        }
        *current = snapshot;
        drop(current);
        if let Some(host) = host.upgrade() {
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .notifications_changed();
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

async fn connect(
    shared: Weak<Shared>,
    application: crate::notifications::Application,
) -> Result<Portal, String> {
    tokio::time::timeout(PORTAL_TIMEOUT, async move {
        // A host daemon is not launched from the UI desktop file's cgroup.
        // Registration gives the portal the same application identity as UI.
        let _ = ashpd::register_host_app(
            crate::instance::notification_desktop_id(application)
                .parse()
                .map_err(|error| format!("Invalid application identity: {error}"))?,
        )
        .await;
        let proxy = NotificationProxy::new()
            .await
            .map_err(|error| error.to_string())?;
        let listener = NotificationProxy::new()
            .await
            .map_err(|error| error.to_string())?;
        let (ready, subscribed) = tokio::sync::oneshot::channel();
        let listener = tokio::spawn(async move {
            let actions = match listener.receive_action_invoked().await {
                Ok(actions) => actions,
                Err(error) => {
                    let _ = ready.send(Err(error.to_string()));
                    return;
                }
            };
            let _ = ready.send(Ok(()));
            futures_util::pin_mut!(actions);
            while let Some(action) = actions.next().await {
                tracing::debug!(
                    id = action.id(),
                    action = action.name(),
                    "desktop notification action received"
                );
                let Some(shared) = shared.upgrade() else {
                    break;
                };
                if action.name() != "open" {
                    continue;
                }
                let Ok(id) = Uuid::parse_str(action.id()) else {
                    continue;
                };
                let mut pending = shared.pending.lock().unwrap_or_else(|p| p.into_inner());
                if pending.actions.len() < 32 {
                    let token: Option<String> = action.parameter().iter().find_map(|parameter| {
                        let dictionary: std::collections::HashMap<
                            String,
                            ashpd::zvariant::OwnedValue,
                        > = parameter.try_clone().ok()?.try_into().ok()?;
                        dictionary
                            .get("activation-token")?
                            .try_clone()
                            .ok()?
                            .try_into()
                            .ok()
                    });
                    pending
                        .actions
                        .push_back((id, token.filter(|token| token.len() <= 4096)));
                }
                drop(pending);
                let _ = shared.wake.try_send(());
            }
        });
        let portal = Portal { proxy, listener };
        // Register the action match before adding the first notification. The
        // Portal guard aborts the listener if readiness fails or times out.
        subscribed.await.map_err(|error| error.to_string())??;
        Ok(portal)
    })
    .await
    .map_err(|_| "Desktop notification portal timed out".to_owned())?
}

async fn send(proxy: &NotificationProxy<'_>, notice: &Notice) -> Result<(), String> {
    let body = crate::notifications::context(&notice.event);
    let notification = Notification::new(notice.event.evidence.kind.label())
        .body(body.as_str())
        .default_action("open");
    tokio::time::timeout(
        PORTAL_TIMEOUT,
        proxy.add_notification(&notice.id.to_string(), notification),
    )
    .await
    .map_err(|_| "Desktop notification portal timed out".to_owned())?
    .map_err(|error| format!("Desktop notifications unavailable: {error}"))
}

async fn remove(proxy: &Portal, id: Uuid) -> Result<(), String> {
    tokio::time::timeout(
        PORTAL_TIMEOUT,
        proxy.proxy.remove_notification(&id.to_string()),
    )
    .await
    .map_err(|_| "Desktop notification portal timed out".to_owned())?
    .map_err(|error| format!("Could not withdraw desktop notification: {error}"))
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

fn queue_withdrawal(queue: &mut BTreeSet<Uuid>, id: Uuid) {
    if queue.len() == MAX_PENDING && !queue.contains(&id) {
        queue.pop_first();
    }
    queue.insert(id);
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn withdrawal_backlog_is_bounded_and_coalesced() {
        let mut queue = BTreeSet::new();
        for index in 0..MAX_PENDING + 10 {
            queue_withdrawal(&mut queue, Uuid::from_u128(index as u128));
        }
        assert_eq!(queue.len(), MAX_PENDING);
        let existing = *queue.last().unwrap();
        queue_withdrawal(&mut queue, existing);
        assert_eq!(queue.len(), MAX_PENDING);
    }
}

async fn run(
    shared: Weak<Shared>,
    host: Weak<Mutex<HostState>>,
    mut receiver: mpsc::Receiver<()>,
    application: crate::notifications::Application,
) {
    let path =
        crate::instance::project_dirs().map(|dirs| dirs.data_dir().join("agent-attention.sqlite3"));
    let mut journal = path
        .ok_or_else(|| "Notification data directory is unavailable".to_owned())
        .and_then(|path| journal::Journal::open(&path));
    let settings = preferences();
    let settings_error = settings.as_ref().err().cloned();
    let mut policy = settings.unwrap_or_else(|_| Preferences {
        enabled: false,
        ..Default::default()
    });
    let mut presence = BTreeMap::<ClientId, Presence>::new();
    let mut proxy = None;
    // Only events observed during this daemon lifetime are eligible for retry.
    // Restored attention stays visible in the UI without announcing old backlog.
    let mut delivery = BTreeMap::<Target, Notice>::new();
    let mut withdrawals = BTreeSet::new();
    let mut next_attempt = Instant::now();
    let mut retry_delay = RETRY_DELAY;
    let mut snapshot = Snapshot {
        diagnostic: settings_error,
        ..Default::default()
    };
    match &journal {
        Ok(journal) => match journal.list() {
            Ok(notices) => snapshot.notices = notices,
            Err(error) => snapshot.diagnostic = Some(error),
        },
        Err(error) => snapshot.diagnostic = Some(error.clone()),
    }
    publish(&shared, &host, snapshot.clone());
    while wait_for_work(
        &mut receiver,
        (proxy.is_none() && (!delivery.is_empty() || !withdrawals.is_empty()))
            .then_some(next_attempt),
    )
    .await
    {
        let Some(core) = shared.upgrade() else {
            break;
        };
        let pending = std::mem::take(&mut *core.pending.lock().unwrap_or_else(|p| p.into_inner()));
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
        let Ok(journal) = &mut journal else {
            continue;
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
            next_attempt = Instant::now();
            retry_delay = RETRY_DELAY;
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
                queue_withdrawal(&mut withdrawals, old.id);
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
            queue_withdrawal(&mut withdrawals, id);
        }
        for event in pending.events.into_values() {
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
            match journal.record(event, suppressed) {
                Ok(Some(notice)) => {
                    delivery.remove(&notice.event.target());
                    if let Some(old) = snapshot
                        .notices
                        .iter()
                        .find(|old| old.event.target() == notice.event.target())
                    {
                        queue_withdrawal(&mut withdrawals, old.id);
                    }
                    if !suppressed {
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
        let backlog = !delivery.is_empty() || !withdrawals.is_empty();
        // A test is a person asking right now, so it retries a portal that
        // the backlog is still backing off from. Its failure is answered to
        // that person rather than raised as every window's diagnostic, and
        // leaves the backlog's schedule alone.
        if proxy.is_none()
            && ((backlog && Instant::now() >= next_attempt) || !pending.tests.is_empty())
        {
            match connect(shared.clone(), application).await {
                Ok(connected) => {
                    proxy = Some(connected);
                    snapshot.diagnostic = None;
                    retry_delay = RETRY_DELAY;
                }
                Err(error) => {
                    answer_tests(&pending.tests, &TestOutcome::Unavailable(error.clone()));
                    if backlog {
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
                let test = Notification::new("cagents notifications work")
                    .body("Agent completion and attention alerts appear here.");
                let outcome = match tokio::time::timeout(
                    PORTAL_TIMEOUT,
                    backend
                        .proxy
                        .add_notification("cagents-notification-test", test),
                )
                .await
                {
                    Ok(Ok(())) => TestOutcome::Sent,
                    Ok(Err(error)) => TestOutcome::Unavailable(error.to_string()),
                    Err(_) => TestOutcome::TimedOut,
                };
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
                delivery.values().take(8).cloned().collect()
            };
            for notice in batch {
                if stopped(&shared) {
                    return;
                }
                match send(&backend.proxy, &notice).await {
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
                let Some(id) = withdrawals.pop_first() else {
                    break;
                };
                if let Err(error) = remove(backend, id).await {
                    withdrawals.insert(id);
                    snapshot.diagnostic = Some(error);
                    failed = true;
                    break;
                }
            }
            if failed {
                proxy = None;
                next_attempt = Instant::now() + retry_delay;
                retry_delay = (retry_delay * 2).min(Duration::from_secs(300));
            } else if (!delivery.is_empty() || !withdrawals.is_empty())
                && let Some(shared) = shared.upgrade()
            {
                let _ = shared.wake.try_send(());
            }
        }
        match journal.list() {
            Ok(notices) => snapshot.notices = notices,
            Err(error) => snapshot.diagnostic = Some(error),
        }
        for (action, token) in pending.actions {
            if let Some(notice) = snapshot.notices.iter().find(|notice| notice.id == action) {
                if let Some(host) = host.upgrade() {
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
                                snapshot.diagnostic = Some(format!(
                                    "Could not open the notification's window: {error}"
                                ));
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
        }
        publish(&shared, &host, snapshot.clone());
    }
}
