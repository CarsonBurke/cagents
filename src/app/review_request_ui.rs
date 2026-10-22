//! Shell routing for the shared, bounded review-request coordinator.
use super::*;
pub use crate::pull_request_review::request_ui::monitor::Action;
pub(super) type State = crate::pull_request_review::request_ui::monitor::Monitor;

/// How long the monitor may wait to start a read that has fallen due. A
/// settled repository falls due once a minute however often this ticks, and
/// a tick with nothing due changes nothing.
const FOCUSED_INTERVAL: Duration = Duration::from_secs(10);
/// A daemon that cannot take alerts leaves only this window's own flags to
/// keep current.
const UNAVAILABLE_INTERVAL: Duration = Duration::from_secs(30);
/// Out of focus, discovery mostly serves the desktop alerts, which exist for
/// someone looking elsewhere. Two minutes keeps those on a human timescale,
/// twice the monitor's per-repository refresh, while waking the window a
/// twelfth as often. Regaining focus reads whatever has fallen due at once.
const BACKGROUND_INTERVAL: Duration = Duration::from_secs(2 * 60);

fn review_interval(focused: bool, daemon_unavailable: bool) -> Duration {
    if !focused {
        BACKGROUND_INTERVAL
    } else if daemon_unavailable {
        UNAVAILABLE_INTERVAL
    } else {
        FOCUSED_INTERVAL
    }
}

impl Cagents {
    fn watches_review_requests(&self) -> bool {
        self.github_availability
            .values()
            .any(|availability| availability.repository_key().is_some())
    }

    pub(super) fn review_request_subscription(&self) -> Subscription<Message> {
        if !self.watches_review_requests() {
            return Subscription::none();
        }
        cosmic::iced::time::every(review_interval(
            self.refresh_ui.focused,
            crate::daemon_health::current().unavailable(),
        ))
        .map(|_| Message::ReviewRequests(Action::Tick))
    }

    /// The interval timer restarts at its full length when focus returns, so
    /// whatever fell due while the window ticked slowly is read now instead.
    pub(super) fn review_requests_focus_regained(&mut self) -> Task<Message> {
        if !self.watches_review_requests() {
            return Task::none();
        }
        self.update_review_requests(Action::Tick)
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
        let revision = self.refresh_ui.review_requests.revision();
        let task = self
            .refresh_ui
            .review_requests
            .update(action, repositories.values().cloned())
            .map(Message::ReviewRequests)
            .map(cosmic::Action::App);
        let revised = self.refresh_ui.review_requests.revision() != revision;
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
        // The review an alert's focus target names comes from the monitor's
        // snapshots alone; most ticks leave them as they were.
        if revised {
            self.review_alert_focus_changed();
        }
        Task::batch([task, report])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watching_one_repository() -> Cagents {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.github_availability.insert(
            ProjectId::new(),
            GitHubAvailability::Ready {
                repository_key: GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
                pull_requests: Vec::new(),
                refreshed_at: 1,
            },
        );
        app
    }

    #[test]
    fn an_unfocused_window_backs_off_even_when_the_daemon_is_healthy() {
        assert_eq!(review_interval(true, false), Duration::from_secs(10));
        assert_eq!(review_interval(true, true), Duration::from_secs(30));
        assert_eq!(review_interval(false, false), BACKGROUND_INTERVAL);
        assert_eq!(review_interval(false, true), BACKGROUND_INTERVAL);
        assert!(BACKGROUND_INTERVAL >= Duration::from_secs(60));
    }

    #[test]
    fn a_tick_that_changes_nothing_does_not_republish_review_presence() {
        let mut app = watching_one_repository();
        drop(app.update_review_requests(Action::Tick));
        assert!(app.refresh_ui.review_requests.reading());
        assert!(
            app.notification_ui.take_pending_presence().is_some(),
            "a newly watched repository can change what a focused review names"
        );
        for _ in 0..3 {
            drop(app.update_review_requests(Action::Tick));
        }
        assert!(app.notification_ui.take_pending_presence().is_none());
    }

    #[test]
    fn regaining_focus_looks_for_review_requests_at_once() {
        let mut app = watching_one_repository();
        let main = window::Id::RESERVED;
        app.core.set_main_window_id(Some(main));
        app.refresh_ui.focused = true;
        for (window, focused) in [(window::Id::unique(), true), (main, true), (main, false)] {
            drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
                window, focused,
            ))));
            assert!(
                !app.refresh_ui.review_requests.reading(),
                "only regaining focus starts a read"
            );
        }
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(main, true))));
        assert!(app.refresh_ui.review_requests.reading());
    }
}
