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

#[derive(Default)]
struct Pending {
    presence: Option<Presence>,
    preferences: Option<Preferences>,
    acknowledged: BTreeSet<uuid::Uuid>,
}

pub(super) struct State {
    pub snapshot: Snapshot,
    presence: Arc<Mutex<Presence>>,
    pending: Arc<Mutex<Pending>>,
    wake: Option<crossbeam_channel::Sender<()>>,
    receiver: async_channel::Receiver<Action>,
    last_workspace: Weak<WorkspaceState>,
    last_focus: Option<(Option<CardId>, bool)>,
    acknowledged: BTreeSet<uuid::Uuid>,
    /// Whether a test notification is waiting on the desktop's answer.
    pub testing: bool,
    inbox_activation: Option<(crate::agent_execution::Lineage, uuid::Uuid)>,
    session_activation: Option<TerminalId>,
    activation_generation: u64,
    activation_attempt: Option<(Weak<WorkspaceState>, bool)>,
    #[cfg(test)]
    activation_attempts: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            snapshot: Snapshot::default(),
            presence: Arc::new(Mutex::new(Presence::default())),
            pending: Arc::new(Mutex::new(Pending::default())),
            wake: None,
            receiver: async_channel::bounded(32).1,
            last_workspace: Weak::new(),
            last_focus: None,
            acknowledged: BTreeSet::new(),
            testing: false,
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

