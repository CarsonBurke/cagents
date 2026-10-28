//! Standalone editor routing for the shared daemon notification broker.
use super::*;
use cagents::notifications::{self, Target, review::ReviewRequest};

#[derive(Debug, Clone)]
pub enum Action {
    Observed(notifications::ui::Action),
    Focus(notifications::ui::Action),
    Verified {
        request: ReviewRequest,
        result: Result<(), String>,
    },
    Reported(Result<(), String>),
}

#[derive(Default)]
pub(super) struct State {
    pub observer: notifications::ui::Observer,
    snapshot: notifications::Snapshot,
    pending: Option<ReviewRequest>,
    verifying: bool,
}

impl Editor {
    pub(super) fn notification_action(&mut self, action: Action) -> Task<Message> {
        let next = if matches!(action, Action::Observed(_)) {
            self.notifications
                .observer
                .next()
                .map(|action| Message::Notification(Action::Observed(action)))
                .map(cosmic::Action::App)
        } else {
            Task::none()
        };
        let task = match action {
            Action::Observed(notifications::ui::Action::Snapshot(snapshot)) => {
                self.notifications.snapshot = snapshot;
                Task::none()
            }
            Action::Observed(notifications::ui::Action::Activated(Target::PullRequest(
                request,
            ))) => {
                self.notifications.pending = Some(request);
                self.notifications.verifying = false;
                self.open_requested_review()
            }
            Action::Observed(notifications::ui::Action::Activated(
                Target::Session(_)
                | Target::SessionInbox(_)
                | Target::AgentRun(_)
                | Target::Automation { .. }
                | Target::AutomationProposal { .. },
            )) => Task::none(),
            Action::Observed(notifications::ui::Action::ActivateWindow(token)) => iced::Task::done(
                cosmic::Action::Cosmic(cosmic::app::Action::Activate(token.unwrap_or_default())),
            ),
            Action::Observed(notifications::ui::Action::Failed(error))
            | Action::Focus(notifications::ui::Action::Failed(error))
            | Action::Reported(Err(error)) => {
                self.notice = Some(format!("Review alerts: {error}"));
                Task::none()
            }
            Action::Verified { request, result } => {
                if self.notifications.pending.as_ref() != Some(&request) {
                    return Task::none();
                }
                self.notifications.verifying = false;
                if let Err(error) = result {
                    self.notifications.pending = None;
                    self.notice = Some(format!("Review alerts: {error}"));
                    Task::none()
                } else {
                    self.open_verified_review(request)
                }
            }
            Action::Reported(Ok(())) | Action::Focus(_) => Task::none(),
        };
        Task::batch([next, task])
    }

    fn requested_review_tab(&self, request: &ReviewRequest) -> Option<CardId> {
        self.tabs
            .iter()
            .find(|tab| {
                tab.review
                    .review_request_repository()
                    .is_some_and(|key| key.same_repository(&request.repository_key()))
            })
            .map(|tab| tab.id)
    }

    pub(super) fn open_requested_review(&mut self) -> Task<Message> {
        let Some(request) = self.notifications.pending.as_ref() else {
            return Task::none();
        };
        if self.modal_open() || self.notifications.verifying {
            return Task::none();
        }
        if self.requested_review_tab(request).is_none() {
            self.notice = Some("Open this review's repository to continue. The alert will be checked against your current GitHub account before opening.".into());
            return Task::none();
        }
        self.notifications.verifying = true;
        let request = request.clone();
        let checked = request.clone();
        iced::Task::perform(notifications::review::verify(checked), move |result| {
            Message::Notification(Action::Verified {
                request: request.clone(),
                result: result.map(|_| ()),
            })
        })
        .map(cosmic::Action::App)
    }

    fn open_verified_review(&mut self, request: ReviewRequest) -> Task<Message> {
        if self.modal_open() {
            return Task::none();
        }
        let Some(id) = self.requested_review_tab(&request) else {
            return Task::none();
        };
        self.notifications.pending = None;
        self.sidebar_open = true;
        self.sidebar_panel = WorktreePanel::PullRequests;
        let focus = self.select_tab(id);
        let review =
            self.handle_review(id, pull_request_review::Action::OpenNumber(request.number));
        let ids = self
            .notifications
            .snapshot
            .notices
            .iter()
            .filter(|notice| notice.event.target == Target::PullRequest(request.clone()))
            .map(|notice| notice.id)
            .collect::<Vec<_>>();
        let authority = cagents::agent_execution::Authority::from_environment();
        let acknowledge = iced::Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    // Equality tests scope only; never retries with this User value.
                    if authority != cagents::agent_execution::Authority::default()
                        && cagents::device::is_remote()
                    {
                        return Err(
                            "A local session capability cannot use remote notification APIs"
                                .to_owned(),
                        );
                    }
                    let socket = cagents::terminal_ipc::daemon_socket_path()
                        .map_err(|error| error.to_string())?;
                    let mut client = cagents::control::ControlClient::connect(&socket)
                        .map_err(|error| error.to_string())?;
                    client
                        .authenticate(authority)
                        .map_err(|error| error.to_string())?;
                    for id in ids {
                        client
                            .acknowledge_notification(id)
                            .map_err(|error| error.to_string())?;
                    }
                    Ok(())
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result)
            },
            |result| Message::Notification(Action::Reported(result)),
        )
        .map(cosmic::Action::App);
        Task::batch([
            focus,
            review,
            acknowledge,
            iced::Task::done(cosmic::Action::Cosmic(cosmic::app::Action::Activate(
                String::new(),
            ))),
        ])
    }

    pub(super) fn report_review_requests(&mut self) -> Task<Message> {
        let requests = self.review_requests.take_observed();
        if !self.settings.notifications.enabled || !self.settings.notifications.review_requests {
            return self.open_requested_review();
        }
        if let Err(error) = self.notifications.observer.start() {
            self.notice = Some(format!("Review alerts: {error}"));
            return self.open_requested_review();
        }
        let report = if requests.is_empty() {
            Task::none()
        } else {
            iced::Task::perform(
                notifications::review::report(notifications::review::ObservedRequests {
                    destination: self.notifications.observer.destination(),
                    requests,
                }),
                |result| Message::Notification(Action::Reported(result)),
            )
            .map(cosmic::Action::App)
        };
        Task::batch([
            report,
            self.notification_focus(),
            self.open_requested_review(),
        ])
    }

    pub(super) fn notification_focus(&self) -> Task<Message> {
        let target = self
            .window_focused
            .then_some(self.active)
            .flatten()
            .and_then(|id| {
                let tab = self.tabs.iter().find(|tab| tab.id == id)?;
                if !self.sidebar_open || self.sidebar_panel != WorktreePanel::PullRequests {
                    return None;
                }
                let target = tab.review.target()?;
                self.review_requests
                    .target(&target.repository, target.pull_request.number)
            });
        let changed = self.notifications.observer.focused_target() != target;
        let read = notifications::focused_notice_ids(
            &self.notifications.snapshot,
            target.as_ref(),
            None,
            changed,
            self.settings.notifications.while_focused,
        );
        iced::Task::batch([
            self.notifications.observer.focus(target),
            self.notifications.observer.acknowledge(read),
        ])
        .map(|action| Message::Notification(Action::Focus(action)))
        .map(cosmic::Action::App)
    }
}
