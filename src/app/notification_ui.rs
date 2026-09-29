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
    ReviewVerified {
        request: crate::notifications::review::ReviewRequest,
        result: Result<(), String>,
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
    pending_review: Option<crate::notifications::review::ReviewRequest>,
    verifying_review: bool,
    presence: Arc<Mutex<Presence>>,
    pending: Arc<Mutex<Pending>>,
    wake: Option<crossbeam_channel::Sender<()>>,
    receiver: async_channel::Receiver<Action>,
    last_workspace: Weak<WorkspaceState>,
    last_focus: Option<(Option<CardId>, bool)>,
    acknowledged: BTreeSet<uuid::Uuid>,
    /// Whether a test notification is waiting on the desktop's answer.
    pub testing: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            snapshot: Snapshot::default(),
            pending_review: None,
            verifying_review: false,
            presence: Arc::new(Mutex::new(Presence::default())),
            pending: Arc::new(Mutex::new(Pending::default())),
            wake: None,
            receiver: async_channel::bounded(32).1,
            last_workspace: Weak::new(),
            last_focus: None,
            acknowledged: BTreeSet::new(),
            testing: false,
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
                    thread::sleep(Duration::from_secs(2));
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
                            match receiver.recv_timeout(Duration::from_secs(2)) {
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

    pub(super) fn open_pending_review_alert(&mut self) -> Task<Message> {
        let Some(request) = self.notification_ui.pending_review.as_ref() else {
            return Task::none();
        };
        if self.notification_ui.verifying_review || self.modal_is_open() {
            return Task::none();
        }
        if !self.github_availability.values().any(|availability| {
            availability
                .repository_key()
                .is_some_and(|key| key.same_repository(&request.repository_key()))
        }) {
            self.status = Some(TransientNotice::warning(
                "Open this review's repository to continue. The alert will be checked against your current GitHub account before opening.",
            ));
            return Task::none();
        }
        self.notification_ui.verifying_review = true;
        let request = request.clone();
        let checked = request.clone();
        iced::Task::perform(
            crate::notifications::review::verify(checked),
            move |result| {
                Message::NotificationUi(Action::ReviewVerified {
                    request: request.clone(),
                    result: result.map(|_| ()),
                })
            },
        )
        .map(cosmic::Action::App)
    }

    pub(super) fn review_alert_focus_changed(&mut self) {
        self.notification_ui.last_focus = None;
        self.sync_notification_presence();
    }

    pub(super) fn update_notifications(&mut self, action: Action) -> Task<Message> {
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
                self.notification_ui.pending_review = Some(request);
                self.notification_ui.verifying_review = false;
                Task::batch([
                    self.activate_main_window(None),
                    self.open_pending_review_alert(),
                ])
            }
            Action::ReviewVerified { request, result } => {
                if self.notification_ui.pending_review.as_ref() != Some(&request) {
                    return Task::none();
                }
                self.notification_ui.verifying_review = false;
                if let Err(error) = result {
                    self.notification_ui.pending_review = None;
                    self.status = Some(TransientNotice::warning(error));
                    return self.activate_main_window(None);
                }
                let project =
                    self.github_availability
                        .iter()
                        .find_map(|(project, availability)| {
                            availability
                                .repository_key()
                                .is_some_and(|key| key.same_repository(&request.repository_key()))
                                .then_some(*project)
                        });
                if self.modal_is_open() {
                    self.status = Some(TransientNotice::warning(
                        "Finish the open dialog, then open this review from Pull requests.",
                    ));
                    return self.activate_main_window(None);
                }
                let Some(project) = project else {
                    self.status = Some(TransientNotice::warning(
                        "This review's repository is no longer open. Open it to continue reviewing.",
                    ));
                    return self.activate_main_window(None);
                };
                self.notification_ui.pending_review = None;
                let target = Target::PullRequest(request.clone());
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
                let open = self.review_pull_request(project, request.number);
                Task::batch([self.activate_main_window(None), open])
            }
            Action::Activated(Target::Session(session)) => {
                let Some(card) = self
                    .durable
                    .cards
                    .iter()
                    .find(|card| card.existence.is_open() && card_session(card) == Some(session))
                    .cloned()
                else {
                    self.status = Some(TransientNotice::warning(
                        "This agent's pane is no longer open. Its attention record remains available.",
                    ));
                    return self.activate_main_window(None);
                };
                if self.modal_is_open() {
                    self.status = Some(TransientNotice::warning(
                        "Finish the open dialog, then select the agent's highlighted pane.",
                    ));
                    return self.activate_main_window(None);
                }
                let channel = self
                    .durable
                    .channels
                    .iter()
                    .find(|channel| channel.cards.contains(&card.id))
                    .map(|channel| channel.id);
                let scope = if let Some(channel) = channel {
                    self.select_channel(channel)
                } else {
                    let directory = self.durable.card_scopes.get(&card.id).cloned().or_else(|| {
                        match &card.kind {
                            CardKind::Terminal(session) => {
                                Some(session.launch.working_directory.clone())
                            }
                            CardKind::Chat(session) => {
                                Some(session.launch.recipe.working_directory.clone())
                            }
                            _ => None,
                        }
                    });
                    directory.map_or_else(Task::none, |directory| {
                        self.select_worktree(directory, false)
                    })
                };
                let focus = self.workspace.focus(card.id).map_or_else(
                    |_| Task::none(),
                    |update| self.apply_viewport(update, true, false),
                );
                Task::batch([scope, focus, self.activate_main_window(None)])
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

fn card_session(card: &CardState) -> Option<TerminalId> {
    match &card.kind {
        CardKind::Terminal(session) => Some(session.id),
        CardKind::Chat(session) => Some(session.id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
