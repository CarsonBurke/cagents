//! Windows mirror attention and report focus; only the daemon delivers alerts.
use super::*;
use crate::{
    control::ControlClient,
    notifications::{Preferences, Presence, Snapshot, Target, TestOutcome, Update},
};
use std::{
    collections::BTreeSet,
    sync::{Mutex, Weak},
    thread,
};

#[derive(Debug, Clone)]
pub enum Action {
    Snapshot(Snapshot),
    Activated(Target),
    OpenNotice(NoticeActivation),
    HostBound(HostSessionProof),
    LifecycleResolved {
        generation: u64,
        result: Result<crate::automations::LifecyclePage, ()>,
    },
    SessionActivationExpired {
        session: TerminalId,
        generation: u64,
    },
    InboxResolved {
        lineage: crate::agent_execution::Lineage,
        operation: uuid::Uuid,
        result: Result<TerminalSession, ()>,
    },
    ReviewsReported(Result<(), String>),
    Test,
    Tested(Result<TestOutcome, String>),
    Failed(String),
}

/// A rendered incident, not permission to replay an arbitrary target.
#[derive(Debug, Clone)]
pub struct NoticeActivation {
    notice: uuid::Uuid,
    target: Target,
    host: Option<HostSessionProof>,
    registry: uuid::Uuid,
    backend: Option<(String, String, String)>,
    workspace: Weak<WorkspaceState>,
}

impl PartialEq for NoticeActivation {
    fn eq(&self, other: &Self) -> bool {
        self.notice == other.notice
            && self.target == other.target
            && self.host == other.host
            && self.registry == other.registry
            && self.backend == other.backend
            && self.workspace.ptr_eq(&other.workspace)
    }
}
impl Eq for NoticeActivation {}

#[derive(Clone)]
struct LifecycleActivation {
    id: uuid::Uuid,
    sequence: u64,
    generation: u64,
    registry: uuid::Uuid,
    host: HostSessionProof,
    backend: Option<(String, String, String)>,
    workspace: Weak<WorkspaceState>,
}

fn lifecycle_backend() -> Option<(String, String, String)> {
    crate::device::selected().map(|device| {
        (
            device.name.clone(),
            device.host.clone(),
            device.executable.clone(),
        )
    })
}

fn read_lifecycle(
    activation: &LifecycleActivation,
) -> Result<crate::automations::LifecyclePage, ()> {
    if lifecycle_backend() != activation.backend {
        return Err(());
    }
    let mut client = connect().map_err(|_| ())?;
    if client.proof() != &activation.host {
        return Err(());
    }
    if client.read_workspace_model().map_err(|_| ())?.identity != activation.registry {
        return Err(());
    }
    // At SQLite's maximum sequence there cannot be a later receipt. The
    // receiver still requires exact equality; it never substitutes the latest.
    let before = activation
        .sequence
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64);
    let page = match client
        .automation(
            crate::agent_execution::Authority::default(),
            crate::automations::Request::Audit {
                id: activation.id,
                before,
                limit: 1,
            },
        )
        .map_err(|_| ())?
    {
        crate::automations::Response::Audit(page) => page,
        _ => return Err(()),
    };
    if client.read_workspace_model().map_err(|_| ())?.identity != activation.registry {
        return Err(());
    }
    Ok(page)
}

struct ManagedActivation {
    agent: crate::agent_execution::Agent,
    display: crate::workspace_model::views::ManagedDisplay,
    session: crate::agent_execution::HostedSession,
}

#[derive(Default)]
struct Pending {
    presence: Option<Presence>,
    preferences: Option<Preferences>,
    acknowledged: BTreeSet<uuid::Uuid>,
}

pub(super) struct Preview {
    pub notice: uuid::Uuid,
    pub rank: usize,
    pub text: String,
    seen: u64,
}

pub(super) struct State {
    pub snapshot: Snapshot,
    /// Derived once per broker snapshot, never searched during row rendering.
    pub previews: HashMap<TerminalId, Preview>,
    preview_generation: u64,
    presence: Arc<Mutex<Presence>>,
    pending: Arc<Mutex<Pending>>,
    wake: Option<crossbeam_channel::Sender<()>>,
    receiver: async_channel::Receiver<Action>,
    last_workspace: Weak<WorkspaceState>,
    /// What the last published presence was derived from, past the durable
    /// workspace: the focused card, window focus, whether the card's pane is
    /// active, and which review a focused pull request pane has open.
    last_focus: Option<(Option<CardId>, bool, bool, Option<u64>)>,
    visible_focus: Option<CardId>,
    visible_notices: BTreeSet<uuid::Uuid>,
    visible_acknowledged: bool,
    acknowledged: BTreeSet<uuid::Uuid>,
    /// Whether a test notification is waiting on the desktop's answer.
    pub testing: bool,
    lifecycle_activation: Option<LifecycleActivation>,
    lifecycle_read: Option<u64>,
    host: Option<HostSessionProof>,
    // HostBound precedes its Snapshot on the one observer FIFO. Retained old
    // snapshots may still inform existing chrome, but cannot authorize a click.
    snapshot_host: Option<HostSessionProof>,
    snapshot_backend: Option<(String, String, String)>,
    inbox_activation: Option<(crate::agent_execution::Lineage, uuid::Uuid)>,
    session_activation: Option<TerminalId>,
    activation_generation: u64,
    activation_attempt: Option<(Weak<WorkspaceState>, bool, uuid::Uuid, u64)>,
    #[cfg(test)]
    activation_attempts: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            snapshot: Snapshot::default(),
            previews: HashMap::new(),
            preview_generation: 0,
            presence: Arc::new(Mutex::new(Presence::default())),
            pending: Arc::new(Mutex::new(Pending::default())),
            wake: None,
            receiver: async_channel::bounded(32).1,
            last_workspace: Weak::new(),
            last_focus: None,
            visible_focus: None,
            visible_notices: BTreeSet::new(),
            visible_acknowledged: false,
            acknowledged: BTreeSet::new(),
            testing: false,
            lifecycle_activation: None,
            lifecycle_read: None,
            host: None,
            snapshot_host: None,
            snapshot_backend: None,
            inbox_activation: None,
            session_activation: None,
            activation_generation: 0,
            activation_attempt: None,
            #[cfg(test)]
            activation_attempts: 0,
        }
    }
}

impl State {
    pub(super) fn scope_changed(&mut self) {
        self.activation_attempt = None;
    }

    fn changed(&self, change: impl FnOnce(&mut Pending)) {
        change(&mut self.pending.lock().unwrap_or_else(|p| p.into_inner()));
        if let Some(wake) = &self.wake {
            let _ = wake.try_send(());
        }
    }

    pub fn preferences(&self, preferences: Preferences) {
        self.changed(|pending| pending.preferences = Some(preferences));
    }

    /// Preferences queued for the daemon and not yet sent.
    #[cfg(test)]
    pub(super) fn pending_preferences(&self) -> Option<Preferences> {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .preferences
    }

    /// Presence queued for the daemon and not yet sent, taken as if sent.
    #[cfg(test)]
    pub(super) fn take_pending_presence_for_test(&self) -> Option<Presence> {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .presence
            .take()
    }

    #[cfg(test)]
    pub(super) fn focused_session_for_test(&self) -> Option<TerminalId> {
        self.presence
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .focused
    }

    #[cfg(test)]
    pub(super) fn acknowledged_for_test(&self, id: uuid::Uuid) -> bool {
        self.acknowledged.contains(&id)
    }

    pub(super) fn acknowledge(&mut self, id: uuid::Uuid) {
        if self.acknowledged.insert(id) {
            // Explicit local reads must retire their excerpt immediately,
            // including while the broker acknowledgement is disconnected.
            self.previews.retain(|_, preview| preview.notice != id);
            self.changed(|pending| {
                pending.acknowledged.insert(id);
            });
        }
    }
}

fn connect() -> Result<ControlClient, String> {
    let socket = crate::terminal_ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    ControlClient::connect(&socket).map_err(|error| error.to_string())
}

/// Resolve only the current, unambiguous live terminal owner. This is a
/// directory/descriptor read on an existing daemon, never an inbox read.
fn resolve_inbox(lineage: crate::agent_execution::Lineage) -> Result<TerminalSession, ()> {
    use crate::agent_execution::{
        Authority, Operation, Reply,
        directory::{Scope, Surface},
    };
    let mut client = connect().map_err(|_| ())?;
    let Reply::Directory(directory) = client
        .agent(Authority::default(), Operation::Directory(Scope::All))
        .map_err(|_| ())?
    else {
        return Err(());
    };
    let mut owners = directory
        .caller
        .iter()
        .chain(&directory.sessions)
        .filter(|entry| entry.lineage == lineage);
    let owner = owners.next().ok_or(())?;
    if owners.next().is_some() || owner.surface != Surface::Terminal || owner.provider.is_none() {
        return Err(());
    }
    let session = client
        .list()
        .map_err(|_| ())?
        .into_iter()
        .find(|descriptor| descriptor.session.id == owner.id)
        .ok_or(())?
        .session;
    session.continuity.live_proof().ok_or(())?;
    Ok(session)
}

fn same_live_terminal(current: &TerminalSession, fresh: &TerminalSession) -> bool {
    current.id == fresh.id
        && current.launch == fresh.launch
        && fresh.continuity.live_proof().is_some()
        && current.continuity.live_proof() == fresh.continuity.live_proof()
}

