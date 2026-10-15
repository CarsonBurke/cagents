//! One daemon broker: provider callbacks only coalesce bounded semantic records.
//! The broker alone touches SQLite and the desktop portal, outside HostState.
mod journal;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex, Weak},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use ashpd::{
    desktop::notification::{Notification, NotificationProxy},
    zbus,
    zvariant::OwnedValue,
};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::state::{ClientId, HostLock, SharedHost};
use crate::{
    model::TerminalId,
    notifications::{Event, Notice, Preferences, Presence, Snapshot, Target, TestOutcome},
};

const MAX_PENDING: usize = 2048;
const MAX_ACTIONS: usize = 32;
const MAX_ACTIVATION_TOKEN: usize = 4096;
const PORTAL_TIMEOUT: Duration = Duration::from_secs(3);
const RETRY_DELAY: Duration = Duration::from_secs(30);

/// The application action a notification's default action invokes, with the
/// notice identity as its target. Notification backends deliver `app.` actions
/// to the application's `org.freedesktop.Application` object together with the
/// activation token the notification server minted for the click; without that
/// token a Wayland compositor refuses to raise the window.
const OPEN_ACTION: &str = "open-notification";
/// The portal-signal action used while this daemon cannot receive application
/// actions. Backends such as xdg-desktop-portal-gtk drop the activation token on
/// this path, so the window is routed but may not be raised.
const SIGNAL_OPEN_ACTION: &str = "open";

struct Portal {
    proxy: NotificationProxy<'static>,
    listener: tokio::task::JoinHandle<()>,
}

impl Drop for Portal {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

/// What the portal knows of this daemon's application ID. Registration binds
/// the D-Bus connection, and ashpd shares one connection for the process, so
/// the outcome outlives any single `Portal`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Registration {
    #[default]
    Untried,
    /// A `Register` call was sent, but a timeout discarded its reply.
    Unknown,
    Registered,
    /// The portal identifies this connection some other way, such as when no
    /// desktop entry exists for the ID or the portal predates registration.
    Refused,
}

impl Registration {
    fn needs_attempt(self) -> bool {
        matches!(self, Self::Untried | Self::Unknown)
    }

    /// The state after a `Register` attempt that started from `self`.
    fn settle(self, outcome: Result<(), String>) -> Self {
        match outcome {
            Ok(()) => Self::Registered,
            // The portal rejects a second registration of one connection. After
            // a lost reply that registration can only have been ours: this
            // daemon makes no other portal call before registering.
            Err(error)
                if self == Self::Unknown && error.contains("Connection already associated") =>
            {
                Self::Registered
            }
            Err(_) => Self::Refused,
        }
    }
}

#[derive(Default)]
struct InboxAttention {
    pending: BTreeSet<crate::agent_execution::Lineage>,
    events: BTreeMap<Target, Event>,
}

#[derive(Default)]
struct Pending {
    events: BTreeMap<Target, Event>,
    // Durable scheduler evidence must reach the journal in commit order. Its
    // target includes mutable presentation metadata and cannot be sorted or
    // coalesced before each exact evidence key receives a journal receipt.
    automation_events: VecDeque<Event>,
    automation_keys: BTreeSet<String>,
    inboxes: Option<InboxAttention>,
    retired: BTreeSet<Target>,
    acknowledged: BTreeSet<Uuid>,
    presence_changed: bool,
    actions: VecDeque<(Uuid, Option<String>)>,
    /// The portal service's latest owner, when it changed; `Some(None)` means
    /// it exited. A different owner has forgotten this daemon's registration.
    portal_owner: Option<Option<zbus::names::OwnedUniqueName>>,
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
    /// Scheduler outbox receipts are emitted only after the journal commits.
    automation_receipts: Mutex<BTreeSet<String>>,
    wake: mpsc::Sender<()>,
}

#[derive(Clone)]
pub(super) struct Handle(Arc<Shared>);

impl Handle {
    fn change(&self, change: impl FnOnce(&mut Pending)) {
        let mut pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.stopped {
            return;
        }
        change(&mut pending);
        drop(pending);
        let _ = self.0.wake.try_send(());
    }

    pub(super) fn take_automation_receipts(&self) -> BTreeSet<String> {
        std::mem::take(
            &mut *self
                .0
                .automation_receipts
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        )
    }

