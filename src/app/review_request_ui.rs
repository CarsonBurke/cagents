//! Shell routing for the shared, bounded review-request coordinator.
use super::*;
pub use crate::pull_request_review::request_ui::monitor::Action;
pub(super) type State = crate::pull_request_review::request_ui::monitor::Monitor;

const BACKGROUND_REVIEW_INTERVAL: Duration = Duration::from_secs(5 * 60);

fn review_interval(focused: bool, daemon_unavailable: bool) -> Duration {
    if !focused {
        BACKGROUND_REVIEW_INTERVAL
    } else if daemon_unavailable {
        Duration::from_secs(30)
    } else {
        Duration::from_secs(10)
    }
}

impl Cagents {
    pub(super) fn review_request_subscription(&self) -> Subscription<Message> {
        if self
            .github_availability
            .values()
            .all(|availability| availability.repository_key().is_none())
        {
            return Subscription::none();
        }
        cosmic::iced::time::every(review_interval(
            self.refresh_ui.focused,
            crate::daemon_health::current().unavailable(),
        ))
        .map(|_| Message::ReviewRequests(Action::Tick))
    }

    pub(super) fn update_review_requests(&mut self, action: Action) -> Task<Message> {
        let repositories = self
            .github_availability
            .iter()
            .filter_map(|(&project, availability)| {
                availability
                    .repository_key()
                    .cloned()
                    .map(|key| (project, key))
            })
            .collect::<HashMap<_, _>>();
        let task = self
            .refresh_ui
            .review_requests
            .update(action, repositories.values().cloned())
            .map(Message::ReviewRequests)
            .map(cosmic::Action::App);
        let requests = if crate::daemon_health::current().unavailable() {
            Vec::new()
        } else {
            self.refresh_ui.review_requests.take_observed()
        };
        let report = if requests.is_empty()
            || !self.settings.notifications.enabled
            || !self.settings.notifications.review_requests
        {
            Task::none()
        } else {
            iced::Task::perform(
                crate::notifications::review::report(
                    crate::notifications::review::ObservedRequests {
                        destination: crate::notifications::Destination {
                            application: crate::notifications::Application::Cagents,
                            window: instance::window_slot().into(),
                        },
                        requests,
                    },
                ),
                |result| Message::NotificationUi(notification_ui::Action::ReviewsReported(result)),
            )
            .map(cosmic::Action::App)
        };
        for project in &mut self.sidebar_model.projects {
            let snapshot = repositories.get(&project.id).and_then(|repository| {
                self.refresh_ui
                    .review_requests
                    .snapshots()
                    .find(|(key, _)| key.same_repository(repository))
                    .map(|(_, snapshot)| snapshot)
            });
            if let Some(snapshot) = snapshot {
                project.apply_review_requests(&snapshot.requested);
            } else {
                project.apply_review_requests(&Default::default());
            }
        }
        for runtime in self.cards.values_mut() {
            if let CardRuntime::PullRequests(pane) = runtime {
                pane.review.clear_external_review_requests();
                for (repository, snapshot) in self.refresh_ui.review_requests.snapshots() {
                    pane.review.apply_review_requests(repository, snapshot);
                }
            }
        }
        self.review_alert_focus_changed();
        Task::batch([task, report])
    }
}

#[cfg(test)]
mod polling_tests {
    use super::*;

    #[test]
    fn unfocused_review_polling_backs_off_even_when_daemon_is_healthy() {
        assert_eq!(review_interval(false, false), Duration::from_secs(300));
        assert_eq!(review_interval(false, true), Duration::from_secs(300));
        assert_eq!(review_interval(true, false), Duration::from_secs(10));
        assert_eq!(review_interval(true, true), Duration::from_secs(30));
    }
}