fn observe(
    sender: &async_channel::Sender<Action>,
    presence: &Mutex<Presence>,
) -> Result<(), String> {
    let mut client = connect()?;
    sender
        .send_blocking(Action::HostBound(client.proof().clone()))
        .map_err(|error| error.to_string())?;
    let presence = presence.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let snapshot = client
        .watch_notifications(presence)
        .map_err(|error| error.to_string())?;
    sender
        .send_blocking(Action::Snapshot(snapshot))
        .map_err(|error| error.to_string())?;
    while !sender.is_closed() {
        let action = match client
            .next_notification()
            .map_err(|error| error.to_string())?
        {
            Update::Changed => Action::Snapshot(
                client
                    .read_notifications()
                    .map_err(|error| error.to_string())?,
            ),
            Update::Activated(session) => Action::Activated(session),
        };
        sender
            .send_blocking(action)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

impl Cagents {
    pub(super) fn start_notifications(&mut self) {
        self.sync_notification_presence();
        let (sender, receiver) = async_channel::bounded(32);
        self.notification_ui.receiver = receiver;
        let presence = self.notification_ui.presence.clone();
        let observer = sender.clone();
        let _ = thread::Builder::new()
            .name("cagents-attention".into())
            .spawn(move || {
                while !observer.is_closed() {
                    if let Err(error) = observe(&observer, &presence) {
                        tracing::debug!(%error, "notification observation disconnected");
                    }
                    thread::sleep(crate::daemon_health::retry_delay());
                }
            });
        let (wake, receiver) = crossbeam_channel::bounded(1);
        self.notification_ui.wake = Some(wake);
        let pending = self.notification_ui.pending.clone();
        let _ = thread::Builder::new()
            .name("cagents-notification-control".into())
            .spawn(move || {
                'control: while receiver.recv().is_ok() {
                    let mut last_error = None;
                    loop {
                        if sender.is_closed() {
                            break 'control;
                        }
                        let requests =
                            std::mem::take(&mut *pending.lock().unwrap_or_else(|p| p.into_inner()));
                        let result = (|| {
                            let mut client = connect()?;
                            if let Some(presence) = requests.presence.clone() {
                                client
                                    .notification_presence(presence)
                                    .map_err(|error| error.to_string())?;
                            }
                            if requests.preferences.is_some() {
                                client
                                    .notification_preferences(requests.preferences)
                                    .map_err(|error| error.to_string())?;
                            }
                            for id in &requests.acknowledged {
                                client
                                    .acknowledge_notification(*id)
                                    .map_err(|error| error.to_string())?;
                            }
                            Ok::<_, String>(())
                        })();
                        if let Err(error) = result {
                            // Every command is idempotent. Retain it across a daemon
                            // restart, but let a newer focus/settings update win.
                            let mut newer = pending.lock().unwrap_or_else(|p| p.into_inner());
                            newer.presence = newer.presence.take().or(requests.presence);
                            newer.preferences = newer.preferences.or(requests.preferences);
                            newer.acknowledged.extend(requests.acknowledged);
                            drop(newer);
                            if last_error.as_ref() != Some(&error) {
                                last_error = Some(error.clone());
                                let _ = sender.send_blocking(Action::Failed(error));
                            }
                            match receiver.recv_timeout(crate::daemon_health::retry_delay()) {
                                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                                    break 'control;
                                }
                                _ => {}
                            }
                        } else {
                            break;
                        }
                    }
                }
            });
        self.notification_ui
            .preferences(self.settings.notifications);
    }

    pub(super) fn acknowledge_card_notices(&mut self, card: CardId) {
        let session = self.card_state(card).and_then(card_session);
        let ids: Vec<_> = self
            .notification_ui
            .snapshot
            .notices
            .iter()
            .filter(|notice| session.is_some() && notice.event.source_session() == session)
            .map(|notice| notice.id)
            .collect();
        for id in ids {
            self.notification_ui.acknowledge(id);
        }
    }

    pub(super) fn sync_visible_attention(&mut self) -> Task<Message> {
        let visible = self
            .workspace
            .focused()
            .filter(|card| self.pane_is_active(*card));
        if self.notification_ui.visible_focus == visible {
            return Task::none();
        }
        self.notification_ui.visible_focus = visible;
        self.notification_ui.visible_acknowledged = false;
        let session = visible
            .and_then(|card| self.card_state(card))
            .and_then(card_session);
        self.notification_ui.visible_notices = self
            .notification_ui
            .snapshot
            .notices
            .iter()
            .filter(|notice| {
                session.is_some()
                    && notice.event.source_session() == session
                    && !notice.acknowledged
                    && !self.notification_ui.acknowledged.contains(&notice.id)
            })
            .map(|notice| notice.id)
            .collect();
        self.attention_focus_epoch = self.attention_focus_epoch.wrapping_add(1);
        let epoch = self.attention_focus_epoch;
        visible.map_or_else(Task::none, |card| {
            cosmic::task::future(async move {
                tokio::time::sleep(Duration::from_millis(400)).await;
                Message::AcknowledgeVisiblePane { card, epoch }
            })
        })
    }

    pub(super) fn acknowledge_visible_pane(&mut self) -> Task<Message> {
        // The timer owns the notices present at its visibility boundary. A
        // newer opted-in alert must survive an older parent's pending dwell.
        self.notification_ui.visible_acknowledged = true;
        let ids = std::mem::take(&mut self.notification_ui.visible_notices);
        for id in ids {
            self.notification_ui.acknowledge(id);
        }
        self.acknowledge_active_pane_with_notices(false)
    }

    pub(super) fn sync_notification_presence(&mut self) {
        let focused_card = self.workspace.focused();
        let qualified = focused_card.is_some_and(|card| self.pane_is_active(card));
        // A pane can open another review without the durable workspace
        // changing, and the review it shows names the alert it focuses.
        let review = focused_card.and_then(|card| match self.cards.get(&card) {
            Some(CardRuntime::PullRequests(pane)) => Some(pane.review.selection_generation()),
            _ => None,
        });
        let focus = (focused_card, self.refresh_ui.focused, qualified, review);
        if self.notification_ui.last_focus == Some(focus)
            && self.notification_ui.last_workspace.as_ptr() == Arc::as_ptr(&self.durable)
        {
            return;
        }
        self.notification_ui.last_focus = Some(focus);
        self.notification_ui.last_workspace = Arc::downgrade(&self.durable);
        let mut sessions = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .filter_map(card_session)
            .collect::<Vec<_>>();
        sessions.sort_unstable();
        sessions.dedup();
        let focused = focus
            .2
            .then_some(focus.0)
            .flatten()
            .and_then(|id| self.card_state(id))
            .filter(|card| self.pane_is_active(card.id))
            .and_then(card_session);
        let focused_target = focus.2.then_some(focus.0).flatten().and_then(|id| {
            let CardRuntime::PullRequests(pane) = self.cards.get(&id)? else {
                return None;
            };
            let target = pane.review.target()?;
            self.refresh_ui
                .review_requests
                .target(&target.repository, target.pull_request.number)
        });
        let presence = Presence {
            window: instance::window_slot().into(),
            application: Default::default(),
            focused_target,
            sessions,
            focused,
        };
        let focus_changed = {
            let mut previous = self
                .notification_ui
                .presence
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let changed =
                previous.focused != focused || previous.focused_target != presence.focused_target;
            *previous = presence.clone();
            changed
        };
        let mut read = crate::notifications::focused_notice_ids(
            &self.notification_ui.snapshot,
            presence.focused_target.as_ref(),
            None,
            focus_changed,
            self.settings.notifications.while_focused,
        );
        let session_read = crate::notifications::focused_notice_ids(
            &self.notification_ui.snapshot,
            None,
            focused,
            focus_changed,
            self.settings.notifications.while_focused,
        );
        // Delivery presence changes immediately, but implicit Session reads on
        // a visibility return share the sidebar's qualified dwell. Preserve
        // the existing policy for fresh alerts while the parent stays visible.
        if focused_card == self.notification_ui.visible_focus {
            read.extend(session_read.into_iter().filter(|id| {
                self.notification_ui.visible_acknowledged
                    || (!focus_changed && !self.notification_ui.visible_notices.contains(id))
            }));
        }
        self.notification_ui
            .changed(|pending| pending.presence = Some(presence));
        for id in read {
            self.notification_ui.acknowledge(id);
        }
    }

    pub(super) fn review_alert_focus_changed(&mut self) {
        self.notification_ui.last_focus = None;
        self.sync_notification_presence();
    }

    /// Stable notice IDs are rendered once. Conflicting coalesced targets for
    /// one ID fail closed rather than choosing whichever row arrived last.
    pub(super) fn attention_notices(&self) -> Vec<&crate::notifications::Notice> {
        let mut seen = HashMap::new();
        let mut conflicts = BTreeSet::new();
        for notice in &self.notification_ui.snapshot.notices {
            if let Some(target) =
                seen.insert(notice.id, (&notice.event.target, notice.acknowledged))
                && target != (&notice.event.target, notice.acknowledged)
            {
                conflicts.insert(notice.id);
            }
        }
        let mut ids = BTreeSet::new();
        self.notification_ui
            .snapshot
            .notices
            .iter()
            .filter(|notice| {
                !notice.acknowledged
                    && !self.notification_ui.acknowledged.contains(&notice.id)
                    && !conflicts.contains(&notice.id)
                    && ids.insert(notice.id)
            })
            .collect()
    }

    pub(super) fn attention_backend_current(&self) -> bool {
        self.workspace_model.identity != crate::workspace_model::NO_REGISTRY
            && self
                .notification_ui
                .snapshot_host
                .as_ref()
                .is_some_and(|proof| proof.is_complete())
            && self.notification_ui.snapshot_host == self.notification_ui.host
            && self.notification_ui.snapshot_backend == lifecycle_backend()
    }

    pub(super) fn attention_notice_owner(
        &self,
        notice: &crate::notifications::Notice,
    ) -> NoticeActivation {
        NoticeActivation {
            notice: notice.id,
            target: notice.event.target.clone(),
            host: self.notification_ui.snapshot_host.clone(),
            registry: self.workspace_model.identity,
            backend: self.notification_ui.snapshot_backend.clone(),
            workspace: Arc::downgrade(&self.durable),
        }
    }

    pub(super) fn attention_notice_action(&self, notice: &crate::notifications::Notice) -> Message {
        Message::NotificationUi(Action::OpenNotice(self.attention_notice_owner(notice)))
    }

    fn can_open_lifecycle_notice(&self) -> bool {
        !self.workspace_inputs_blocked()
            && !self.focused_repository_is_saving()
            && self.exit.is_none()
            && self.workspace.pending_close().is_none()
            && !self.update_prepared
            && self.automation_ui.can_open_lifecycle()
    }

    pub(super) fn update_notifications(&mut self, action: Action) -> Task<Message> {
        if matches!(&action, Action::Activated(target) if !matches!(target, Target::SessionInbox(_)))
        {
            self.notification_ui.inbox_activation = None;
        }
        if matches!(&action, Action::Activated(_)) {
            self.notification_ui.lifecycle_activation = None;
            self.notification_ui.session_activation = None;
            self.notification_ui.activation_attempt = None;
            self.notification_ui.activation_generation =
                self.notification_ui.activation_generation.wrapping_add(1);
        }
        match action {
            Action::OpenNotice(clicked) => {
                let current = self.attention_notices().into_iter().any(|notice| {
                    notice.id == clicked.notice && notice.event.target == clicked.target
                });
                if !self.attention_is_open()
                    || !current
                    || clicked.notice.is_nil()
                    || clicked.registry == crate::workspace_model::NO_REGISTRY
                    || clicked.registry != self.workspace_model.identity
                    || clicked.backend != lifecycle_backend()
                    || clicked.backend != self.notification_ui.snapshot_backend
                    || clicked.workspace.as_ptr() != Arc::as_ptr(&self.durable)
                    || clicked
                        .host
                        .as_ref()
                        .is_none_or(|proof| !proof.is_complete())
                    || clicked.host != self.notification_ui.host
                    || clicked.host != self.notification_ui.snapshot_host
                {
                    self.status = Some(TransientNotice::warning(
                        "This attention entry changed or its backend is unavailable. Open Attention again to choose a current entry.",
                    ));
                    return Task::none();
                }
                if !self.with_attention_dismissed(|app| app.can_open_lifecycle_notice())
                    || (matches!(clicked.target, Target::Automation { .. })
                        && self.notification_ui.lifecycle_read.is_some())
                {
                    self.status = Some(TransientNotice::warning(
                        "Finish the current edit or request and return to the workspace, then choose this attention entry again.",
                    ));
                    return Task::none();
                }
                // Merely listing/reading Attention never acknowledges a notice.
                // Explicit session actions keep their established semantics;
                // historical automation activation remains read-only/no-ACK.
                self.close_attention();
                self.update_notifications(Action::Activated(clicked.target))
            }
            Action::HostBound(proof) => {
                self.notification_ui.snapshot_host = None;
                if self.notification_ui.host.as_ref() != Some(&proof) {
                    self.notification_ui.lifecycle_activation = None;
                }
                self.notification_ui.host = Some(proof);
                Task::none()
            }
            Action::Activated(Target::Automation { id, sequence }) => {
                // Invalidating the desired target does not cancel blocking I/O.
                // Keep one real read outstanding, including across host changes.
                if self.notification_ui.lifecycle_read.is_some() {
                    self.status = Some(TransientNotice::warning(
                        "A lifecycle receipt is still being read. Open this notice again after that read finishes.",
                    ));
                    return self.activate_main_window(None);
                }
                if !self.can_open_lifecycle_notice() {
                    self.status = Some(TransientNotice::warning(
                        "Return to the workspace and finish any open edit or request, then open this lifecycle notice again.",
                    ));
                    return self.activate_main_window(None);
                }
                let Some(host) = self
                    .notification_ui
                    .host
                    .clone()
                    .filter(|proof| proof.is_complete())
                else {
                    self.status = Some(TransientNotice::warning(
                        "The notification backend is not connected. The original notice remains in Attention.",
                    ));
                    return self.activate_main_window(None);
                };
                if id.is_nil()
                    || sequence == 0
                    || sequence > i64::MAX as u64
                    || self.workspace_model.identity == crate::workspace_model::NO_REGISTRY
                {
                    self.status = Some(TransientNotice::warning(
                        "This lifecycle notice is not available on the current workspace. The original notice remains in Attention.",
                    ));
                    return self.activate_main_window(None);
                }
                let activation = LifecycleActivation {
                    id,
                    sequence,
                    generation: self.notification_ui.activation_generation,
                    registry: self.workspace_model.identity,
                    host,
                    backend: lifecycle_backend(),
                    workspace: Arc::downgrade(&self.durable),
                };
                self.notification_ui.lifecycle_read = Some(activation.generation);
                self.notification_ui.lifecycle_activation = Some(activation.clone());
                cosmic::task::future(async move {
                    let generation = activation.generation;
                    let result = tokio::task::spawn_blocking(move || read_lifecycle(&activation))
                        .await
                        .unwrap_or(Err(()));
                    Message::NotificationUi(Action::LifecycleResolved { generation, result })
                })
            }
            Action::LifecycleResolved { generation, result } => {
                if self.notification_ui.lifecycle_read != Some(generation) {
                    return Task::none();
                }
                self.notification_ui.lifecycle_read = None;
                let Some(activation) = self
                    .notification_ui
                    .lifecycle_activation
                    .as_ref()
                    .filter(|current| current.generation == generation)
                    .cloned()
                else {
                    return Task::none();
                };
                self.notification_ui.lifecycle_activation = None;
                if activation.registry != self.workspace_model.identity
                    || self.notification_ui.host.as_ref() != Some(&activation.host)
                    || lifecycle_backend() != activation.backend
                    || activation.workspace.as_ptr() != Arc::as_ptr(&self.durable)
                    || !self.can_open_lifecycle_notice()
                {
                    return Task::none();
                }
                if !self.automation_ui.show_lifecycle_receipt(
                    activation.id,
                    activation.sequence,
                    result,
                ) {
                    return Task::none();
                }
                // Reading a historical receipt is not run attention retirement
                // or a claim that this system condition still holds now.
                let open = self.open_utility_card(CardKind::Automations);
                Task::batch([open, self.activate_main_window(None)])
            }
            Action::Snapshot(snapshot) => {
                self.notification_ui.snapshot_host = self.notification_ui.host.clone();
                self.notification_ui.snapshot_backend = lifecycle_backend();
                if snapshot.diagnostic != self.notification_ui.snapshot.diagnostic
                    && let Some(diagnostic) = &snapshot.diagnostic
                {
                    self.status = Some(TransientNotice::warning(diagnostic.clone()));
                }
                self.notification_ui.acknowledged.retain(|id| {
                    snapshot
                        .notices
                        .iter()
                        .any(|notice| notice.id == *id && !notice.acknowledged)
                });
                let (unread, failed) = self.notification_attention_updates(&snapshot);
                self.sidebar_state.set_failed_updates(failed);
                for card in unread {
                    self.observe_agent_update_for_card(card);
                }
                self.notification_ui.preview_generation =
                    self.notification_ui.preview_generation.wrapping_add(1);
                let generation = self.notification_ui.preview_generation;
                for (rank, notice) in snapshot.notices.iter().enumerate() {
                    if !notice.acknowledged
                        && notice.event.content.is_some()
                        && !self.notification_ui.acknowledged.contains(&notice.id)
                        && let Some(session) = notice.event.source_session()
                    {
                        if let Some(cached) = self.notification_ui.previews.get_mut(&session) {
                            if cached.seen == generation {
                                continue;
                            }
                            if cached.notice == notice.id {
                                cached.rank = rank;
                                cached.seen = generation;
                                continue;
                            }
                        }
                        let message =
                            crate::notifications::presentation::message(&notice.event, true);
                        self.notification_ui.previews.insert(
                            session,
                            Preview {
                                notice: notice.id,
                                rank,
                                text: format!("{}\n{}", message.title, message.body),
                                seen: generation,
                            },
                        );
                    }
                }
                self.notification_ui
                    .previews
                    .retain(|_, cached| cached.seen == generation);
                self.notification_ui.snapshot = snapshot;
                self.refresh_sidebar_channels();
                self.notification_ui.last_focus = None;
                self.sync_notification_presence();
                Task::none()
            }
            Action::ReviewsReported(result) => {
                if let Err(error) = result {
                    self.status = Some(TransientNotice::warning(format!("Review alerts: {error}")));
                }
                Task::none()
            }
            Action::Activated(Target::PullRequest(request)) => {
                // Desktop alerts bring the application forward. Opening a review
                // remains an explicit action in the Pull requests UI.
                let target = Target::PullRequest(request);
                let ids = self
                    .notification_ui
                    .snapshot
                    .notices
                    .iter()
                    .filter(|notice| notice.event.target == target)
                    .map(|notice| notice.id)
                    .collect::<Vec<_>>();
                for id in ids {
                    self.notification_ui.acknowledge(id);
                }
                self.activate_main_window(None)
            }
            Action::Activated(Target::SessionInbox(lineage)) => {
                let operation = uuid::Uuid::new_v4();
                self.notification_ui.inbox_activation = Some((lineage, operation));
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || resolve_inbox(lineage))
                        .await
                        .unwrap_or(Err(()));
                    Message::NotificationUi(Action::InboxResolved {
                        lineage,
                        operation,
                        result,
                    })
                })
            }
            Action::InboxResolved {
                lineage,
                operation,
                result,
            } => {
                if self.notification_ui.inbox_activation != Some((lineage, operation)) {
                    return Task::none();
                }
                self.notification_ui.inbox_activation = None;
                let session = result.ok().filter(|fresh| self.durable.cards.iter().any(|card| {
                    card.existence.is_open() && matches!(&card.kind, CardKind::Terminal(current) if same_live_terminal(current, fresh))
                }));
                let Some(session) = session else {
                    self.status = Some(TransientNotice::warning(
                        "The recipient terminal is no longer available here. Messages remain unread.",
                    ));
                    return self.activate_main_window(None);
                };
                self.status = Some(TransientNotice::warning(
                    "Messages remain unread. Run cagents inbox in the recipient terminal.",
                ));
                self.update_notifications(Action::Activated(Target::Session(session.id)))
            }
            Action::Activated(Target::Session(session)) => {
                // Inventory/model adoption may arrive after the activation watch.
                self.notification_ui.session_activation = Some(session);
                let generation = self.notification_ui.activation_generation;
                let focus = self.retry_notification_activation();
                let expiry = if self.notification_ui.session_activation.is_some() {
                    cosmic::task::future(async move {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        Message::NotificationUi(Action::SessionActivationExpired {
                            session,
                            generation,
                        })
                    })
                } else {
                    Task::none()
                };
                Task::batch([focus, expiry, self.activate_main_window(None)])
            }
            Action::SessionActivationExpired {
                session,
                generation,
            } => {
                if self.notification_ui.session_activation == Some(session)
                    && self.notification_ui.activation_generation == generation
                    && self.resolved_activation_agent(session).is_none()
                    && !self
                        .durable
                        .cards
                        .iter()
                        .any(|card| card.existence.is_open() && card_session(card) == Some(session))
                {
                    self.notification_ui.session_activation = None;
                    self.notification_ui.activation_attempt = None;
                    self.status = Some(TransientNotice::warning(
                        "This agent's pane is not available. Its attention record remains available.",
                    ));
                }
                Task::none()
            }
            Action::Activated(Target::AgentRun(run)) => {
                if self.modal_is_open() || !self.automation_ui.can_open_run() {
                    self.status = Some(TransientNotice::warning(
                        "Finish the open automation edit or request, then open the run notification again.",
                    ));
                    return self.activate_main_window(None);
                }
                let open = self.open_utility_card(CardKind::Automations);
                let read = self.update_automation_ui(crate::automation_ui::Action::InspectRun(run));
                Task::batch([open, read, self.activate_main_window(None)])
            }
            Action::Test => {
                if std::mem::replace(&mut self.notification_ui.testing, true) {
                    return Task::none();
                }
                // Its own connection, so the answer is not queued behind the
                // presence and acknowledgement traffic of the control thread.
                cosmic::task::future(async {
                    let outcome = tokio::task::spawn_blocking(|| {
                        connect()?
                            .send_test_notification()
                            .map_err(|error| error.to_string())
                    })
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()));
                    Message::NotificationUi(Action::Tested(outcome))
                })
            }
            Action::Tested(outcome) => {
                self.notification_ui.testing = false;
                self.status = Some(test_notice(outcome));
                Task::none()
            }
            Action::Failed(error) => {
                self.status = Some(TransientNotice::warning(format!(
                    "Notification service unavailable: {error}"
                )));
                Task::none()
            }
        }
    }

    pub(super) fn retry_notification_activation(&mut self) -> Task<Message> {
        let Some(session) = self.notification_ui.session_activation else {
            return Task::none();
        };
        let blocked = self.exit.is_some()
            || self.workspace_inputs_blocked()
            || self.focused_repository_is_saving()
            || self.update_prepared;
        if self
            .notification_ui
            .activation_attempt
            .as_ref()
            .is_some_and(|(workspace, was_blocked, identity, sequence)| {
                workspace.as_ptr() == Arc::as_ptr(&self.durable)
                    && *was_blocked == blocked
                    && *identity == self.workspace_model.identity
                    && *sequence == self.workspace_model.sequence
            })
        {
            return Task::none();
        }
        self.notification_ui.activation_attempt = Some((
            Arc::downgrade(&self.durable),
            blocked,
            self.workspace_model.identity,
            self.workspace_model.sequence,
        ));
        if blocked {
            return Task::none();
        }
        #[cfg(test)]
        {
            self.notification_ui.activation_attempts += 1;
        }
        // A scheduled agent can remain headless in this window. Only this
        // explicit user activation may mount its exact authenticated descriptor.
        let managed = self.resolved_activation_agent(session);
        if managed.is_none()
            && self.workspace_model.agents.iter().any(|agent| {
                !agent.closed
                    && (agent
                        .session
                        .as_ref()
                        .is_some_and(|owner| owner.id() == session)
                        || agent
                            .console
                            .as_ref()
                            .is_some_and(|owner| owner.id == session))
            })
        {
            // Invalid authoritative membership also fences a retained local
            // projection. It cannot silently downgrade to an unmanaged pane.
            return Task::none();
        }
        let existing = if let Some(activation) = &managed {
            let canonical = CardId::from_uuid(activation.agent.id.as_uuid());
            self.durable.cards.iter().find(|card| card.id == canonical)
        } else {
            // Inventory-only aliases have no model AgentId yet. An open exact
            // session is usable; an older closed alias must not shadow it.
            self.durable
                .cards
                .iter()
                .find(|card| card.existence.is_open() && card_session(card) == Some(session))
                .or_else(|| {
                    self.durable
                        .cards
                        .iter()
                        .find(|card| card_session(card) == Some(session))
                })
        };
        if existing.is_some_and(|card| !card.existence.is_open()) {
            self.notification_ui.session_activation = None;
            self.notification_ui.activation_attempt = None;
            return Task::none();
        }
        let mut adopted = Task::none();
        let id = if let Some(card) = existing {
            if let Some(activation) = &managed {
                let expected = &activation.session;
                let current = match &card.kind {
                    CardKind::Chat(session) => {
                        crate::agent_execution::HostedSession::Chat(session.clone())
                    }
                    CardKind::Terminal(session) => {
                        crate::agent_execution::HostedSession::Terminal(session.clone())
                    }
                    _ => unreachable!("session card"),
                };
                if !expected.same_process(&current) {
                    return Task::none();
                }
            }
            let id = card.id;
            if let Some(activation) = &managed {
                adopted = self
                    .adopt_execution_agent_activation_display(
                        &activation.agent,
                        activation.display.clone(),
                        activation.session.clone(),
                    )
                    .unwrap_or_else(Task::none);
                self.refresh_sidebar_channels();
                self.persist();
            }
            id
        } else {
            let Some(activation) = managed else {
                return Task::none();
            };
            // General managed adoption belongs to the model/inventory path.
            // Only explicit automation activation may mount a headless actor.
            if !matches!(
                activation.agent.origin,
                crate::agent_execution::Origin::Automation { .. }
            ) {
                return Task::none();
            }
            let Some(task) = self.adopt_execution_agent_activation_display(
                &activation.agent,
                activation.display,
                activation.session,
            ) else {
                return Task::none();
            };
            adopted = task;
            self.refresh_sidebar_channels();
            self.persist();
            CardId::from_uuid(activation.agent.id.as_uuid())
        };
        if self
            .durable
            .channels
            .iter()
            .find(|channel| channel.cards.contains(&id))
            .is_some_and(|channel| self.sidebar_model.project(channel.project_id).is_none())
        {
            return Task::none();
        }
        let focus = self.focus_attention_card(id);
        if self.workspace.focused() == Some(id) {
            self.notification_ui.session_activation = None;
            self.notification_ui.activation_attempt = None;
        }
        Task::batch([adopted, focus])
    }

    fn notification_attention_updates(
        &self,
        snapshot: &Snapshot,
    ) -> (BTreeSet<CardId>, BTreeSet<CardId>) {
        let mut unread = BTreeSet::new();
        let mut failed = BTreeSet::new();
        if snapshot.notices.is_empty() {
            return (unread, failed);
        }
        let cards = self.notification_attention_cards();
        for notice in &snapshot.notices {
            if notice.acknowledged || self.notification_ui.acknowledged.contains(&notice.id) {
                continue;
            }
            let Some(card) = notice
                .event
                .source_session()
                .and_then(|session| cards.get(&session).copied().flatten())
            else {
                continue;
            };
            unread.insert(card);
            if notice.event.evidence.kind == crate::notifications::Kind::Failed {
                failed.insert(card);
            }
        }
        (unread, failed)
    }

    /// Journal and workspace-model watches are independent. Reconsider retained
    /// unread evidence after canonical pane/scope reconciliation, without
    /// waiting for a second journal snapshot or adopting/reading a pane.
    pub(super) fn reproject_notification_unread(&mut self) {
        let (cards, failed) = self.notification_attention_updates(&self.notification_ui.snapshot);
        let failed_changed = self.sidebar_state.set_failed_updates(failed);
        if cards.is_empty() && !failed_changed {
            return;
        }
        for card in cards {
            self.observe_agent_update_for_card(card);
        }
        self.refresh_sidebar_channels();
    }

    /// Attribution grants no adoption, focus, startup, persistence or read.
    /// Local inventory-only sessions must be unique. Any raw managed member
    /// fences that fallback, including rejected/ambiguous canonical ownership.
    fn notification_attention_cards(&self) -> HashMap<TerminalId, Option<CardId>> {
        let mut cards = HashMap::new();
        for card in self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
        {
            if let Some(session) = card_session(card) {
                cards
                    .entry(session)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(card.id));
            }
        }
        let mut owners = HashMap::new();
        for agent in self
            .workspace_model
            .agents
            .iter()
            .filter(|agent| !agent.closed)
        {
            let primary = agent.session.as_ref().map(|session| session.id());
            let console = agent.console.as_ref().map(|session| session.id);
            for session in primary
                .into_iter()
                .chain(console.filter(|session| Some(*session) != primary))
            {
                owners
                    .entry(session)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(agent));
            }
        }
        let index = crate::workspace_model::views::ManagedDisplayIndex::new(&self.workspace_model);
        for (session, owner) in owners {
            let canonical = owner.and_then(|agent| {
                let resolved = self.resolved_agent_display(agent, session, &index)?;
                let canonical = CardId::from_uuid(agent.id.as_uuid());
                let card = self
                    .card_state(canonical)
                    .filter(|card| card.existence.is_open())?;
                let local = match &card.kind {
                    CardKind::Chat(session) => {
                        crate::agent_execution::HostedSession::Chat(session.clone())
                    }
                    CardKind::Terminal(session) => {
                        crate::agent_execution::HostedSession::Terminal(session.clone())
                    }
                    _ => return None,
                };
                resolved.session.same_process(&local).then_some(canonical)
            });
            cards.insert(session, canonical);
        }
        cards
    }

    fn resolved_activation_agent(&self, session: TerminalId) -> Option<ManagedActivation> {
        if self.workspace_model.identity == crate::workspace_model::NO_REGISTRY {
            return None;
        }
        // Count raw owners before projection. A rejected duplicate must not
        // disappear from the ambiguity fence merely because one pane matches.
        let mut owners = self.workspace_model.agents.iter().filter(|agent| {
            !agent.closed
                && (agent
                    .session
                    .as_ref()
                    .is_some_and(|owner| owner.id() == session)
                    || agent
                        .console
                        .as_ref()
                        .is_some_and(|owner| owner.id == session))
        });
        let agent = owners.next()?;
        if owners.next().is_some() {
            return None;
        }
        let index = crate::workspace_model::views::ManagedDisplayIndex::new(&self.workspace_model);
        self.resolved_agent_display(agent, session, &index)
    }

    fn resolved_agent_display(
        &self,
        agent: &crate::agent_execution::Agent,
        session: TerminalId,
        index: &crate::workspace_model::views::ManagedDisplayIndex<'_>,
    ) -> Option<ManagedActivation> {
        use crate::agent_execution::Placement;
        if self.workspace_model.identity == crate::workspace_model::NO_REGISTRY {
            return None;
        }
        let resolved = index.activation_display(agent, session)?;
        let display = resolved.display;
        let owner = resolved.session;
        let repository = match &display.scope {
            Placement::Channel(id) => {
                let shared = self.workspace_model.channel(*id)?;
                let local = self
                    .durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == *id)?;
                let project = self.sidebar_model.project(local.project_id)?;
                if project.repository != shared.repository {
                    return None;
                }
                &shared.repository
            }
            Placement::Worktree { repository, path } => {
                if self.worktree_view_scope(path).as_ref() != Some(&display.scope) {
                    return None;
                }
                repository
            }
        };
        // A saved exact pane may have a legacy cross-project display move.
        // Unpublished fallback still belongs to its accepted launch repository.
        if (display.pane.is_none() && repository != &agent.spec.target.repository)
            || self.project_ui.closed.contains(repository)
            || self.workspace_model.closed_projects.contains(repository)
        {
            return None;
        }
        Some(ManagedActivation {
            agent: agent.clone(),
            display,
            session: owner,
        })
    }

    pub(super) fn notification_subscription(&self) -> Subscription<Message> {
        Subscription::run_with(Source(self.notification_ui.receiver.clone()), stream)
            .map(Message::NotificationUi)
    }
}

