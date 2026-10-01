//! Shell routing for the shared, bounded review-request coordinator.
use super::*;
pub use crate::pull_request_review::request_ui::monitor::Action;
pub(super) type State = crate::pull_request_review::request_ui::monitor::Monitor;

impl Cagents {
    pub(super) fn review_request_subscription(&self) -> Subscription<Message> {
        if self
            .github_availability
            .values()
            .all(|availability| availability.repository_key().is_none())
        {
            return Subscription::none();
        }
        cosmic::iced::time::every(Duration::from_secs(10))
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
        let requests = self.refresh_ui.review_requests.take_observed();
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