    pub fn record(&self, event: Event) {
        if event.validate().is_err() {
            return;
        }
        self.change(|pending| {
            // Producers fence worker identity before enqueueing. A newer
            // worker may reuse this session while retirement is still queued;
            // apply that retirement first, then its new event in the broker.
            let automation = matches!(
                event.target,
                Target::Automation { .. } | Target::AutomationProposal { .. }
            );
            if automation {
                if pending.events.len() + pending.automation_events.len() < MAX_PENDING
                    && pending.automation_keys.insert(event.evidence.key.clone())
                {
                    pending.automation_events.push_back(event);
                }
            } else if pending.events.contains_key(&event.target())
                || pending.events.len() + pending.automation_events.len() < MAX_PENDING
            {
                pending.events.insert(event.target(), event);
            }
        });
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
            automation_receipts: Mutex::default(),
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

impl Shared {
    /// Queue one desktop activation of a notice for the broker loop.
    ///
    /// A click can arrive both as an application action and as a portal signal
    /// on backends that emit both. Deliveries that meet in the queue coalesce
    /// into one activation that keeps whichever carried an activation token;
    /// activation is idempotent, so a delivery after the drain is harmless.
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

/// `org.freedesktop.Application` object path for a desktop ID, per the
/// Desktop Entry specification's D-Bus activation section.
fn application_object_path(desktop_id: &str) -> String {
    format!("/{}", desktop_id.replace('.', "/").replace('-', "_"))
}

/// The activation token in desktop platform data. `desktop-startup-id` is the
/// X11-era key some backends still send alongside, or instead of, the
/// Wayland `activation-token`.
fn platform_activation_token(
    platform_data: &std::collections::HashMap<String, OwnedValue>,
) -> Option<String> {
    ["activation-token", "desktop-startup-id"]
        .into_iter()
        .find_map(|key| String::try_from(platform_data.get(key)?.try_clone().ok()?).ok())
        .filter(|token| !token.is_empty() && token.len() <= MAX_ACTIVATION_TOKEN)
}

/// The notice an `ActivateAction` call opens. Its first parameter is the
/// target the notification declared: the notice identity.
fn activated_notice(action: &str, parameter: &[OwnedValue]) -> Option<Uuid> {
    if action != OPEN_ACTION {
        return None;
    }
    let target = match &**parameter.first()? {
        // The target is a variant inside `av`; tolerate one extra boxing.
        ashpd::zvariant::Value::Value(boxed) => &**boxed,
        target => target,
    };
    let target: &str = target.downcast_ref().ok()?;
    Uuid::parse_str(target).ok()
}

/// Whether a portal `ActionInvoked` signal names a notice's open action.
/// Backends that forward application actions as signals keep the `app.`
/// prefix; the notification ID itself identifies the notice.
fn signal_opens_notice(action: &str) -> bool {
    action == SIGNAL_OPEN_ACTION || action.strip_prefix("app.") == Some(OPEN_ACTION)
}

/// Receives notification clicks that backends deliver as application actions.
/// This identity exists only for notification routing, so launching the
/// application or opening files through it is refused explicitly.
struct ApplicationActions(Weak<Shared>);

#[zbus::interface(name = "org.freedesktop.Application", crate = "ashpd::zbus")]
impl ApplicationActions {
    fn activate(
        &self,
        _platform_data: std::collections::HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "cagentsd only activates windows through notification actions".into(),
        ))
    }

    fn open(
        &self,
        _uris: Vec<String>,
        _platform_data: std::collections::HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "cagentsd does not open files".into(),
        ))
    }

    fn activate_action(
        &self,
        action_name: &str,
        parameter: Vec<OwnedValue>,
        platform_data: std::collections::HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        let id = activated_notice(action_name, &parameter).ok_or_else(|| {
            zbus::fdo::Error::InvalidArgs(format!("unknown notification action {action_name:?}"))
        })?;
        let shared = self
            .0
            .upgrade()
            .ok_or_else(|| zbus::fdo::Error::Failed("notifications are stopping".into()))?;
        tracing::debug!(%id, "desktop notification application action received");
        shared.activate(id, platform_activation_token(&platform_data));
        Ok(())
    }
}