    pub(super) fn acknowledge(&mut self, id: uuid::Uuid) {
        if self.acknowledged.insert(id) {
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

    /// Explicit reads acknowledge the durable journal as well as local badges.
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

    pub(super) fn sync_notification_presence(&mut self) {
        let focus = (self.workspace.focused(), self.refresh_ui.focused);
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
            .1
            .then_some(focus.0)
            .flatten()
            .and_then(|id| self.card_state(id))
            .filter(|card| self.pane_is_active(card.id))
            .and_then(card_session);
        let focused_target = focus.1.then_some(focus.0).flatten().and_then(|id| {
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
        let read = crate::notifications::focused_notice_ids(
            &self.notification_ui.snapshot,
            presence.focused_target.as_ref(),
            focused,
            focus_changed,
            self.settings.notifications.while_focused,
        );
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

    pub(super) fn update_notifications(&mut self, action: Action) -> Task<Message> {
        if matches!(&action, Action::Activated(target) if !matches!(target, Target::SessionInbox(_)))
        {
            self.notification_ui.inbox_activation = None;
        }
        if matches!(&action, Action::Activated(_)) {
            self.notification_ui.session_activation = None;
            self.notification_ui.activation_attempt = None;
            self.notification_ui.activation_generation =
                self.notification_ui.activation_generation.wrapping_add(1);
        }
        match action {
            Action::Snapshot(snapshot) => {
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
                for notice in &snapshot.notices {
                    if !notice.acknowledged
                        && !self.notification_ui.acknowledged.contains(&notice.id)
                        && let Some(session) = notice.event.source_session()
                        && let Some(card) = self
                            .durable
                            .cards
                            .iter()
                            .find(|card| {
                                card.existence.is_open() && card_session(card) == Some(session)
                            })
                            .map(|card| card.id)
                    {
                        self.observe_agent_update_for_card(card);
                    }
                }
                self.notification_ui.snapshot = snapshot;
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
                    && !self.durable.cards.iter().any(|card| card.existence.is_open() && card_session(card) == Some(session))
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

    /// Retry after lifecycle/model updates and dialog dismissal, never by spawning
    /// a replacement provider. Explicitly closed cards remain closed.
    pub(super) fn retry_notification_activation(&mut self) -> Task<Message> {
        let Some(session) = self.notification_ui.session_activation else {
            return Task::none();
        };
        let blocked = self.workspace_inputs_blocked()
            || self.focused_repository_is_saving()
            || self.update_prepared;
        if self
            .notification_ui
            .activation_attempt
            .as_ref()
            .is_some_and(|(workspace, was_blocked)| {
                workspace.as_ptr() == Arc::as_ptr(&self.durable) && *was_blocked == blocked
            })
        {
            return Task::none();
        }
        self.notification_ui.activation_attempt = Some((Arc::downgrade(&self.durable), blocked));
        if blocked {
            return Task::none();
        }
        #[cfg(test)]
        {
            self.notification_ui.activation_attempts += 1;
        }
        let card = self
            .durable
            .cards
            .iter()
            .find(|card| card.existence.is_open() && card_session(card) == Some(session));
        let Some(id) = card.map(|card| card.id) else {
            if self
                .durable
                .cards
                .iter()
                .any(|card| card_session(card) == Some(session))
            {
                self.notification_ui.session_activation = None;
                self.notification_ui.activation_attempt = None;
            }
            return Task::none();
        };
        let focus = self.focus_attention_card(id);
        if self.workspace.focused() == Some(id) {
            self.notification_ui.session_activation = None;
            self.notification_ui.activation_attempt = None;
        }
        focus
    }

    pub(super) fn notification_subscription(&self) -> Subscription<Message> {
        Subscription::run_with(Source(self.notification_ui.receiver.clone()), stream)
            .map(Message::NotificationUi)
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
    use super::*;

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
    use crate::{
        agent::Provider,
        model::{ChannelId, ProjectChannel},
    };

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
    fn marking_a_channel_read_acknowledges_the_journal_before_the_next_snapshot() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.refresh_ui.focused = false;
        let card = app.workspace.focused().unwrap();
        let session = card_session(&app.durable.cards[0]).unwrap();
        let channel = crate::model::ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(crate::model::ProjectChannel {
                title_source: Default::default(),
                id: channel,
                project_id: ProjectId::new(),
                title: "Read fixture".into(),
                ordinal: 0,
                shared: false,
                cards: vec![card],
                focused: Some(card),
            });
        let id = uuid::Uuid::new_v4();
        let snapshot = Snapshot {
            notices: vec![crate::notifications::Notice {
                id,
                event: crate::notifications::Event {
                    target: Target::Session(session),
                    source_session: Some(session),
                    provider: Some(Provider::Claude),
                    directory: Some("/tmp".into()),
                    destination: None,
                    evidence: crate::notifications::Evidence {
                        key: "channel-complete".into(),
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
        assert!(app.sidebar_state.has_card_update(card));
        drop(app.update(Message::Sidebar(SidebarAction::MarkChannelRead(channel))));
        assert!(!app.sidebar_state.has_card_update(card));
        assert!(
            app.notification_ui
                .pending
                .lock()
                .unwrap()
                .acknowledged
                .contains(&id)
        );
        drop(app.update_notifications(Action::Snapshot(snapshot)));
        assert!(!app.sidebar_state.has_card_update(card));
    }

    #[test]
    fn background_window_keeps_attention_until_its_agent_is_focused() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let session = card_session(&app.durable.cards[0]).unwrap();
        let id = uuid::Uuid::new_v4();
        let notice = crate::notifications::Notice {
            id,
            event: crate::notifications::Event {
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
        app.refresh_ui.focused = true;
        app.sync_notification_presence();
        let pending = app.notification_ui.pending.lock().unwrap();
        assert_eq!(pending.presence.as_ref().unwrap().focused, Some(session));
        assert_eq!(pending.acknowledged, BTreeSet::from([id]));
    }

    #[test]
    fn opted_in_focused_alert_survives_snapshot_and_is_read_on_return() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.settings.notifications.while_focused = true;
        app.refresh_ui.focused = true;
        app.sync_notification_presence();
        let session = card_session(&app.durable.cards[0]).unwrap();
        let id = uuid::Uuid::new_v4();
        let snapshot = Snapshot {
            notices: vec![crate::notifications::Notice {
                id,
                event: crate::notifications::Event {
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

        app.refresh_ui.focused = false;
        app.sync_notification_presence();
        app.refresh_ui.focused = true;
        app.sync_notification_presence();
        assert_eq!(
            app.notification_ui.pending.lock().unwrap().acknowledged,
            BTreeSet::from([id])
        );
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
        drop(app.update_notifications(Action::SessionActivationExpired { session, generation }));
        assert_eq!(app.notification_ui.session_activation, Some(session), "a long dialog must retain an existing actor's focus request");
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
    fn marking_all_read_retains_channel_until_attention_filter_reentry() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let channel = ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                title_source: Default::default(),
                id: channel,
                project_id: ProjectId::new(),
                title: "Unread".into(),
                ordinal: 0,
                shared: false,
                cards: vec![card],
                focused: Some(card),
            });
        app.sidebar_state
            .apply(&SidebarAction::SetFilter(sidebar::SidebarFilter::Attention));
        app.sidebar_state.observe_card_update(card);
        drop(app.mark_cards_read(vec![card]));
        assert!(!app.sidebar_state.has_card_update(card));
        assert!(
            app.sidebar_state
                .attention_retained(sidebar::SelectionRow::Channel(channel))
        );
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