#[cfg(test)]
fn activation_session(
    agent: &crate::agent_execution::Agent,
) -> Option<crate::agent_execution::HostedSession> {
    if agent.spec.surface == crate::agent_execution::Surface::Terminal {
        agent
            .console
            .clone()
            .map(crate::agent_execution::HostedSession::Terminal)
    } else {
        agent.session.clone()
    }
}

fn test_notice(outcome: Result<TestOutcome, String>) -> TransientNotice {
    match outcome {
        Ok(TestOutcome::Sent) => TransientNotice::success(
            "Test notification sent. Your desktop decides how it looks and whether it plays a sound.",
        ),
        Ok(TestOutcome::Unavailable(error)) => TransientNotice::error(format!(
            "Your desktop's notification service is unavailable: {error}"
        )),
        Ok(TestOutcome::TimedOut) => {
            TransientNotice::error("Your desktop's notification service did not answer in time.")
        }
        Err(error) => TransientNotice::error(format!(
            "Could not reach the cagents daemon to send a test notification: {error}"
        )),
    }
}

pub(super) fn card_session(card: &CardState) -> Option<TerminalId> {
    match &card.kind {
        CardKind::Terminal(session) => Some(session.id),
        CardKind::Chat(session) => Some(session.id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    mod authority;

    use super::*;
    use crate::agent::Provider;

    #[test]
    fn notification_preview_projection_reuses_text_and_retires_acknowledged_context() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let session = TerminalId::new();
        let mut event = crate::notifications::Event::session(
            session,
            crate::agent::Provider::Codex,
            "/repo".into(),
            crate::notifications::Evidence {
                key: "response".into(),
                kind: crate::notifications::Kind::Succeeded,
            },
        );
        event.content = Some(crate::notifications::Content {
            conversation: "Example".into(),
            preview: "Ready to review.".into(),
            request: None,
        });
        let mut notice = crate::notifications::Notice {
            id: uuid::Uuid::new_v4(),
            event,
            delivered: true,
            acknowledged: false,
            window: None,
        };
        let snapshot = |notice| Snapshot {
            notices: vec![notice],
            diagnostic: None,
        };
        drop(app.update_notifications(Action::Snapshot(snapshot(notice.clone()))));
        let allocation = app.notification_ui.previews[&session].text.as_ptr();
        drop(app.update_notifications(Action::Snapshot(snapshot(notice.clone()))));
        assert_eq!(
            app.notification_ui.previews[&session].text.as_ptr(),
            allocation,
            "unchanged journal records must not reformat and reallocate every snapshot"
        );
        notice.acknowledged = true;
        drop(app.update_notifications(Action::Snapshot(snapshot(notice))));
        assert!(!app.notification_ui.previews.contains_key(&session));
    }

    #[test]
    fn notification_channel_preview_moves_to_unread_card_before_broker_reply() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.refresh_ui.focused = false;
        app.settings.notifications.message_previews = true;
        let first = app.durable.cards[0].clone();
        let second = crate::app::tests::terminal_card_state();
        let sessions = [
            card_session(&first).unwrap(),
            card_session(&second).unwrap(),
        ];
        let project = ProjectId::new();
        let channel = ChannelId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "Fixture".into(),
            "/repo".into(),
            Vec::new(),
            Vec::new(),
        ));
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards.push(second.clone());
        durable.channels.push(ProjectChannel {
            title_source: Default::default(),
            id: channel,
            project_id: project,
            title: "Shared channel".into(),
            ordinal: 0,
            shared: false,
            cards: vec![first.id, second.id],
            focused: Some(first.id),
        });
        let notices = sessions
            .into_iter()
            .zip(["First unread result", "Second unread result"])
            .map(|(session, text)| {
                let mut event = crate::notifications::Event::session(
                    session,
                    Provider::Codex,
                    "/repo".into(),
                    crate::notifications::Evidence {
                        key: text.into(),
                        kind: crate::notifications::Kind::Succeeded,
                    },
                );
                event.content = Some(crate::notifications::Content {
                    conversation: "Fixture conversation".into(),
                    preview: text.into(),
                    request: None,
                });
                crate::notifications::Notice {
                    id: uuid::Uuid::new_v4(),
                    event,
                    delivered: true,
                    acknowledged: false,
                    window: None,
                }
            })
            .collect::<Vec<_>>();
        let snapshot = Snapshot {
            notices,
            diagnostic: None,
        };
        drop(app.update_notifications(Action::Snapshot(snapshot.clone())));
        assert!(
            app.sidebar_model.projects[0].channels[0]
                .tooltip_for_test()
                .contains("First unread result")
        );
        drop(app.mark_cards_read(vec![first.id]));
        assert!(!app.sidebar_state.has_card_update(first.id));
        assert!(app.sidebar_state.has_card_update(second.id));
        assert!(!app.notification_ui.previews.contains_key(&sessions[0]));
        let row = &app.sidebar_model.projects[0].channels[0];
        assert!(row.tooltip_for_test().contains("Second unread result"));
        assert!(!row.tooltip_for_test().contains("First unread result"));
        // An older broker snapshot can arrive before the queued ACK commits.
        drop(app.update_notifications(Action::Snapshot(snapshot)));
        assert!(!app.sidebar_state.has_card_update(first.id));
        assert!(app.sidebar_state.has_card_update(second.id));
        let row = &app.sidebar_model.projects[0].channels[0];
        assert!(row.tooltip_for_test().contains("Second unread result"));
        assert!(!row.tooltip_for_test().contains("First unread result"));
    }

    fn live_session(app: &mut Cagents) -> TerminalSession {
        let CardKind::Terminal(session) = &mut Arc::make_mut(&mut app.durable).cards[0].kind else {
            panic!("terminal fixture");
        };
        session.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("host"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("incarnation"),
        };
        session.clone()
    }

    #[test]
    fn inbox_activation_ignores_prior_operations_and_changed_runtime_proofs() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let fresh = live_session(&mut app);
        let lineage = crate::agent_execution::Lineage(fresh.id);
        let old = uuid::Uuid::new_v4();
        let current = uuid::Uuid::new_v4();
        app.notification_ui.inbox_activation = Some((lineage, current));
        let cards = app.durable.cards.clone();
        let focus = app.workspace.focused();
        drop(app.update_notifications(Action::InboxResolved {
            lineage,
            operation: old,
            result: Ok(fresh.clone()),
        }));
        assert_eq!(
            app.notification_ui.inbox_activation,
            Some((lineage, current))
        );
        let mut replaced = fresh;
        replaced.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("replacement"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("incarnation"),
        };
        drop(app.update_notifications(Action::InboxResolved {
            lineage,
            operation: current,
            result: Ok(replaced),
        }));
        assert!(app.notification_ui.inbox_activation.is_none());
        assert_eq!(app.durable.cards, cards);
        assert_eq!(app.workspace.focused(), focus);
        assert!(
            app.status
                .as_ref()
                .unwrap()
                .message
                .contains("Messages remain unread")
        );
    }

    #[test]
    fn inbox_activation_keeps_saved_cards_and_message_acknowledgement_separate() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let fresh = live_session(&mut app);
        let lineage = crate::agent_execution::Lineage(fresh.id);
        let operation = uuid::Uuid::new_v4();
        app.notification_ui.inbox_activation = Some((lineage, operation));
        let mut cards = app.durable.cards.clone();
        drop(app.update_notifications(Action::InboxResolved {
            lineage,
            operation,
            result: Ok(fresh.clone()),
        }));
        assert!(app.notification_ui.inbox_activation.is_none());
        assert_eq!(app.workspace.focused(), Some(cards[0].id));
        assert_eq!(app.durable.focused, Some(cards[0].id));
        // The fixture has not received a native viewport resize. Activation
        // persists its current projected exposure, including overscan, while
        // every other saved field and the live-session proof must survive.
        cards[0].exposure = app
            .workspace
            .visibility(cards[0].id)
            .expect("activated recipient remains in the workspace")
            .into();
        assert_eq!(app.durable.cards, cards);
        assert!(
            app.notification_ui
                .pending
                .lock()
                .unwrap()
                .acknowledged
                .is_empty()
        );
        assert!(
            app.status
                .as_ref()
                .unwrap()
                .message
                .contains("cagents inbox")
        );
        // Duplicate completion cannot act again, and a later explicit
        // lifecycle activation cancels an in-flight inbox activation.
        drop(app.update_notifications(Action::InboxResolved {
            lineage,
            operation,
            result: Err(()),
        }));
        assert!(
            app.status
                .as_ref()
                .unwrap()
                .message
                .contains("cagents inbox")
        );
        app.notification_ui.inbox_activation = Some((lineage, uuid::Uuid::new_v4()));
        drop(app.update_notifications(Action::Activated(Target::Session(fresh.id))));
        assert!(app.notification_ui.inbox_activation.is_none());
        let mut changed_launch = fresh.clone();
        changed_launch.launch.working_directory = "/another".into();
        assert!(!same_live_terminal(&changed_launch, &fresh));
        changed_launch = fresh.clone();
        changed_launch.id = TerminalId::new();
        assert!(!same_live_terminal(&changed_launch, &fresh));
    }

    #[test]
    fn every_test_notification_answer_is_reported_with_its_own_severity() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        for _ in 0..2 {
            let _ = app.update(Message::NotificationUi(Action::Tested(Ok(
                TestOutcome::Sent,
            ))));
        }
        // A repeat is still news: the entry moves up and counts it, rather
        // than the second press going unanswered.
        let latest = app.notices.recent().next().unwrap();
        assert_eq!(latest.notice.severity, NoticeSeverity::Success);
        assert_eq!(latest.repeats, 2);

        let _ = app.update(Message::NotificationUi(Action::Tested(Ok(
            TestOutcome::TimedOut,
        ))));
        assert_eq!(
            app.notices.recent().next().unwrap().notice.severity,
            NoticeSeverity::Error
        );
    }

    #[test]
    fn a_test_notification_is_not_requested_again_while_one_is_pending() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.notification_ui.testing = true;
        let _ = app.update_notifications(Action::Test);
        assert!(app.notification_ui.testing);
        let _ = app.update_notifications(Action::Tested(Err("offline".into())));
        assert!(!app.notification_ui.testing);
    }

    #[test]
    fn completed_run_activation_opens_history_without_mounting_a_provider() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let original = app
            .durable
            .cards
            .iter()
            .filter(|card| matches!(card.kind, CardKind::Chat(_) | CardKind::Terminal(_)))
            .count();
        let run = crate::model::RunId::new();
        // Restoring an automation card starts a list read before its cold
        // notification activation reaches the UI.
        drop(app.update_automation_ui(crate::automation_ui::Action::Refresh));
        drop(app.update_notifications(Action::Activated(Target::AgentRun(run))));
        assert!(
            app.durable
                .cards
                .iter()
                .any(|card| card.kind == CardKind::Automations && card.existence.is_open())
        );
        assert_eq!(
            app.durable
                .cards
                .iter()
                .filter(|card| matches!(card.kind, CardKind::Chat(_) | CardKind::Terminal(_)))
                .count(),
            original
        );
        assert!(matches!(
            app.card_state(app.workspace.focused().unwrap())
                .unwrap()
                .kind,
            CardKind::Automations
        ));
    }
    use crate::model::{ChannelId, ProjectChannel};

    #[test]
    fn review_notification_keeps_current_panes_and_acknowledges_only_that_review() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let focused = app.workspace.focused();
        let cards = app.durable.cards.clone();
        let request = crate::notifications::review::ReviewRequest {
            host: "github.com".into(),
            port: None,
            owner: "fixture".into(),
            repository: "repo".into(),
            number: 42,
            viewer: "viewer-node".into(),
            request: "request-node".into(),
        };
        let target = Target::PullRequest(request.clone());
        let event = crate::notifications::review::ObservedRequests {
            destination: crate::notifications::Destination {
                application: Default::default(),
                window: instance::window_slot().into(),
            },
            requests: vec![request],
        }
        .events()
        .next()
        .unwrap();
        let clicked = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        let mut unrelated = event.clone();
        if let Target::PullRequest(request) = &mut unrelated.target {
            request.number = 43;
            request.request = "other-request-node".into();
        }
        unrelated.evidence.key = "other-request-node".into();
        app.notification_ui.snapshot.notices = [(clicked, event), (other, unrelated)]
            .into_iter()
            .map(|(id, event)| crate::notifications::Notice {
                id,
                event,
                delivered: true,
                acknowledged: false,
                window: None,
            })
            .collect();

        drop(app.update_notifications(Action::Activated(target)));
        assert_eq!(app.workspace.focused(), focused);
        assert_eq!(app.durable.cards, cards);
        assert_eq!(
            app.notification_ui.pending.lock().unwrap().acknowledged,
            BTreeSet::from([clicked])
        );

        // Later review-monitor updates cannot reopen the clicked review.
        drop(app.update_review_requests(crate::app::review_request_ui::Action::Tick));
        assert_eq!(app.workspace.focused(), focused);
        assert_eq!(app.durable.cards, cards);
    }

    #[test]
    fn background_window_keeps_attention_until_its_agent_is_focused() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.core.set_main_window_id(Some(window::Id::RESERVED));
        app.context_inspector_open = false;
        let card = app.workspace.focused().unwrap();
        let session = card_session(&app.durable.cards[0]).unwrap();
        let id = uuid::Uuid::new_v4();
        let notice = crate::notifications::Notice {
            id,
            event: crate::notifications::Event {
                content: None,
                target: crate::notifications::Target::Session(session),
                source_session: Some(session),
                provider: Some(Provider::Claude),
                directory: Some("/tmp".into()),
                destination: None,
                evidence: crate::notifications::Evidence {
                    key: "complete".into(),
                    kind: crate::notifications::Kind::Succeeded,
                },
            },
            delivered: true,
            acknowledged: false,
            window: None,
        };
        drop(app.update_notifications(Action::Snapshot(Snapshot {
            notices: vec![notice],
            diagnostic: None,
        })));
        assert!(
            app.notification_ui
                .pending
                .lock()
                .unwrap()
                .acknowledged
                .is_empty()
        );
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
            window::Id::RESERVED,
            true,
        ))));
        assert_eq!(
            app.notification_ui.focused_session_for_test(),
            Some(session)
        );
        assert!(
            !app.notification_ui.acknowledged_for_test(id),
            "return waits for qualified dwell"
        );
        drop(app.update(Message::AcknowledgeVisiblePane {
            card,
            epoch: app.attention_focus_epoch,
        }));
        assert_eq!(
            app.notification_ui.pending.lock().unwrap().acknowledged,
            BTreeSet::from([id])
        );
    }

    #[test]
    fn opted_in_focused_alert_survives_snapshot_and_is_read_on_return() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.core.set_main_window_id(Some(window::Id::RESERVED));
        app.context_inspector_open = false;
        app.settings.notifications.while_focused = true;
        app.refresh_ui.focused = true;
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Tick)));
        let session = card_session(&app.durable.cards[0]).unwrap();
        let id = uuid::Uuid::new_v4();
        let snapshot = Snapshot {
            notices: vec![crate::notifications::Notice {
                id,
                event: crate::notifications::Event {
                    content: None,
                    target: crate::notifications::Target::Session(session),
                    source_session: Some(session),
                    provider: Some(Provider::Claude),
                    directory: Some("/tmp".into()),
                    destination: None,
                    evidence: crate::notifications::Evidence {
                        key: "focused-completion".into(),
                        kind: crate::notifications::Kind::Succeeded,
                    },
                },
                delivered: true,
                acknowledged: false,
                window: None,
            }],
            diagnostic: None,
        };
        drop(app.update_notifications(Action::Snapshot(snapshot.clone())));
        drop(app.update_notifications(Action::Snapshot(snapshot)));
        assert!(
            app.notification_ui
                .pending
                .lock()
                .unwrap()
                .acknowledged
                .is_empty(),
            "notification snapshots must not withdraw an explicitly requested focused alert"
        );

        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
            window::Id::RESERVED,
            false,
        ))));
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
            window::Id::RESERVED,
            true,
        ))));
        assert!(
            !app.notification_ui.acknowledged_for_test(id),
            "return waits for qualified dwell"
        );
        let card = app.workspace.focused().unwrap();
        drop(app.update(Message::AcknowledgeVisiblePane {
            card,
            epoch: app.attention_focus_epoch,
        }));
        assert_eq!(
            app.notification_ui.pending.lock().unwrap().acknowledged,
            BTreeSet::from([id])
        );
    }

    #[test]
    fn attention_modal_visibility_updates_presence_and_invalidates_parent_dwell_without_card_changes()
     {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        app.refresh_ui.focused = true;
        let card = app.workspace.focused().unwrap();
        let session = card_session(app.card_state(card).unwrap()).unwrap();
        drop(app.sync_visible_attention());
        app.sync_notification_presence();
        assert_eq!(
            app.notification_ui.focused_session_for_test(),
            Some(session)
        );
        let stale_epoch = app.attention_focus_epoch;
        let unchanged = Arc::as_ptr(&app.durable);
        app.settings_reset_confirm = true;
        drop(app.sync_visible_attention());
        app.sync_notification_presence();
        assert_eq!(Arc::as_ptr(&app.durable), unchanged);
        assert_eq!(app.notification_ui.focused_session_for_test(), None);
        let id = uuid::Uuid::new_v4();
        drop(
            app.update(Message::NotificationUi(Action::Snapshot(Snapshot {
                notices: vec![crate::notifications::Notice {
                    id,
                    event: crate::notifications::Event {
                        content: None,
                        target: Target::Session(session),
                        source_session: Some(session),
                        provider: Some(Provider::Claude),
                        directory: Some("/tmp".into()),
                        destination: None,
                        evidence: crate::notifications::Evidence {
                            key: "parent-waiting-behind-modal".into(),
                            kind: crate::notifications::Kind::Waiting,
                        },
                    },
                    delivered: true,
                    acknowledged: false,
                    window: None,
                }],
                diagnostic: None,
            }))),
        );
        assert!(app.sidebar_state.has_card_update(card));
        assert!(!app.notification_ui.acknowledged_for_test(id));
        app.settings_reset_confirm = false;
        drop(app.sync_visible_attention());
        app.sync_notification_presence();
        assert_eq!(
            app.notification_ui.focused_session_for_test(),
            Some(session)
        );
        let current_epoch = app.attention_focus_epoch;
        assert!(!app.notification_ui.acknowledged_for_test(id));
        assert_eq!(app.durable.focused, app.workspace.focused());
        let resize = app.workspace.resize_viewport(1200.0);
        drop(app.apply_viewport(resize, true, false));
        assert!(
            app.sidebar_state.has_card_update(card),
            "an unrelated viewport update cannot bypass visible dwell"
        );
        drop(app.update(Message::AcknowledgeVisiblePane {
            card,
            epoch: stale_epoch,
        }));
        assert!(app.sidebar_state.has_card_update(card));
        drop(app.update(Message::AcknowledgeVisiblePane {
            card,
            epoch: current_epoch,
        }));
        assert!(!app.sidebar_state.has_card_update(card));
        assert!(app.notification_ui.acknowledged_for_test(id));
    }

    #[test]
    fn attention_background_journal_waits_for_current_visible_dwell() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let session = card_session(&app.durable.cards[0]).unwrap();
        let id = uuid::Uuid::new_v4();
        let notice = crate::notifications::Notice {
            id,
            event: crate::notifications::Event {
                content: None,
                target: crate::notifications::Target::Session(session),
                source_session: Some(session),
                provider: Some(Provider::Claude),
                directory: Some("/tmp".into()),
                destination: None,
                evidence: crate::notifications::Evidence {
                    key: "complete".into(),
                    kind: crate::notifications::Kind::Succeeded,
                },
            },
            delivered: true,
            acknowledged: false,
            window: None,
        };
        drop(app.update_notifications(Action::Snapshot(Snapshot {
            notices: vec![notice],
            diagnostic: None,
        })));
        assert!(
            app.notification_ui
                .pending
                .lock()
                .unwrap()
                .acknowledged
                .is_empty()
        );
        app.context_inspector_open = false;
        app.core.set_main_window_id(Some(window::Id::RESERVED));
        let card = app.workspace.focused().unwrap();
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
            window::Id::RESERVED,
            true,
        ))));
        let stale_epoch = app.attention_focus_epoch;
        assert_eq!(
            app.notification_ui.focused_session_for_test(),
            Some(session)
        );
        assert!(!app.notification_ui.acknowledged_for_test(id));
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
            window::Id::RESERVED,
            false,
        ))));
        drop(app.update(Message::AcknowledgeVisiblePane {
            card,
            epoch: stale_epoch,
        }));
        assert!(
            !app.notification_ui.acknowledged_for_test(id),
            "returning then leaving before dwell cannot read an unseen journal entry"
        );
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
            window::Id::RESERVED,
            true,
        ))));
        let epoch = app.attention_focus_epoch;
        assert_ne!(epoch, stale_epoch);
        assert!(!app.notification_ui.acknowledged_for_test(id));
        drop(app.update(Message::AcknowledgeVisiblePane { card, epoch }));
        assert!(app.notification_ui.acknowledged_for_test(id));
        assert!(!app.sidebar_state.has_card_update(card));
    }

    #[test]
    fn attention_opted_in_fresh_alert_survives_an_older_visibility_dwell() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        app.core.set_main_window_id(Some(window::Id::RESERVED));
        let card = app.workspace.focused().unwrap();
        app.settings.notifications.while_focused = true;
        app.refresh_ui.focused = true;
        app.sync_notification_presence();
        let session = card_session(&app.durable.cards[0]).unwrap();
        let id = uuid::Uuid::new_v4();
        let snapshot = Snapshot {
            notices: vec![crate::notifications::Notice {
                id,
                event: crate::notifications::Event {
                    content: None,
                    target: crate::notifications::Target::Session(session),
                    source_session: Some(session),
                    provider: Some(Provider::Claude),
                    directory: Some("/tmp".into()),
                    destination: None,
                    evidence: crate::notifications::Evidence {
                        key: "focused-completion".into(),
                        kind: crate::notifications::Kind::Succeeded,
                    },
                },
                delivered: true,
                acknowledged: false,
                window: None,
            }],
            diagnostic: None,
        };
        drop(app.update_notifications(Action::Snapshot(snapshot.clone())));
        drop(app.update_notifications(Action::Snapshot(snapshot.clone())));
        assert!(
            app.notification_ui
                .pending
                .lock()
                .unwrap()
                .acknowledged
                .is_empty(),
            "notification snapshots must not withdraw an explicitly requested focused alert"
        );

        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
            window::Id::RESERVED,
            false,
        ))));
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
            window::Id::RESERVED,
            true,
        ))));
        let epoch = app.attention_focus_epoch;
        assert!(!app.notification_ui.acknowledged_for_test(id));
        let newer = uuid::Uuid::new_v4();
        let mut next = snapshot.clone();
        let mut notice = next.notices[0].clone();
        notice.id = newer;
        notice.event.evidence.key = "newer-focused-completion".into();
        next.notices.push(notice);
        drop(app.update(Message::NotificationUi(Action::Snapshot(next))));
        assert!(!app.notification_ui.acknowledged_for_test(newer));
        drop(app.update(Message::AcknowledgeVisiblePane { card, epoch }));
        assert!(app.notification_ui.acknowledged_for_test(id));
        assert!(
            !app.notification_ui.acknowledged_for_test(newer),
            "an older dwell cannot withdraw a newer opted-in alert"
        );
        drop(app.mark_cards_read(vec![card]));
        assert!(
            app.notification_ui.acknowledged_for_test(newer),
            "explicit Mark Read retains its declared current scope"
        );
    }

    #[test]
    fn attention_committed_window_close_stops_claiming_viewed_content_during_exit_reap() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        let session = live_session(&mut app);
        let card = app.workspace.focused().unwrap();
        let window = window::Id::unique();
        app.core.set_main_window_id(Some(window));
        app.refresh_ui.focused = true;
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Tick)));
        assert_eq!(
            app.notification_ui.focused_session_for_test(),
            Some(session.id)
        );
        drop(app.update(Message::WindowCloseRequested(window)));
        assert!(
            app.exit.is_some(),
            "hold the real committed exit before reap completion"
        );
        assert!(app.core.main_window_id().is_none());
        assert!(
            app.refresh_ui.focused,
            "the stale native focus bit is the regression control"
        );
        assert_eq!(app.notification_ui.focused_session_for_test(), None);
        let id = uuid::Uuid::new_v4();
        drop(
            app.update(Message::NotificationUi(Action::Snapshot(Snapshot {
                notices: vec![crate::notifications::Notice {
                    id,
                    event: crate::notifications::Event {
                        content: None,
                        target: Target::Session(session.id),
                        source_session: Some(session.id),
                        provider: Some(Provider::Claude),
                        directory: Some("/tmp".into()),
                        destination: None,
                        evidence: crate::notifications::Evidence {
                            key: "waiting-during-exit-reap".into(),
                            kind: crate::notifications::Kind::Waiting,
                        },
                    },
                    delivered: true,
                    acknowledged: false,
                    window: None,
                }],
                diagnostic: None,
            }))),
        );
        assert_eq!(app.notification_ui.focused_session_for_test(), None);
        assert!(!app.notification_ui.acknowledged_for_test(id));
        assert!(app.sidebar_state.has_card_update(card));
    }

    #[test]
    fn attention_early_window_close_modal_and_escape_fence_presence_and_old_dwell() {
        use cosmic::iced::futures::StreamExt;
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let terminal = live_session(&mut app);
        let card = app.workspace.focused().unwrap();
        let window = window::Id::unique();
        app.core.set_main_window_id(Some(window));
        app.refresh_ui.focused = true;
        app.settings.notifications.while_focused = true;
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Tick)));
        assert_eq!(
            app.notification_ui.focused_session_for_test(),
            Some(terminal.id)
        );
        let original_epoch = app.attention_focus_epoch;
        // This is a real close blocker retained outside the parent surface.
        app.automation_ui.update(crate::automation_ui::Action::New);
        app.automation_ui
            .update(crate::automation_ui::Action::Field(
                crate::automation_ui::Field::Name,
                "Keep this unsaved draft".into(),
            ));
        drop(app.update(Message::WindowCloseRequested(window)));
        assert_eq!(app.transient_work_close, Some(window));
        assert!(app.modal_is_open());
        assert_eq!(app.notification_ui.focused_session_for_test(), None);
        assert_ne!(app.attention_focus_epoch, original_epoch);
        let id = uuid::Uuid::new_v4();
        drop(
            app.update(Message::NotificationUi(Action::Snapshot(Snapshot {
                notices: vec![crate::notifications::Notice {
                    id,
                    event: crate::notifications::Event {
                        content: None,
                        target: Target::Session(terminal.id),
                        source_session: Some(terminal.id),
                        provider: Some(Provider::Claude),
                        directory: Some("/tmp".into()),
                        destination: None,
                        evidence: crate::notifications::Evidence {
                            key: "waiting-behind-actual-close-dialog".into(),
                            kind: crate::notifications::Kind::Waiting,
                        },
                    },
                    delivered: true,
                    acknowledged: false,
                    window: None,
                }],
                diagnostic: None,
            }))),
        );
        assert!(app.sidebar_state.has_card_update(card));
        assert!(!app.notification_ui.acknowledged_for_test(id));
        let escape = app.update(Message::KeyEvent {
            event: keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            },
            captured: false,
        });
        // Drive only the actual Escape dismissal message. A newly scheduled
        // dwell remains a separate fenced message, not blanket task draining.
        if app.transient_work_close.is_some() {
            let cancellations = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let Some(mut stream) = cosmic::iced::runtime::task::into_stream(escape) else {
                        return 0;
                    };
                    let mut cancellations = 0;
                    while let Some(action) = stream.next().await {
                        if matches!(
                            action,
                            iced::runtime::Action::Output(cosmic::Action::App(
                                Message::CancelTransientWorkClose
                            ))
                        ) {
                            cancellations += 1;
                        }
                    }
                    cancellations
                });
            assert_eq!(
                cancellations, 1,
                "Escape must route the original close cancellation"
            );
            drop(app.update(Message::CancelTransientWorkClose));
        } else {
            drop(escape);
        }
        assert!(app.transient_work_close.is_none());
        assert!(app.automation_ui.draft_dirty());
        assert_eq!(
            app.notification_ui.focused_session_for_test(),
            Some(terminal.id)
        );
        let current_epoch = app.attention_focus_epoch;
        assert_ne!(current_epoch, original_epoch);
        drop(app.update(Message::AcknowledgeVisiblePane {
            card,
            epoch: original_epoch,
        }));
        assert!(app.sidebar_state.has_card_update(card));
        assert!(!app.notification_ui.acknowledged_for_test(id));
        drop(app.update(Message::AcknowledgeVisiblePane {
            card,
            epoch: current_epoch,
        }));
        assert!(!app.sidebar_state.has_card_update(card));
        assert!(app.notification_ui.acknowledged_for_test(id));
    }

    fn scheduled_attention_fixture() -> (Cagents, crate::agent_execution::Agent) {
        use crate::agent_execution::{
            Agent, Origin, Placement, Spec, Surface, Target as AgentTarget,
        };
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.context_inspector_open = false;
        let project = ProjectId::new();
        let repository = PathBuf::from("/tmp");
        let channel = ChannelId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "Scheduled".into(),
            repository.clone(),
            Vec::new(),
            Vec::new(),
        ));
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                id: channel,
                project_id: project,
                title: "Scheduled".into(),
                title_source: Default::default(),
                ordinal: 0,
                shared: true,
                cards: vec![],
                focused: None,
            });
        let CardKind::Terminal(mut console) = super::super::tests::terminal_card_state().kind
        else {
            unreachable!()
        };
        console.launch.working_directory = repository.clone();
        console.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("scheduled-host"),
            boot_id: crate::model::BootId::new("scheduled-boot"),
            host_incarnation: crate::model::HostIncarnation::new("scheduled-incarnation"),
        };
        let placement = Placement::Channel(channel);
        let agent = Agent {
            id: crate::model::AgentId::new(),
            name: None,
            origin: Origin::Automation {
                id: uuid::Uuid::new_v4(),
            },
            spec: Spec {
                provider: crate::agent::Provider::Claude,
                account: None,
                target: AgentTarget {
                    repository: repository.clone(),
                    directory: repository.clone(),
                    placement: Some(placement.clone()),
                },
                surface: Surface::Terminal,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement,
            session: None,
            console: Some(console),
            created_ms: 0,
            closed: false,
        };
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.workspace_model.sequence = 1;
        app.workspace_model
            .channels
            .push(crate::workspace_model::ModelChannel {
                id: channel,
                repository,
                title: "Scheduled".into(),
                title_source: Default::default(),
                ordinal: 0,
            });
        (app, agent)
    }

    #[test]
    fn attention_activation_adopts_only_the_exact_existing_scheduled_actor_after_watch_arrival() {
        let (mut app, agent) = scheduled_attention_fixture();
        let descriptor = activation_session(&agent).unwrap();
        let session = descriptor.id();
        let card = CardId::from_uuid(agent.id.as_uuid());
        drop(app.update_notifications(Action::Activated(Target::Session(session))));
        assert!(app.durable.cards.is_empty());
        let unchanged = Arc::as_ptr(&app.durable);
        app.workspace_model.agents.push(agent.clone());
        app.workspace_model.sequence += 1;
        assert_eq!(Arc::as_ptr(&app.durable), unchanged);
        drop(app.retry_notification_activation());
        assert_eq!(app.workspace.focused(), Some(card));
        assert_eq!(app.durable.active_channel, agent.channel());
        assert!(app.notification_ui.session_activation.is_none());
        let adopted = app.card_state(card).unwrap();
        assert!(
            matches!(&adopted.kind, CardKind::Terminal(actual) if crate::agent_execution::HostedSession::Terminal(actual.clone()).same_process(&descriptor))
        );
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert!(
            !app.chat_start_pending(card),
            "terminal activation cannot start a chat or replay a provider prompt"
        );
        let cards = app.durable.cards.len();
        drop(app.update_notifications(Action::Activated(Target::Session(session))));
        assert_eq!(app.durable.cards.len(), cards);
    }

    #[test]
    fn scheduled_attention_chat_activation_attaches_to_the_existing_native_proof_without_a_prompt()
    {
        let (mut app, mut agent) = scheduled_attention_fixture();
        let console = agent.console.take().unwrap();
        let session = crate::chat::Session {
            native_continuity: Default::default(),
            id: TerminalId::new(),
            proof: console.continuity.live_proof(),
            terminal: None,
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: agent.spec.provider,
                session_id: None,
                recipe: console.launch,
            },
        };
        agent.spec.surface = crate::agent_execution::Surface::Chat;
        agent.session = Some(crate::agent_execution::HostedSession::Chat(session.clone()));
        app.workspace_model.agents.push(agent.clone());
        drop(app.update_notifications(Action::Activated(Target::Session(session.id))));
        let card = CardId::from_uuid(agent.id.as_uuid());
        assert_eq!(app.workspace.focused(), Some(card));
        assert!(
            app.chat_start_pending(card),
            "existing native descriptor is attached, never synthesized"
        );
        assert!(
            matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(actual) if actual == &session)
        );
        assert_eq!(app.durable.cards.len(), 1);
    }

    #[test]
    fn scheduled_attention_activation_uses_authenticated_current_placement_after_a_move() {
        let (mut app, mut agent) = scheduled_attention_fixture();
        let original = agent.spec.target.placement.clone();
        let destination = ChannelId::new();
        let mut local = app.durable.channels[0].clone();
        local.id = destination;
        local.title = "Moved destination".into();
        Arc::make_mut(&mut app.durable).channels.push(local);
        let mut shared = app.workspace_model.channels[0].clone();
        shared.id = destination;
        app.workspace_model.channels.push(shared);
        agent.placement = crate::agent_execution::Placement::Channel(destination);
        let session = activation_session(&agent).unwrap().id();
        app.workspace_model.agents.push(agent.clone());
        drop(app.update_notifications(Action::Activated(Target::Session(session))));
        assert_eq!(
            agent.spec.target.placement, original,
            "accepted launch request remains immutable"
        );
        assert_eq!(app.durable.active_channel, Some(destination));
        assert_eq!(
            app.workspace.focused(),
            Some(CardId::from_uuid(agent.id.as_uuid()))
        );
        assert!(app.durable.channels[0].cards.is_empty());
    }

    #[test]
    fn attention_activation_follows_saved_surface_and_legacy_display_without_rewriting_execution() {
        use crate::agent_execution::{HostedSession, Placement, Surface};
        use crate::workspace_model::views::{Pane, View};
        for (saved_chat, shared_uuid) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let (mut app, mut agent) = scheduled_attention_fixture();
            let console = agent.console.clone().unwrap();
            let native = crate::chat::Session {
                native_continuity: Default::default(),
                id: if shared_uuid {
                    console.id
                } else {
                    TerminalId::new()
                },
                proof: console.continuity.live_proof(),
                terminal: None,
                owned_codex_listener: None,
                launch: crate::chat::Launch {
                    provider: agent.spec.provider,
                    session_id: None,
                    recipe: console.launch.clone(),
                },
            };
            agent.session = Some(HostedSession::Chat(native.clone()));
            agent.spec.surface = if saved_chat {
                Surface::Terminal
            } else {
                Surface::Chat
            };
            let original = agent.clone();
            app.project_ui
                .closed
                .insert(original.spec.target.repository.clone());
            let destination = ChannelId::new();
            let project = ProjectId::new();
            let repository = PathBuf::from("/legacy-display");
            app.sidebar_model.projects.push(SidebarProject::new(
                project,
                RepositoryId::new(),
                "Legacy display".into(),
                repository.clone(),
                vec![],
                vec![],
            ));
            let mut local = app.durable.channels[0].clone();
            local.id = destination;
            local.project_id = project;
            Arc::make_mut(&mut app.durable).channels.push(local);
            let mut shared = app.workspace_model.channels[0].clone();
            shared.id = destination;
            shared.repository = repository;
            app.workspace_model.channels.push(shared);
            let mut card = super::super::tests::terminal_card_state();
            card.id = CardId::from_uuid(agent.id.as_uuid());
            card.title = "Saved moved conversation".into();
            card.width_fraction = 0.65;
            card.kind = if saved_chat {
                CardKind::Chat(native.clone())
            } else {
                CardKind::Terminal(console.clone())
            };
            let pane = Pane::from_card(&card, Some(agent.spec.target.directory.clone())).unwrap();
            let mut source = View::empty(agent.channel().unwrap());
            source.retired.push(card.id);
            app.workspace_model.views = vec![
                source,
                View {
                    scope: Placement::Channel(destination),
                    revision: 2,
                    panes: vec![pane.clone()],
                    retired: vec![],
                },
            ];
            app.workspace_model.agents.push(agent);
            let session = if saved_chat { console.id } else { native.id };
            drop(app.update_notifications(Action::Activated(Target::Session(session))));
            assert_eq!(app.durable.active_channel, Some(destination));
            assert_eq!(app.workspace.focused(), Some(card.id));
            assert_eq!(app.card_state(card.id).unwrap().kind, card.kind);
            assert_eq!(app.card_state(card.id).unwrap().title, card.title);
            assert_eq!(app.card_state(card.id).unwrap().width_fraction, 0.65);
            assert!(app.durable.channels[0].cards.is_empty());
            let actual_session = if saved_chat { native.id } else { console.id };
            drop(app.update_notifications(Action::Activated(Target::Session(actual_session))));
            assert_eq!(app.workspace.focused(), Some(card.id));
            assert_eq!(
                app.durable.cards.len(),
                1,
                "counterpart and actual-surface clicks reuse one canonical pane"
            );
            assert_eq!(
                app.workspace_model.agents[0], original,
                "display-only activation preserves accepted cwd/spec/placement/provider/native actor"
            );
            app.project_ui
                .closed
                .remove(&original.spec.target.repository);
            drop(app.select_channel(original.channel().unwrap()));
            assert_ne!(app.workspace.focused(), Some(card.id));
            let focused = app.workspace.focused();
            let saved_cards = app.durable.cards.clone();
            let current = &mut app.workspace_model.agents[0];
            if saved_chat {
                let console = current.console.as_mut().unwrap();
                console.continuity = TerminalContinuity::RelaunchAvailable {
                    recipe: console.launch.clone(),
                    last_exit: None,
                };
            } else if let Some(HostedSession::Chat(native)) = &mut current.session {
                native.proof = None;
            }
            app.workspace_model.sequence += 1;
            drop(app.update_notifications(Action::Activated(Target::Session(session))));
            assert_eq!(
                app.workspace.focused(),
                focused,
                "the requested counterpart needs its own complete current proof even when the saved surface is proved, including shared UUIDs"
            );
            assert_eq!(app.durable.cards, saved_cards);
            app.workspace_model.agents[0] = original.clone();
            app.workspace_model.sequence += 1;
            app.workspace_model.views[1].panes[0].kind =
                super::super::tests::terminal_card_state().kind;
            drop(app.update_notifications(Action::Activated(Target::Session(session))));
            assert_eq!(
                app.workspace.focused(),
                focused,
                "replaced canonical ownership cannot reactivate a stale retained local pane"
            );
            assert_eq!(app.durable.cards, saved_cards);
            assert_eq!(app.workspace_model.agents[0], original);
        }
    }

    #[test]
    fn attention_fresh_headless_pair_mounts_primary_once_without_replaying_provider() {
        use crate::agent_execution::HostedSession;
        let (mut app, mut agent) = scheduled_attention_fixture();
        let console = agent.console.clone().unwrap();
        let native = crate::chat::Session {
            native_continuity: Default::default(),
            id: console.id,
            proof: console.continuity.live_proof(),
            terminal: Some(console.clone()),
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: agent.spec.provider,
                session_id: None,
                recipe: console.launch.clone(),
            },
        };
        agent.session = Some(HostedSession::Chat(native.clone()));
        let original = agent.clone();
        let id = CardId::from_uuid(agent.id.as_uuid());
        let initial_count = app.durable.cards.len();
        assert!(
            app.workspace_model
                .views
                .iter()
                .all(|view| view.panes.iter().all(|pane| pane.id != id))
        );
        app.workspace_model.agents.push(agent);
        drop(app.update_notifications(Action::Activated(Target::Session(console.id))));
        assert_eq!(app.workspace.focused(), Some(id));
        assert_eq!(app.card_state(id).unwrap().kind, CardKind::Chat(native));
        assert_eq!(app.durable.cards.len(), initial_count + 1);
        assert_eq!(
            app.workspace_model.agents[0], original,
            "activation cannot rewrite or replay the actor"
        );
        drop(app.update_notifications(Action::Activated(Target::Session(console.id))));
        assert_eq!(app.workspace.focused(), Some(id));
        assert_eq!(
            app.durable.cards.len(),
            initial_count + 1,
            "repeated clicks reuse canonical membership"
        );
    }

    #[test]
    fn attention_console_alias_defers_fresh_chat_until_exact_shared_reconciliation() {
        use crate::agent_execution::HostedSession;
        use crate::workspace_model::views::{Pane, View};
        let (mut app, mut agent) = scheduled_attention_fixture();
        let console = agent.console.clone().unwrap();
        let native = crate::chat::Session {
            native_continuity: Default::default(),
            id: console.id,
            proof: console.continuity.live_proof(),
            terminal: Some(console.clone()),
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: agent.spec.provider,
                session_id: None,
                recipe: console.launch.clone(),
            },
        };
        agent.session = Some(HostedSession::Chat(native));
        let canonical = CardId::from_uuid(agent.id.as_uuid());
        let alias = CardId::new();
        let original = agent.clone();
        app.workspace_model.agents.push(agent);
        app.workspace_model.views = vec![View {
            scope: original.placement.clone(),
            revision: 1,
            panes: vec![Pane {
                id: alias,
                title: "Existing console".into(),
                user_title: None,
                pending_terminal_start: false,
                width_fraction: 1.0,
                scope: None,
                kind: CardKind::Terminal(console.clone()),
            }],
            retired: vec![],
        }];
        drop(app.update_notifications(Action::Activated(Target::Session(console.id))));
        assert!(app.durable.cards.is_empty());
        assert_ne!(app.workspace.focused(), Some(canonical));
        assert_eq!(app.notification_ui.session_activation, Some(console.id));
        assert!(!app.chat_start_pending(canonical));
        assert_eq!(app.workspace_model.agents[0], original);

        // An actual shared alias migration publishes the canonical console.
        // Retry must attach that saved surface rather than create the primary.
        app.workspace_model.views[0].panes[0].id = canonical;
        app.workspace_model.views[0].retired.push(alias);
        app.workspace_model.views[0].revision += 1;
        app.workspace_model.sequence += 1;
        drop(app.retry_notification_activation());
        assert_eq!(app.workspace.focused(), Some(canonical));
        assert_eq!(
            app.card_state(canonical).unwrap().kind,
            CardKind::Terminal(console.clone())
        );
        assert_eq!(app.durable.cards.len(), 1);
        assert!(!app.chat_start_pending(canonical));
        assert!(app.notification_ui.session_activation.is_none());
        assert_eq!(app.workspace_model.agents[0], original);
        drop(app.update_notifications(Action::Activated(Target::Session(console.id))));
        assert_eq!(app.durable.cards.len(), 1);
        assert_eq!(app.workspace.focused(), Some(canonical));
    }

    #[test]
    fn attention_unmanaged_open_session_is_not_shadowed_by_its_retired_alias() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        let card = app.workspace.focused().unwrap();
        let session = card_session(app.card_state(card).unwrap()).unwrap();
        let mut alias = app.card_state(card).unwrap().clone();
        alias.id = CardId::new();
        alias.existence = super::super::project_ui::removed_existence("retired inventory alias");
        Arc::make_mut(&mut app.durable)
            .cards
            .insert(0, alias.clone());
        app.sidebar_state.observe_card_update(card);
        drop(app.update_notifications(Action::Activated(Target::Session(session))));
        assert_eq!(app.workspace.focused(), Some(card));
        assert!(
            !app.sidebar_state.has_card_update(card),
            "successful activation acknowledges the open exact session"
        );
        assert_eq!(app.card_state(alias.id).unwrap().existence, alias.existence);
        assert_eq!(app.durable.cards.len(), 2);
    }

    #[test]
    fn attention_non_automation_activation_waits_for_normal_model_adoption() {
        let (mut app, mut agent) = scheduled_attention_fixture();
        agent.origin = crate::agent_execution::Origin::User;
        let session = agent.console.as_ref().unwrap().id;
        app.workspace_model.agents.push(agent.clone());
        drop(app.update_notifications(Action::Activated(Target::Session(session))));
        assert!(app.durable.cards.is_empty());
        assert_eq!(app.notification_ui.session_activation, Some(session));
        drop(app.adopt_execution_agent(&agent).unwrap());
        drop(app.retry_notification_activation());
        assert_eq!(
            app.workspace.focused(),
            Some(CardId::from_uuid(agent.id.as_uuid()))
        );
        assert_eq!(app.durable.cards.len(), 1);
    }

    fn attention_snapshot(session: TerminalId) -> Snapshot {
        Snapshot {
            diagnostic: None,
            notices: vec![crate::notifications::Notice {
                id: uuid::Uuid::new_v4(),
                event: crate::notifications::Event::session(
                    session,
                    Provider::Claude,
                    "/tmp".into(),
                    crate::notifications::Evidence {
                        key: "canonical-unread".into(),
                        kind: crate::notifications::Kind::Waiting,
                    },
                ),
                delivered: true,
                acknowledged: false,
                window: None,
            }],
        }
    }

    #[test]
    fn notification_snapshot_rejects_invalid_managed_membership_without_alias_fallback() {
        for scenario in 0..4 {
            let (mut app, agent) = scheduled_attention_fixture();
            let session = activation_session(&agent).unwrap().id();
            let canonical = CardId::from_uuid(agent.id.as_uuid());
            app.workspace_model.agents.push(agent.clone());
            drop(app.adopt_execution_agent(&agent).unwrap());
            let mut alias = app.card_state(canonical).unwrap().clone();
            alias.id = CardId::new();
            let alias_id = alias.id;
            Arc::make_mut(&mut app.durable).cards.insert(0, alias);
            match scenario {
                0 => {
                    let mut duplicate = agent.clone();
                    duplicate.id = crate::model::AgentId::new();
                    app.workspace_model.agents.push(duplicate);
                }
                1 => {
                    let CardKind::Terminal(session) =
                        &mut app.card_state_mut(canonical).unwrap().kind
                    else {
                        unreachable!()
                    };
                    if let TerminalContinuity::Live {
                        host_incarnation, ..
                    } = &mut session.continuity
                    {
                        *host_incarnation = crate::model::HostIncarnation::new("replacement-host");
                    }
                }
                2 => {
                    app.card_state_mut(canonical).unwrap().existence =
                        super::super::project_ui::removed_existence("explicit canonical close");
                }
                3 => app.workspace_model.identity = crate::workspace_model::NO_REGISTRY,
                _ => unreachable!(),
            }
            let before = app.durable.clone();
            let model = app.workspace_model.clone();
            let focused = app.workspace.focused();
            let count = app.cards.len();
            drop(app.update_notifications(Action::Snapshot(attention_snapshot(session))));
            assert!(
                !app.sidebar_state.has_card_update(alias_id),
                "scenario{scenario}"
            );
            assert!(
                !app.sidebar_state.has_card_update(canonical),
                "scenario{scenario}"
            );
            assert!(
                Arc::ptr_eq(&before, &app.durable),
                "attribution cannot adopt or persist"
            );
            assert_eq!(app.workspace_model, model);
            assert_eq!(app.workspace.focused(), focused);
            assert_eq!(app.cards.len(), count);
            assert!(app.notification_ui.session_activation.is_none());
        }
    }

    #[test]
    fn notification_snapshot_marks_unique_unmanaged_session_without_first_alias_routing() {
        for duplicate_open in [false, true] {
            let mut app = super::super::tests::context_inspector_test_app(true);
            app.refresh_ui.focused = false;
            let current = app.durable.cards[0].clone();
            let session = card_session(&current).unwrap();
            let mut alias = current.clone();
            alias.id = CardId::new();
            if !duplicate_open {
                alias.existence = super::super::project_ui::removed_existence("older closed alias");
            }
            let alias_id = alias.id;
            Arc::make_mut(&mut app.durable).cards.insert(0, alias);
            drop(app.update_notifications(Action::Snapshot(attention_snapshot(session))));
            assert_eq!(
                app.sidebar_state.has_card_update(current.id),
                !duplicate_open
            );
            assert!(!app.sidebar_state.has_card_update(alias_id));
        }
    }

    #[test]
    fn attention_managed_activation_uses_canonical_identity_ahead_of_open_or_closed_aliases() {
        use crate::agent_execution::Origin;
        use crate::workspace_model::views::{Pane, View};
        for automation in [false, true] {
            for alias_open in [false, true] {
                let (mut app, mut agent) = scheduled_attention_fixture();
                if !automation {
                    agent.origin = Origin::User;
                }
                let session = agent.console.as_ref().unwrap().id;
                let canonical = CardId::from_uuid(agent.id.as_uuid());
                app.workspace_model.agents.push(agent.clone());
                drop(app.adopt_execution_agent(&agent).unwrap());
                let card = app.card_state(canonical).unwrap().clone();
                app.workspace_model.views = vec![View {
                    scope: agent.placement.clone(),
                    revision: 1,
                    panes: vec![
                        Pane::from_card(&card, Some(agent.spec.target.directory.clone())).unwrap(),
                    ],
                    retired: vec![],
                }];
                let mut alias = card.clone();
                alias.id = CardId::new();
                if !alias_open {
                    alias.existence =
                        super::super::project_ui::removed_existence("retired inventory alias");
                }
                let alias_id = alias.id;
                let alias_existence = alias.existence.clone();
                let durable = Arc::make_mut(&mut app.durable);
                durable.cards.insert(0, alias);
                durable
                    .chat_drafts
                    .insert(alias_id, "Preserved alias draft".into());
                let other = ChannelId::new();
                let mut local = durable.channels[0].clone();
                local.id = other;
                local.cards.clear();
                local.focused = None;
                durable.channels.push(local);
                let mut shared = app.workspace_model.channels[0].clone();
                shared.id = other;
                app.workspace_model.channels.push(shared);
                drop(app.select_channel(other));
                assert_ne!(app.workspace.focused(), Some(canonical));
                if alias_open {
                    // Drive the real alias blocker: both drafts must survive,
                    // so shared reconciliation cannot establish uniqueness by
                    // removing the earlier inventory alias.
                    app.chat_ui = crate::app::chat_ui::State::with_drafts(HashMap::from([
                        (alias_id, "Preserved alias draft".into()),
                        (canonical, "Preserved canonical draft".into()),
                    ]));
                    app.acknowledge_view_edit(agent.placement.clone(), None);
                    drop(app.reconcile_shared_views());
                    assert!(app.shared_views.blocked.contains(&agent.placement));
                    assert!(app.card_state(alias_id).unwrap().existence.is_open());
                }
                let snapshot = attention_snapshot(session);
                let notice = snapshot.notices[0].id;
                drop(app.update_notifications(Action::Snapshot(snapshot.clone())));
                assert!(app.sidebar_state.has_card_update(canonical));
                assert!(!app.sidebar_state.has_card_update(alias_id));
                assert!(!app.notification_ui.acknowledged_for_test(notice));
                drop(app.update_notifications(Action::Activated(Target::Session(session))));
                assert!(!app.sidebar_state.has_card_update(canonical));
                assert!(!app.sidebar_state.has_card_update(alias_id));
                assert!(app.notification_ui.acknowledged_for_test(notice));
                drop(app.update_notifications(Action::Snapshot(snapshot)));
                assert!(!app.sidebar_state.has_card_update(canonical));
                assert!(!app.sidebar_state.has_card_update(alias_id));
                assert_eq!(
                    app.workspace.focused(),
                    Some(canonical),
                    "inventory alias order cannot determine managed activation identity"
                );
                assert_eq!(app.card_state(alias_id).unwrap().existence, alias_existence);
                assert_eq!(
                    app.durable.chat_drafts.get(&alias_id).map(String::as_str),
                    Some("Preserved alias draft")
                );
                assert_eq!(app.durable.cards.len(), 2);
                assert_eq!(app.workspace_model.agents[0], agent);
                drop(app.select_channel(other));
                let focused = app.workspace.focused();
                Arc::make_mut(&mut app.durable)
                    .cards
                    .iter_mut()
                    .find(|card| card.id == canonical)
                    .unwrap()
                    .existence =
                    super::super::project_ui::removed_existence("explicit canonical close");
                let before = app.durable.cards.clone();
                drop(app.update_notifications(Action::Activated(Target::Session(session))));
                assert_eq!(app.workspace.focused(), focused);
                assert_eq!(
                    app.durable.cards, before,
                    "an open alias must not bypass explicit canonical closure"
                );
                assert!(app.notification_ui.session_activation.is_none());
            }
        }
    }

    #[test]
    fn compact_attention_failed_news_is_distinct_from_working_and_unconfirmed_loss() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.refresh_ui.focused = false;
        let card = app.durable.cards[0].id;
        let session = card_session(&app.durable.cards[0]).unwrap();
        let project = ProjectId::new();
        let channel = ChannelId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "Attention".into(),
            "/repo".into(),
            Vec::new(),
            Vec::new(),
        ));
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                title_source: Default::default(),
                id: channel,
                project_id: project,
                title: "Agent".into(),
                ordinal: 0,
                shared: false,
                cards: vec![card],
                focused: Some(card),
            });
        app.sidebar_state.set_card_activity(
            card,
            crate::agent::Activity {
                provider: crate::agent::Provider::Codex,
                state: crate::agent::ActivityState::Working,
            },
        );
        let notice = |kind, key: &str| crate::notifications::Notice {
            id: uuid::Uuid::new_v4(),
            acknowledged: false,
            delivered: true,
            window: None,
            event: crate::notifications::Event::session(
                session,
                crate::agent::Provider::Codex,
                "/repo".into(),
                crate::notifications::Evidence {
                    key: key.into(),
                    kind,
                },
            ),
        };
        let failed = notice(crate::notifications::Kind::Failed, "known-failed-turn");
        let loss = notice(crate::notifications::Kind::Interrupted, "observation-ended");
        let cancelled = notice(
            crate::notifications::Kind::Cancelled,
            "known-cancelled-turn",
        );
        drop(
            app.update(Message::NotificationUi(Action::Snapshot(Snapshot {
                notices: vec![failed.clone(), loss.clone(), cancelled.clone()],
                diagnostic: None,
            }))),
        );
        let summary = app.sidebar_state.project_attention(project);
        assert_eq!(
            summary.unread, 1,
            "three journal events still name one conversation"
        );
        assert_eq!(summary.failed, 1);
        assert_eq!(
            summary.working, 1,
            "historical failure does not stop live work"
        );
        assert_eq!(summary.state(), Some(sidebar::AttentionState::Failed));
        assert_eq!(app.sidebar_model.projects[0].channels[0].attention, summary);
        let mut read_failed = failed;
        read_failed.acknowledged = true;
        drop(
            app.update(Message::NotificationUi(Action::Snapshot(Snapshot {
                notices: vec![read_failed, loss, cancelled],
                diagnostic: None,
            }))),
        );
        let summary = app.sidebar_state.project_attention(project);
        assert_eq!(
            summary.failed, 0,
            "an empty failed projection replaces the old one"
        );
        assert_eq!(summary.unread, 1);
        assert_eq!(summary.state(), Some(sidebar::AttentionState::Working));
        assert_eq!(
            app.sidebar_state.card_activity(card).unwrap().state,
            crate::agent::ActivityState::Working
        );
        drop(app.update(Message::Sidebar(SidebarAction::MarkChannelRead(channel))));
        assert_eq!(app.sidebar_state.project_attention(project).unread, 0);
        assert_eq!(app.sidebar_state.card_attention(card).failed, 0);
    }

    #[test]
    fn compact_attention_empty_foreign_draft_alias_cannot_erase_canonical_failed_news() {
        use crate::workspace_model::views::{Pane, View};
        let (mut app, agent) = scheduled_attention_fixture();
        let session = agent.console.as_ref().unwrap().id;
        let canonical = CardId::from_uuid(agent.id.as_uuid());
        let channel = match &agent.placement {
            crate::agent_execution::Placement::Channel(channel) => *channel,
            _ => unreachable!(),
        };
        let project = app.durable.channels[0].project_id;
        app.workspace_model.agents.push(agent.clone());
        drop(app.adopt_execution_agent(&agent).unwrap());
        let card = app.card_state(canonical).unwrap().clone();
        app.workspace_model.views = vec![View {
            scope: agent.placement.clone(),
            revision: 1,
            panes: vec![Pane::from_card(&card, Some(agent.spec.target.directory.clone())).unwrap()],
            retired: vec![],
        }];
        let mut alias = card.clone();
        alias.id = CardId::new();
        let alias_id = alias.id;
        let foreign = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            foreign,
            RepositoryId::new(),
            "Legacy inventory".into(),
            "/foreign".into(),
            Vec::new(),
            Vec::new(),
        ));
        let mut foreign_channel = app.durable.channels[0].clone();
        foreign_channel.id = ChannelId::new();
        foreign_channel.project_id = foreign;
        foreign_channel.shared = false;
        foreign_channel.cards = vec![alias_id];
        foreign_channel.focused = None;
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards.insert(0, alias);
        durable.channels.push(foreign_channel);
        app.chat_ui = chat_ui::State::with_drafts(HashMap::from([
            (alias_id, "Keep alias draft".into()),
            (canonical, "Keep canonical draft".into()),
        ]));
        app.acknowledge_view_edit(agent.placement.clone(), None);
        drop(app.reconcile_shared_views());
        assert!(app.shared_views.blocked.contains(&agent.placement));
        assert!(app.card_state(alias_id).unwrap().existence.is_open());
        assert_eq!(
            app.sidebar_state.card_attention(alias_id),
            sidebar::AttentionSummary::default()
        );
        app.refresh_ui.focused = false;
        let mut snapshot = attention_snapshot(session);
        snapshot.notices[0].event.evidence.kind = crate::notifications::Kind::Failed;
        let notice = snapshot.notices[0].id;
        drop(app.update_notifications(Action::Snapshot(snapshot.clone())));
        assert_eq!(app.sidebar_state.project_attention(project).unread, 1);
        assert_eq!(app.sidebar_state.project_attention(project).failed, 1);
        assert_eq!(
            app.sidebar_state.project_attention(foreign),
            sidebar::AttentionSummary::default()
        );
        assert!(!app.sidebar_state.has_card_update(alias_id));
        drop(app.update(Message::Sidebar(SidebarAction::MarkChannelRead(channel))));
        drop(app.update_notifications(Action::Snapshot(snapshot)));
        assert_eq!(app.sidebar_state.project_attention(project).unread, 0);
        assert_eq!(app.sidebar_state.project_attention(project).failed, 0);
        assert!(app.notification_ui.acknowledged_for_test(notice));
        assert!(app.card_state(alias_id).unwrap().existence.is_open());
        let saved = app.persistent_workspace();
        let drafts = &saved.chat_drafts;
        assert_eq!(
            drafts.get(&alias_id).map(String::as_str),
            Some("Keep alias draft")
        );
        assert_eq!(
            drafts.get(&canonical).map(String::as_str),
            Some("Keep canonical draft")
        );
    }

    #[test]
    fn model_only_attention_reprojection_clears_failed_news_without_an_unread_target() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.refresh_ui.focused = false;
        let card = app.durable.cards[0].clone();
        let session = card_session(&card).unwrap();
        let mut snapshot = attention_snapshot(session);
        snapshot.notices[0].event.evidence.kind = crate::notifications::Kind::Failed;
        drop(app.update_notifications(Action::Snapshot(snapshot.clone())));
        assert_eq!(app.sidebar_state.card_attention(card.id).failed, 1);

        // A new open alias makes an unmanaged session ambiguous. No second
        // journal Snapshot arrives, and attribution must not choose an alias.
        let mut alias = card.clone();
        alias.id = CardId::new();
        let alias_id = alias.id;
        Arc::make_mut(&mut app.durable).cards.push(alias);
        let (unread, failed) = app.notification_attention_updates(&snapshot);
        assert!(unread.is_empty());
        assert!(failed.is_empty());
        app.reproject_notification_unread();
        assert_eq!(app.sidebar_state.card_attention(card.id).failed, 0);
        assert_eq!(app.sidebar_state.card_attention(alias_id).failed, 0);
        assert_eq!(app.notification_ui.snapshot, snapshot);
        assert_eq!(
            app.durable.cards.len(),
            2,
            "projection neither adopts nor removes"
        );

        Arc::make_mut(&mut app.durable)
            .cards
            .retain(|known| known.id != alias_id);
        app.reproject_notification_unread();
        assert_eq!(app.sidebar_state.card_attention(card.id).failed, 1);
        assert_eq!(app.notification_ui.snapshot, snapshot);
    }

    #[test]
    fn notification_snapshot_before_model_reprojects_canonical_unread_without_another_snapshot() {
        use super::super::workspace_model_sync::ModelUpdate;
        use crate::workspace_model::views::{Pane, View};
        for kind in [
            crate::notifications::Kind::Succeeded,
            crate::notifications::Kind::Failed,
        ] {
            for connected in [false, true] {
                for control in ["valid", "locally-acknowledged", "replaced-proof"] {
                    let (mut app, agent) = scheduled_attention_fixture();
                    let session = agent.console.as_ref().unwrap().id;
                    let canonical = CardId::from_uuid(agent.id.as_uuid());
                    app.workspace_model.agents.push(agent.clone());
                    drop(app.adopt_execution_agent(&agent).unwrap());
                    let card = app.card_state(canonical).unwrap().clone();
                    app.workspace_model.views = vec![View {
                        scope: agent.placement.clone(),
                        revision: 1,
                        panes: vec![
                            Pane::from_card(&card, Some(agent.spec.target.directory.clone()))
                                .unwrap(),
                        ],
                        retired: vec![],
                    }];
                    let mut alias = card.clone();
                    alias.id = CardId::new();
                    let alias_id = alias.id;
                    Arc::make_mut(&mut app.durable).cards.insert(0, alias);
                    app.chat_ui = crate::app::chat_ui::State::with_drafts(HashMap::from([
                        (alias_id, "Preserved alias draft".into()),
                        (canonical, "Preserved canonical draft".into()),
                    ]));
                    // Establish the actual initialized shared-view blocker. These
                    // two live aliases cannot be made unique by discarding a draft.
                    app.acknowledge_view_edit(agent.placement.clone(), None);
                    drop(app.reconcile_shared_views());
                    assert!(app.shared_views.blocked.contains(&agent.placement));
                    app.refresh_ui.focused = false;
                    let focus = app.workspace.focused();
                    Arc::make_mut(&mut app.durable).workspace_model_identity =
                        Some(app.workspace_model.identity);
                    let mut model = std::mem::take(&mut app.workspace_model);
                    let mut snapshot = attention_snapshot(session);
                    // A cold, already-completed source need not emit another runtime
                    // activity transition or another identical broker snapshot.
                    snapshot.notices[0].event.evidence.kind = kind;
                    let notice = snapshot.notices[0].id;
                    drop(app.update(Message::NotificationUi(Action::Snapshot(snapshot.clone()))));
                    assert!(!app.sidebar_state.has_card_update(canonical));
                    assert!(!app.sidebar_state.has_card_update(alias_id));
                    if control == "locally-acknowledged" {
                        app.notification_ui.acknowledge(notice);
                    }
                    if control == "replaced-proof" {
                        let CardKind::Terminal(replaced) = &mut model.views[0].panes[0].kind else {
                            unreachable!()
                        };
                        replaced.continuity = TerminalContinuity::Live {
                            host_session_id: crate::model::HostSessionId::new("replacement-host"),
                            boot_id: crate::model::BootId::new("replacement-boot"),
                            host_incarnation: crate::model::HostIncarnation::new(
                                "replacement-incarnation",
                            ),
                        };
                    }
                    let update = if connected {
                        ModelUpdate::Connected(model)
                    } else {
                        ModelUpdate::Read(model)
                    };
                    drop(app.update(Message::WorkspaceModelObserved(update)));
                    assert_eq!(
                        app.sidebar_state.has_card_update(canonical),
                        control == "valid"
                    );
                    assert!(!app.sidebar_state.has_card_update(alias_id));
                    assert_eq!(
                        app.sidebar_state.card_attention(canonical).failed,
                        usize::from(
                            control == "valid" && kind == crate::notifications::Kind::Failed
                        )
                    );
                    assert_eq!(
                        app.sidebar_state.card_attention(alias_id),
                        sidebar::AttentionSummary::default()
                    );
                    assert!(app.shared_views.blocked.contains(&agent.placement));
                    assert!(app.card_state(alias_id).unwrap().existence.is_open());
                    assert_eq!(app.card_state(canonical).unwrap().kind, card.kind);
                    assert_eq!(
                        app.workspace.focused(),
                        focus,
                        "unread projection cannot focus or activate"
                    );
                    assert_eq!(
                        app.notification_ui.snapshot, snapshot,
                        "no second Snapshot was delivered"
                    );
                    assert_eq!(
                        app.notification_ui.acknowledged_for_test(notice),
                        control == "locally-acknowledged"
                    );
                    assert!(app.notification_ui.session_activation.is_none());
                    assert!(!app.chat_start_pending(canonical));
                    // The exact local ACK must also survive a later model replay.
                    if control == "valid" {
                        drop(app.mark_cards_read(vec![canonical]));
                        let replay = app.workspace_model.clone();
                        drop(
                            app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(replay))),
                        );
                        assert!(!app.sidebar_state.has_card_update(canonical));
                        assert!(!app.sidebar_state.has_card_update(alias_id));
                        assert!(app.notification_ui.acknowledged_for_test(notice));
                    }
                }
            }
        }
    }

    #[test]
    fn scheduled_attention_activation_waits_for_modal_and_preserves_local_close() {
        let (mut app, agent) = scheduled_attention_fixture();
        let session = activation_session(&agent).unwrap().id();
        app.workspace_model.agents.push(agent.clone());
        app.settings_reset_confirm = true;
        drop(app.update_notifications(Action::Activated(Target::Session(session))));
        let generation = app.notification_ui.activation_generation;
        drop(app.update_notifications(Action::SessionActivationExpired {
            session,
            generation,
        }));
        assert_eq!(app.notification_ui.session_activation, Some(session));
        assert!(app.durable.cards.is_empty());
        app.settings_reset_confirm = false;
        drop(app.retry_notification_activation());
        let card = CardId::from_uuid(agent.id.as_uuid());
        assert_eq!(app.workspace.focused(), Some(card));
        let durable = Arc::make_mut(&mut app.durable);
        durable
            .cards
            .iter_mut()
            .find(|entry| entry.id == card)
            .unwrap()
            .existence = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: Some("closed locally".into()),
            },
        };
        drop(app.update_notifications(Action::Activated(Target::Session(session))));
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert_eq!(app.durable.cards.len(), 1);
        assert!(app.notification_ui.session_activation.is_none());
    }

    #[test]
    fn scheduled_attention_activation_rejects_missing_proof_ambiguous_owner_and_wrong_scope() {
        let (mut app, agent) = scheduled_attention_fixture();
        let session = activation_session(&agent).unwrap().id();
        let mut missing = agent.clone();
        missing.console.as_mut().unwrap().continuity = TerminalContinuity::RelaunchAvailable {
            recipe: missing.console.as_ref().unwrap().launch.clone(),
            last_exit: None,
        };
        app.workspace_model.agents = vec![missing];
        drop(app.update_notifications(Action::Activated(Target::Session(session))));
        assert!(app.durable.cards.is_empty());
        let mut duplicate = agent.clone();
        duplicate.id = crate::model::AgentId::new();
        app.workspace_model.agents = vec![agent.clone(), duplicate];
        drop(app.update_notifications(Action::Activated(Target::Session(session))));
        assert!(app.durable.cards.is_empty());
        let mut wrong_scope = agent;
        wrong_scope.spec.target.repository = "/other".into();
        app.workspace_model.agents = vec![wrong_scope];
        drop(app.update_notifications(Action::Activated(Target::Session(session))));
        assert!(app.durable.cards.is_empty());
    }

    #[test]
    fn missing_attention_target_does_not_scan_on_unrelated_wakes_and_expiry_is_fenced() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.context_inspector_open = false;
        let first = TerminalId::new();
        drop(app.update_notifications(Action::Activated(Target::Session(first))));
        let first_generation = app.notification_ui.activation_generation;
        assert_eq!(app.notification_ui.activation_attempts, 1);
        for _ in 0..100 {
            drop(app.retry_notification_activation());
        }
        assert_eq!(app.notification_ui.activation_attempts, 1);
        let latest = TerminalId::new();
        drop(app.update_notifications(Action::Activated(Target::Session(latest))));
        drop(app.update_notifications(Action::SessionActivationExpired {
            session: first,
            generation: first_generation,
        }));
        assert_eq!(app.notification_ui.session_activation, Some(latest));
        let generation = app.notification_ui.activation_generation;
        drop(app.update_notifications(Action::SessionActivationExpired {
            session: latest,
            generation,
        }));
        assert!(app.notification_ui.session_activation.is_none());
    }

    #[test]
    fn attention_activation_retries_when_its_project_arrives_without_card_membership_changes() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.context_inspector_open = false;
        let target = super::super::tests::terminal_card_state();
        let card = target.id;
        let session = card_session(&target).unwrap();
        let project = ProjectId::new();
        let channel = ChannelId::new();
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards.push(target);
        durable.channels.push(ProjectChannel {
            title_source: Default::default(),
            id: channel,
            project_id: project,
            title: "Arriving project".into(),
            ordinal: 0,
            shared: false,
            cards: vec![card],
            focused: Some(card),
        });
        drop(app.update_notifications(Action::Activated(Target::Session(session))));
        assert!(app.workspace.focused().is_none());
        assert_eq!(app.notification_ui.session_activation, Some(session));
        let unchanged_cards = Arc::as_ptr(&app.durable);
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "Project".into(),
            "/tmp".into(),
            Vec::new(),
            Vec::new(),
        ));
        assert_eq!(Arc::as_ptr(&app.durable), unchanged_cards);
        // Loaded inventory can change without touching pane membership. Drive
        // the actual semantic notification and common update postlude rather
        // than an unrelated attention-owner helper from the larger campaign.
        drop(app.update(Message::GitUi(crate::app::git_ui::Action::Changed)));
        assert_eq!(app.workspace.focused(), Some(card));
        assert_eq!(app.durable.active_channel, Some(channel));
        assert!(app.notification_ui.session_activation.is_none());
    }

    #[test]
    fn attention_activation_waits_for_dialog_and_latest_target_wins() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        let previous = app.workspace.focused();
        let card = super::super::tests::terminal_card_state();
        let session = card_session(&card).unwrap();
        let id = card.id;
        Arc::make_mut(&mut app.durable).cards.push(card);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.settings_reset_confirm = true;
        drop(app.update_notifications(Action::Activated(Target::Session(session))));
        assert_eq!(app.workspace.focused(), previous);
        assert_eq!(app.notification_ui.session_activation, Some(session));
        let generation = app.notification_ui.activation_generation;
        drop(app.update_notifications(Action::SessionActivationExpired {
            session,
            generation,
        }));
        assert_eq!(
            app.notification_ui.session_activation,
            Some(session),
            "a long dialog must retain an existing actor's focus request"
        );
        app.settings_reset_confirm = false;
        drop(app.retry_notification_activation());
        assert_eq!(app.workspace.focused(), Some(id));
        assert!(app.notification_ui.session_activation.is_none());
        let missing = TerminalId::new();
        drop(app.update_notifications(Action::Activated(Target::Session(missing))));
        drop(app.update_notifications(Action::Activated(Target::Session(session))));
        assert!(app.notification_ui.session_activation.is_none());
        assert_eq!(app.workspace.focused(), Some(id));
    }

    #[test]
    fn activation_selects_the_owning_channel_and_exact_pane() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let target = super::super::tests::terminal_card_state();
        let card = target.id;
        let session = card_session(&target).unwrap();
        let project = ProjectId::new();
        let channel = ChannelId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "Fixture".into(),
            "/tmp".into(),
            Vec::new(),
            Vec::new(),
        ));
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards.push(target);
        durable.channels.push(ProjectChannel {
            title_source: Default::default(),
            id: channel,
            project_id: project,
            title: "Target".into(),
            ordinal: 1,
            shared: false,
            cards: vec![card],
            focused: Some(card),
        });
        drop(app.update_notifications(Action::Activated(Target::Session(session))));
        assert_eq!(app.durable.active_channel, Some(channel));
        assert_eq!(app.workspace.focused(), Some(card));
        let count = app.durable.cards.len();
        drop(app.update_notifications(Action::Activated(Target::Session(TerminalId::new()))));
        assert_eq!(app.durable.active_channel, Some(channel));
        assert_eq!(app.workspace.focused(), Some(card));
        assert_eq!(
            app.durable.cards.len(),
            count,
            "missing targets cannot invent replacement sessions"
        );
    }
    fn lifecycle_notice_fixture() -> (Cagents, uuid::Uuid, u64, uuid::Uuid) {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.workspace_model.identity = uuid::Uuid::new_v4();
        let proof = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("lifecycle-fixture"),
            boot_id: crate::model::BootId::new("lifecycle-fixture"),
            host_incarnation: crate::model::HostIncarnation::new("lifecycle-fixture"),
        };
        drop(app.update_notifications(Action::HostBound(proof)));
        let id = uuid::Uuid::new_v4();
        let sequence = 42;
        let notice = uuid::Uuid::new_v4();
        app.notification_ui
            .snapshot
            .notices
            .push(crate::notifications::Notice {
                id: notice,
                delivered: true,
                acknowledged: false,
                window: None,
                event: crate::notifications::Event {
                    target: Target::Automation { id, sequence },
                    source_session: None,
                    provider: None,
                    directory: None,
                    destination: None,
                    content: None,
                    evidence: crate::notifications::Evidence {
                        key: format!("automation:{id}:lifecycle:{sequence}"),
                        kind: crate::notifications::Kind::AutomationBlocked,
                    },
                },
            });
        (app, id, sequence, notice)
    }

    fn exact_notice_page(id: uuid::Uuid, sequence: u64) -> crate::automations::LifecyclePage {
        crate::automations::LifecyclePage {
            events: vec![crate::automations::LifecycleEvent {
                automation: id,
                sequence,
                revision: 3,
                occurred_ms: 600,
                cause: crate::automations::LifecycleCause::PrecheckInterrupted,
                actor: crate::automations::LifecycleActor::System,
                enabled: false,
                blocked: true,
                firing: None,
            }],
            next: None,
            earlier_events_unavailable: false,
        }
    }

    #[test]
    fn no_run_notice_opens_only_exact_read_only_receipt_and_keeps_unavailable_attention() {
        for available in [true, false] {
            let (mut app, id, sequence, notice) = lifecycle_notice_fixture();
            let original = app.durable.cards.clone();
            // Drop, never poll, the real Task: these are typed completion and
            // routing tests, not daemon/RPC/native execution evidence.
            drop(app.update_notifications(Action::Activated(Target::Automation { id, sequence })));
            assert_eq!(
                app.durable.cards, original,
                "read happens before utility/state opening"
            );
            let generation = app
                .notification_ui
                .lifecycle_activation
                .as_ref()
                .unwrap()
                .generation;
            drop(app.update_notifications(Action::LifecycleResolved {
                generation,
                result: if available {
                    Ok(exact_notice_page(id, sequence))
                } else {
                    Err(())
                },
            }));
            assert!(
                app.durable
                    .cards
                    .iter()
                    .any(|card| card.kind == CardKind::Automations && card.existence.is_open())
            );
            assert!(matches!(
                app.card_state(app.workspace.focused().unwrap())
                    .unwrap()
                    .kind,
                CardKind::Automations
            ));
            assert!(app.notification_ui.lifecycle_activation.is_none());
            assert!(!app.notification_ui.acknowledged_for_test(notice));
            assert_eq!(app.notification_ui.snapshot.notices.len(), 1);
            assert!(!app.notification_ui.snapshot.notices[0].acknowledged);
            assert_eq!(
                app.durable
                    .cards
                    .iter()
                    .filter(|c| matches!(c.kind, CardKind::Chat(_) | CardKind::Terminal(_)))
                    .count(),
                original
                    .iter()
                    .filter(|c| matches!(c.kind, CardKind::Chat(_) | CardKind::Terminal(_)))
                    .count()
            );
        }
    }

    #[test]
    fn lifecycle_notice_does_not_open_over_dirty_modal_or_changed_backend_and_old_generation() {
        for changed in [
            "modal",
            "draft",
            "registry",
            "host",
            "workspace",
            "update",
            "close",
        ] {
            let (mut app, id, sequence, notice) = lifecycle_notice_fixture();
            drop(app.update_notifications(Action::Activated(Target::Automation { id, sequence })));
            let generation = app
                .notification_ui
                .lifecycle_activation
                .as_ref()
                .unwrap()
                .generation;
            match changed {
                "modal" => app.settings_reset_confirm = true,
                "draft" => {
                    drop(app.update_automation_ui(crate::automation_ui::Action::New));
                }
                "registry" => app.workspace_model.identity = uuid::Uuid::new_v4(),
                "host" => {
                    let mut proof = app.notification_ui.host.clone().unwrap();
                    proof.host_incarnation = crate::model::HostIncarnation::new("replacement");
                    drop(app.update_notifications(Action::HostBound(proof)));
                }
                "workspace" => {
                    let mut workspace = (*app.durable).clone();
                    workspace.generation += 1;
                    app.durable = Arc::new(workspace);
                }
                "update" => app.update_prepared = true,
                "close" => {
                    app.workspace
                        .request_close(app.workspace.focused().unwrap())
                        .unwrap();
                    assert!(app.workspace.pending_close().is_some());
                }
                _ => unreachable!(),
            }
            let cards = app.durable.cards.clone();
            let focused = app.workspace.focused();
            drop(app.update_notifications(Action::LifecycleResolved {
                generation,
                result: Ok(exact_notice_page(id, sequence)),
            }));
            assert_eq!(
                app.durable.cards, cards,
                "late reply mounted a utility after {changed}"
            );
            assert_eq!(app.workspace.focused(), focused);
            assert!(!app.notification_ui.acknowledged_for_test(notice));
            if changed == "draft" {
                assert!(app.automation_ui.has_draft());
            }
        }
        let (mut app, id, sequence, notice) = lifecycle_notice_fixture();
        app.settings_reset_confirm = true;
        let cards = app.durable.cards.clone();
        drop(app.update_notifications(Action::Activated(Target::Automation { id, sequence })));
        assert!(app.notification_ui.lifecycle_activation.is_none());
        assert_eq!(app.durable.cards, cards);
        app.settings_reset_confirm = false;
        drop(app.update_notifications(Action::Activated(Target::Automation { id, sequence })));
        let first = app
            .notification_ui
            .lifecycle_activation
            .as_ref()
            .unwrap()
            .generation;
        drop(
            app.update_notifications(Action::Activated(Target::Automation {
                id,
                sequence: sequence + 1,
            })),
        );
        assert!(app.notification_ui.lifecycle_activation.is_none());
        assert_eq!(app.notification_ui.lifecycle_read, Some(first));
        drop(app.update_notifications(Action::LifecycleResolved {
            generation: first,
            result: Ok(exact_notice_page(id, sequence)),
        }));
        assert!(app.notification_ui.lifecycle_activation.is_none());
        assert!(app.notification_ui.lifecycle_read.is_none());
        assert_eq!(app.durable.cards, cards);
        assert!(!app.notification_ui.acknowledged_for_test(notice));
    }
    #[test]
    fn lifecycle_activation_churn_keeps_one_actual_read_until_its_matching_completion() {
        let (mut app, id, sequence, notice) = lifecycle_notice_fixture();
        let cards = app.durable.cards.clone();
        drop(app.update_notifications(Action::Activated(Target::Automation { id, sequence })));
        let first = app.notification_ui.lifecycle_read.unwrap();
        for offset in 0..64 {
            drop(
                app.update_notifications(Action::Activated(Target::Automation {
                    id: if offset % 2 == 0 {
                        id
                    } else {
                        uuid::Uuid::new_v4()
                    },
                    sequence: sequence + offset,
                })),
            );
            assert_eq!(app.notification_ui.lifecycle_read, Some(first));
            assert!(app.notification_ui.lifecycle_activation.is_none());
        }
        let mut replacement = app.notification_ui.host.clone().unwrap();
        replacement.host_incarnation = crate::model::HostIncarnation::new("new backend");
        drop(app.update_notifications(Action::HostBound(replacement)));
        assert_eq!(app.notification_ui.lifecycle_read, Some(first));
        drop(app.update_notifications(Action::LifecycleResolved {
            generation: first + 1,
            result: Err(()),
        }));
        assert_eq!(app.notification_ui.lifecycle_read, Some(first));
        drop(app.update_notifications(Action::LifecycleResolved {
            generation: first,
            result: Ok(exact_notice_page(id, sequence)),
        }));
        assert!(app.notification_ui.lifecycle_read.is_none());
        assert_eq!(app.durable.cards, cards);
        assert!(!app.notification_ui.acknowledged_for_test(notice));
        drop(app.update_notifications(Action::Activated(Target::Automation { id, sequence })));
        let retry = app.notification_ui.lifecycle_read.unwrap();
        assert_ne!(retry, first);
        assert_eq!(
            app.notification_ui
                .lifecycle_activation
                .as_ref()
                .unwrap()
                .generation,
            retry
        );
        drop(app.update_notifications(Action::LifecycleResolved {
            generation: first,
            result: Err(()),
        }));
        assert_eq!(app.notification_ui.lifecycle_read, Some(retry));
        drop(app.update_notifications(Action::LifecycleResolved {
            generation: retry,
            result: Ok(exact_notice_page(id, sequence)),
        }));
        assert!(app.notification_ui.lifecycle_read.is_none());
        assert!(
            app.durable
                .cards
                .iter()
                .any(|card| card.kind == CardKind::Automations)
        );
        assert!(!app.notification_ui.acknowledged_for_test(notice));
    }
}

#[derive(Clone)]
struct Source(async_channel::Receiver<Action>);
impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        self.0.same_channel(&other.0)
    }
}
impl Eq for Source {}
impl std::hash::Hash for Source {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::any::TypeId::of::<Self>().hash(state);
    }
}
fn stream(source: &Source) -> Pin<Box<dyn iced::futures::Stream<Item = Action> + Send>> {
    Box::pin(iced::futures::stream::unfold(
        source.0.clone(),
        |receiver| async move { receiver.recv().await.ok().map(|action| (action, receiver)) },
    ))
}

#[cfg(test)]
mod journal_tests;