/// Own the desktop ID's bus name so notification backends can deliver the
/// clicked notice and its activation token as an application action.
///
/// The name is never queued for: a queued request would leave backends calling
/// whichever process owns it now.
async fn serve_application_actions(
    shared: Weak<Shared>,
    desktop_id: &str,
) -> zbus::Result<zbus::Connection> {
    let connection = zbus::connection::Builder::session()?
        .serve_at(
            application_object_path(desktop_id),
            ApplicationActions(shared),
        )?
        .build()
        .await?;
    // Without queueing, a name another process owns is refused with a reply,
    // not an error; clicks addressed to it would reach that process.
    match connection
        .request_name_with_flags(desktop_id, zbus::fdo::RequestNameFlags::DoNotQueue.into())
        .await?
    {
        zbus::fdo::RequestNameReply::PrimaryOwner | zbus::fdo::RequestNameReply::AlreadyOwner => {
            Ok(connection)
        }
        zbus::fdo::RequestNameReply::InQueue | zbus::fdo::RequestNameReply::Exists => {
            Err(zbus::Error::NameTaken)
        }
    }
}

/// Receives desktop notification clicks for the daemon's lifetime.
///
/// The application-action endpoint is deliberately independent of `Portal`:
/// notices already on screen keep addressing it while a failed delivery
/// reconnects the portal.
#[derive(Default)]
struct Activation {
    registration: Registration,
    /// The portal instance that decided `registration`.
    portal: Option<zbus::names::OwnedUniqueName>,
    /// The portal was reached but the bus would not name its owner, so the
    /// next owner change cannot be compared and counts as a replacement.
    portal_unknown: bool,
    actions: Option<zbus::Connection>,
}

impl Activation {
    /// Whether notices can use the application action, which carries the
    /// click's activation token, rather than the portal signal.
    fn receives_actions(&self) -> bool {
        self.registration == Registration::Registered && self.actions.is_some()
    }

    /// Forget a registration held by a portal instance that has exited or been
    /// replaced. Returns whether the portal must be reconnected so the new
    /// instance hears `Register` before any other call.
    fn portal_changed(&mut self, owner: Option<zbus::names::OwnedUniqueName>) -> bool {
        if self.portal_unknown {
            self.portal_unknown = false;
            // The instance may still be the one we registered with, which
            // rejects a second `Register` as already associated; `Unknown`
            // reads that rejection as the registration it is.
            self.registration = match self.registration {
                Registration::Registered => Registration::Unknown,
                _ => Registration::Untried,
            };
            self.portal = None;
            return true;
        }
        if self.portal.is_none() || self.portal == owner {
            return false;
        }
        self.registration = Registration::Untried;
        self.portal = None;
        true
    }

    async fn prepare(
        &mut self,
        shared: &Weak<Shared>,
        application: crate::notifications::Application,
    ) -> Result<(), String> {
        let desktop_id = crate::instance::notification_desktop_id(application);
        // A host daemon is not launched from the UI desktop file's cgroup.
        // Registration gives the portal the same application identity as UI.
        if self.registration.needs_attempt() {
            let id = desktop_id
                .parse()
                .map_err(|error| format!("Invalid application identity: {error}"))?;
            let previous = self.registration;
            // A timeout may drop this future after the call was sent.
            self.registration = Registration::Unknown;
            let outcome = ashpd::register_host_app(id).await;
            if let Err(error) = &outcome {
                tracing::debug!(%error, "desktop portal did not register the application");
            }
            self.registration = previous.settle(outcome.map_err(|error| error.to_string()));
        }
        // Backends address application actions to the portal's view of the
        // application ID, which is ours only once registration succeeded.
        if self.registration == Registration::Registered && self.actions.is_none() {
            self.actions = serve_application_actions(shared.clone(), desktop_id)
                .await
                .inspect_err(|error| {
                    tracing::warn!(
                        %error,
                        "cannot receive notification actions; clicks may not raise windows"
                    );
                })
                .ok();
        }
        Ok(())
    }
}

async fn connect(
    shared: Weak<Shared>,
    application: crate::notifications::Application,
    activation: &mut Activation,
) -> Result<Portal, String> {
    tokio::time::timeout(PORTAL_TIMEOUT, async move {
        activation.prepare(&shared, application).await?;
        let proxy = NotificationProxy::new()
            .await
            .map_err(|error| error.to_string())?;
        if activation.portal.is_none() {
            // Asks the bus, not the portal, so it cannot affect registration.
            activation.portal = zbus::fdo::DBusProxy::new(proxy.connection())
                .await
                .map_err(|error| error.to_string())?
                .get_name_owner(proxy.destination().to_owned())
                .await
                .ok();
            activation.portal_unknown = activation.portal.is_none();
        }
        let listener = NotificationProxy::new()
            .await
            .map_err(|error| error.to_string())?;
        let (ready, subscribed) = tokio::sync::oneshot::channel();
        let listener = tokio::spawn(async move {
            let subscriptions = async {
                Ok::<_, ashpd::Error>((
                    listener.receive_action_invoked().await?,
                    listener.receive_owner_changed().await?,
                ))
            };
            let (actions, owners) = match subscriptions.await {
                Ok(streams) => streams,
                Err(error) => {
                    let _ = ready.send(Err(error.to_string()));
                    return;
                }
            };
            let _ = ready.send(Ok(()));
            futures_util::pin_mut!(actions);
            futures_util::pin_mut!(owners);
            loop {
                let action = tokio::select! {
                    action = actions.next() => action,
                    owner = owners.next() => {
                        let (Some(owner), Some(shared)) = (owner, shared.upgrade()) else {
                            break;
                        };
                        shared
                            .pending
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .portal_owner = Some(owner.map(Into::into));
                        let _ = shared.wake.try_send(());
                        continue;
                    }
                };
                let Some(action) = action else {
                    break;
                };
                tracing::debug!(
                    id = action.id(),
                    action = action.name(),
                    "desktop notification action received"
                );
                let Some(shared) = shared.upgrade() else {
                    break;
                };
                if !signal_opens_notice(action.name()) {
                    continue;
                }
                let Ok(id) = Uuid::parse_str(action.id()) else {
                    continue;
                };
                // Portal v2 backends append platform data after the target.
                let token = action.parameter().iter().find_map(|parameter| {
                    platform_activation_token(&parameter.try_clone().ok()?.try_into().ok()?)
                });
                shared.activate(id, token);
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

async fn send(portal: &Portal, notice: &Notice, application_action: bool) -> Result<(), String> {
    let body = crate::notifications::context(&notice.event);
    let id = notice.id.to_string();
    let action = if application_action {
        format!("app.{OPEN_ACTION}")
    } else {
        SIGNAL_OPEN_ACTION.to_owned()
    };
    let notification = Notification::new(notice.event.evidence.kind.label())
        .body(body.as_str())
        .default_action(action.as_str())
        .default_action_target(id.as_str());
    tokio::time::timeout(
        PORTAL_TIMEOUT,
        portal.proxy.add_notification(&id, notification),
    )
    .await
    .map_err(|_| "Desktop notification portal timed out".to_owned())?
    .map_err(|error| format!("Desktop notifications unavailable: {error}"))
}

async fn send_test(backend: &Portal) -> TestOutcome {
    let test = Notification::new("cagents notifications work")
        .body("Agent completion and attention alerts appear here.");
    match tokio::time::timeout(
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
    }
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
    fn inbox_reconciliation_coalesces_independently_from_lifecycle_attention() {
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let session = TerminalId::new();
        let lineage = crate::agent_execution::Lineage(session);
        let inbox = Event {
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
            automation_receipts: Mutex::default(),
            wake,
        }));
        let host = crate::host::service::test_host();
        let notice = Notice {
            id: Uuid::new_v4(),
            event: Event {
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
            automation_receipts: Mutex::default(),
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
            automation_receipts: Mutex::default(),
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

    fn value(value: &str) -> OwnedValue {
        ashpd::zvariant::Value::from(value).try_into().unwrap()
    }

    fn shared() -> (Arc<Shared>, mpsc::Receiver<()>) {
        let (wake, receiver) = mpsc::channel(1);
        let shared = Arc::new(Shared {
            pending: Mutex::default(),
            presence: Mutex::default(),
            snapshot: Mutex::default(),
            automation_receipts: Mutex::default(),
            wake,
        });
        (shared, receiver)
    }

    #[test]
    fn application_action_target_names_the_clicked_notice() {
        let id = Uuid::new_v4();
        assert_eq!(
            activated_notice(OPEN_ACTION, &[value(&id.to_string())]),
            Some(id)
        );
        // Backends strip the `app.` prefix before calling ActivateAction.
        assert_eq!(
            activated_notice(&format!("app.{OPEN_ACTION}"), &[value(&id.to_string())]),
            None
        );
        assert_eq!(activated_notice("quit", &[value(&id.to_string())]), None);
        assert_eq!(activated_notice(OPEN_ACTION, &[]), None);
        assert_eq!(
            activated_notice(OPEN_ACTION, &[value("not-a-notice")]),
            None
        );
        let number: OwnedValue = ashpd::zvariant::Value::from(7u32).try_into().unwrap();
        assert_eq!(activated_notice(OPEN_ACTION, &[number]), None);
        let boxed: OwnedValue =
            ashpd::zvariant::Value::Value(Box::new(ashpd::zvariant::Value::from(id.to_string())))
                .try_into()
                .unwrap();
        assert_eq!(activated_notice(OPEN_ACTION, &[boxed]), Some(id));
    }

    #[test]
    fn registration_survives_a_lost_reply_but_not_another_identity() {
        let associated = || Err("Connection already associated with an application ID".to_owned());
        assert_eq!(
            Registration::Untried.settle(Ok(())),
            Registration::Registered
        );
        // Only our own earlier Register call can have associated the
        // connection after its reply was lost.
        assert_eq!(
            Registration::Unknown.settle(associated()),
            Registration::Registered
        );
        // Without that call, the portal chose the identity from elsewhere.
        assert_eq!(
            Registration::Untried.settle(associated()),
            Registration::Refused
        );
        assert_eq!(
            Registration::Untried.settle(Err("App info not found".into())),
            Registration::Refused
        );
        assert!(Registration::Unknown.needs_attempt());
        assert!(!Registration::Refused.needs_attempt());
        assert!(!Registration::Registered.needs_attempt());
        // Owning the name alone is not enough: backends address the portal's
        // application ID, so a refused registration keeps the signal path.
        let unowned = Activation {
            registration: Registration::Registered,
            ..Default::default()
        };
        assert!(!unowned.receives_actions());
    }

    #[test]
    fn a_replaced_portal_forgets_the_registration() {
        let name = |name: &str| zbus::names::OwnedUniqueName::try_from(name).unwrap();
        let registered = || Activation {
            registration: Registration::Registered,
            portal: Some(name(":1.1")),
            ..Default::default()
        };
        let mut activation = registered();
        // The instance we registered with reappearing needs nothing.
        assert!(!activation.portal_changed(Some(name(":1.1"))));
        assert_eq!(activation.registration, Registration::Registered);
        for owner in [None, Some(name(":1.9"))] {
            let mut activation = registered();
            assert!(activation.portal_changed(owner));
            assert_eq!(activation.registration, Registration::Untried);
            assert_eq!(activation.portal, None);
        }
        // A portal that was never reached has no registration to lose, and a
        // refusal is retried with a replacement that may accept it.
        assert!(!Activation::default().portal_changed(None));
        let mut refused = Activation {
            registration: Registration::Refused,
            portal: Some(name(":1.1")),
            ..Default::default()
        };
        assert!(refused.portal_changed(Some(name(":1.2"))));
        assert!(refused.registration.needs_attempt());
    }

    #[test]
    fn a_portal_of_unknown_owner_takes_its_first_change_as_a_replacement() {
        let name = |name: &str| zbus::names::OwnedUniqueName::try_from(name).unwrap();
        let mut activation = Activation {
            registration: Registration::Registered,
            portal_unknown: true,
            ..Default::default()
        };
        assert!(activation.portal_changed(Some(name(":1.9"))));
        assert!(!activation.portal_unknown);
        // Re-registering with the same instance must still read as registered.
        assert_eq!(activation.registration, Registration::Unknown);
        assert_eq!(
            activation
                .registration
                .settle(Err("Connection already associated".into())),
            Registration::Registered
        );
        // With the owner looked up again, only a real change counts.
        activation.portal = Some(name(":1.9"));
        assert!(!activation.portal_changed(Some(name(":1.9"))));
    }

    #[test]
    fn portal_signals_open_notices_through_either_action_name() {
        assert!(signal_opens_notice(SIGNAL_OPEN_ACTION));
        assert!(signal_opens_notice(&format!("app.{OPEN_ACTION}")));
        assert!(!signal_opens_notice(OPEN_ACTION));
        assert!(!signal_opens_notice("app.quit"));
    }

    #[test]
    fn platform_data_yields_a_bounded_activation_token() {
        let data = |entries: &[(&str, &str)]| {
            entries
                .iter()
                .map(|(key, token)| ((*key).to_owned(), value(token)))
                .collect::<std::collections::HashMap<_, _>>()
        };
        assert_eq!(
            platform_activation_token(&data(&[
                ("desktop-startup-id", "x11"),
                ("activation-token", "wayland"),
            ])),
            Some("wayland".into())
        );
        assert_eq!(
            platform_activation_token(&data(&[("desktop-startup-id", "x11")])),
            Some("x11".into())
        );
        assert_eq!(platform_activation_token(&data(&[])), None);
        assert_eq!(
            platform_activation_token(&data(&[("activation-token", "")])),
            None
        );
        let oversized = "t".repeat(MAX_ACTIVATION_TOKEN + 1);
        assert_eq!(
            platform_activation_token(&data(&[("activation-token", &oversized)])),
            None
        );
    }

    #[test]
    fn application_object_path_follows_the_desktop_entry_specification() {
        assert_eq!(
            application_object_path("io.github.cagents.Development"),
            "/io/github/cagents/Development"
        );
        assert_eq!(
            application_object_path("org.example.my-app"),
            "/org/example/my_app"
        );
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
        let mut queue = BTreeSet::new();
        for index in 0..MAX_PENDING + 10 {
            queue_withdrawal(&mut queue, Uuid::from_u128(index as u128));
        }
        assert_eq!(queue.len(), MAX_PENDING);
        let existing = *queue.last().unwrap();
        queue_withdrawal(&mut queue, existing);
        assert_eq!(queue.len(), MAX_PENDING);
    }
    #[test]
    fn committed_automation_transfer_preserves_fifo_and_receipts_through_replay() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.sqlite3");
        let registry = Arc::new(Mutex::new(crate::host::model::Registry::at(&path).unwrap()));
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        let mut journal = journal::Journal::open(&directory.path().join("notices.sqlite3")).unwrap();
        let id = Uuid::new_v4();
        let event = |reason: &str, key: &str| Event {
            target: Target::Automation { id, name: "Fixture".into(), reason: reason.into() },
            source_session: None, provider: None, directory: None, destination: None,
            evidence: crate::notifications::Evidence { key: key.into(), kind: crate::notifications::Kind::AutomationSkipped },
        };
        // Exercise committed scheduler rows, the actual transfer, pending queue,
        // journal and receipt ACK. The newer reason sorts before the old reason.
        let commit = |event: &Event| {
            let mut connection = rusqlite::Connection::open(&path).unwrap();
            let tx = connection.transaction().unwrap();
            tx.execute("DELETE FROM automation_notifications", []).unwrap();
            tx.execute("INSERT INTO automation_notifications(key,event) VALUES(?1,?2)",
                rusqlite::params![event.evidence.key, serde_json::to_string(event).unwrap()]).unwrap();
            tx.commit().unwrap();
        };
        let old = event("Z earlier", "earlier");
        let newer = event("A newest", "newest");
        commit(&old);
        super::super::automations::transfer_notifications(&registry, &handle).unwrap();
        commit(&newer);
        super::super::automations::transfer_notifications(&registry, &handle).unwrap();
        // Repeated flushes coalesce only duplicate evidence, not distinct rows.
        super::super::automations::transfer_notifications(&registry, &handle).unwrap();
        let pending = std::mem::take(&mut *shared.pending.lock().unwrap());
        assert!(pending.events.is_empty());
        assert_eq!(pending.automation_events.iter().map(|event| event.evidence.key.as_str()).collect::<Vec<_>>(), ["earlier", "newest"]);
        for event in pending.automation_events {
            journal_event(&mut journal, &Arc::downgrade(&shared), event, false).unwrap();
        }
        assert_eq!(journal.list().unwrap()[0].event, newer);
        super::super::automations::transfer_notifications(&registry, &handle).unwrap();
        assert!(registry.lock().unwrap().automation_notifications().unwrap().is_empty());
        assert!(shared.pending.lock().unwrap().automation_events.is_empty());

        // Interrupted-after-journal replay emits the missing receipt without
        // replacing the latest content with previously seen evidence.
        commit(&old);
        super::super::automations::transfer_notifications(&registry, &handle).unwrap();
        let pending = std::mem::take(&mut *shared.pending.lock().unwrap());
        for event in pending.automation_events {
            assert!(journal_event(&mut journal, &Arc::downgrade(&shared), event, false).unwrap().is_none());
        }
        assert_eq!(journal.list().unwrap()[0].event, newer);
        super::super::automations::transfer_notifications(&registry, &handle).unwrap();
        assert!(registry.lock().unwrap().automation_notifications().unwrap().is_empty());

        // Identical presentation targets still journal and ACK both distinct
        // evidence keys rather than silently losing the older pending key.
        let first = event("Same metadata", "same-first");
        let last = event("Same metadata", "same-last");
        commit(&first);
        super::super::automations::transfer_notifications(&registry, &handle).unwrap();
        commit(&last);
        super::super::automations::transfer_notifications(&registry, &handle).unwrap();
        let pending = std::mem::take(&mut *shared.pending.lock().unwrap());
        assert_eq!(pending.automation_events.len(), 2);
        for event in pending.automation_events {
            journal_event(&mut journal, &Arc::downgrade(&shared), event, false).unwrap();
        }
        assert_eq!(handle.take_automation_receipts(), BTreeSet::from(["same-first".into(), "same-last".into()]));
        assert_eq!(journal.list().unwrap()[0].event, last);
    }

    #[test]
    fn stopped_and_full_notification_queues_never_fabricate_scheduler_journal_receipts() {
        let event = Event {
            target: Target::Automation {
                id: Uuid::new_v4(),
                name: "Scheduled fixture".into(),
                reason: "Project closed".into(),
            },
            source_session: None,
            provider: None,
            directory: None,
            destination: None,
            evidence: crate::notifications::Evidence {
                key: "automation-fixture".into(),
                kind: crate::notifications::Kind::AutomationPaused,
            },
        };
        let (shared, _receiver) = shared();
        let handle = Handle(shared.clone());
        shared.pending.lock().unwrap().stopped = true;
        handle.record(event.clone());
        assert!(shared.pending.lock().unwrap().events.is_empty());
        assert!(shared.pending.lock().unwrap().automation_events.is_empty());
        assert!(handle.take_automation_receipts().is_empty());
        shared.pending.lock().unwrap().stopped = false;
        for _ in 0..MAX_PENDING {
            let mut other = event.clone();
            other.target = Target::Session(TerminalId::new());
            other.evidence.kind = crate::notifications::Kind::Succeeded;
            handle.record(other);
        }
        handle.record(event.clone());
        let pending = shared.pending.lock().unwrap();
        assert_eq!(pending.events.len(), MAX_PENDING);
        assert!(!pending.events.contains_key(&event.target));
        assert!(pending.automation_events.is_empty());
        drop(pending);
        assert!(handle.take_automation_receipts().is_empty());
    }
}

/// Return scheduler receipts only after SQLite has committed the exact evidence.
/// A duplicate is also a durable receipt: replay must not overwrite newer notice
/// content, and the scheduler may then retire its already-journaled outbox row.
fn journal_event(
    journal: &mut journal::Journal,
    shared: &Weak<Shared>,
    event: Event,
    suppressed: bool,
) -> Result<Option<Notice>, String> {
    let receipt = matches!(
        &event.target,
        Target::Automation { .. } | Target::AutomationProposal { .. }
    ).then(|| event.evidence.key.clone());
    let result = journal.record(event, suppressed);
    if result.is_ok()
        && let Some(key) = receipt
        && let Some(core) = shared.upgrade()
    {
        let mut receipts = core.automation_receipts.lock().unwrap_or_else(|p| p.into_inner());
        if receipts.len() < MAX_PENDING {
            receipts.insert(key);
        }
    }
    result
}

async fn run(
    shared: Weak<Shared>,
    host: Weak<HostLock>,
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
    let mut activation = Activation::default();
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
    match &mut journal {
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
        let mut pending =
            std::mem::take(&mut *core.pending.lock().unwrap_or_else(|p| p.into_inner()));
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
        if let Some(owner) = pending.portal_owner
            && activation.portal_changed(owner)
        {
            // Register with the new portal before anything else calls it, or
            // it identifies this connection without the application ID.
            proxy = None;
            next_attempt = Instant::now();
        }
        let Ok(journal) = &mut journal else {
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
        for event in pending.events.into_values().chain(pending.automation_events) {
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
            let automation = matches!(&event.target, Target::Automation { .. } | Target::AutomationProposal { .. });
            let recorded = journal_event(journal, &shared, event, suppressed);
            if recorded.is_ok()
                && automation
                && let Some(host) = host.upgrade()
            {
                super::automations::wake(&host);
            }
            match recorded {
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
        let backlog_due =
            (!delivery.is_empty() || !withdrawals.is_empty()) && Instant::now() >= next_attempt;
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
                delivery.values().take(8).cloned().collect()
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
