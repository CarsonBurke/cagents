//! Shell routing for the shared, bounded review-request coordinator.
use super::*;
use crate::pull_request_review::request_ui::monitor;
pub use monitor::Action;
pub(super) type State = crate::pull_request_review::request_ui::monitor::Monitor;

/// A daemon that cannot take alerts leaves only a focused window's own flags
/// to keep current.
const UNAVAILABLE_INTERVAL: Duration = Duration::from_secs(30);

fn review_interval(focused: bool, daemon_unavailable: bool) -> Duration {
    if focused && daemon_unavailable {
        UNAVAILABLE_INTERVAL
    } else {
        monitor::tick_interval(focused)
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
                pane.review
                    .sync_review_requests(self.refresh_ui.review_requests.snapshots());
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
impl Cagents {
    /// Completes the monitor's next read as GitHub naming `requested` as the
    /// pull requests awaiting the viewer's review.
    pub(super) fn complete_review_read_for_test(&mut self, requested: &[u64]) {
        use crate::github::review_requests::{Current, Entry, Page, Viewer, scan::Work};
        self.refresh_ui.review_requests.make_due_for_test();
        drop(self.update_review_requests(Action::Tick));
        let (repository, generation) = self
            .refresh_ui
            .review_requests
            .in_flight_for_test()
            .cloned()
            .expect("a review read started");
        let entries = requested
            .iter()
            .map(|&number| Entry {
                number,
                title: format!("Pull request {number}"),
                url: format!(
                    "https://{}/{}/{}/pull/{number}",
                    repository.host, repository.owner, repository.name
                )
                .parse()
                .unwrap(),
                viewer_latest_review_request: Some(Current {
                    id: format!("request-{number}"),
                    as_code_owner: false,
                    requested_reviewer: None,
                }),
            })
            .collect();
        drop(self.update_review_requests(Action::Loaded {
            repository,
            generation,
            work: Work::Page { cursor: None },
            result: Ok(crate::pull_request_review::request_ui::Reply::Page(Page {
                viewer: Viewer {
                    id: "viewer".into(),
                    login: "viewer".into(),
                },
                entries,
                next_cursor: None,
            })),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watching_one_repository() -> Cagents {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        app.github_availability.insert(
            project,
            GitHubAvailability::Ready {
                repository_key: GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
                pull_requests: Vec::new(),
                refreshed_at: 1,
            },
        );
        let row = sidebar::SidebarPullRequest::new(
            project,
            7,
            "Pull request 7".into(),
            "https://github.com/owner/repo/pull/7".parse().unwrap(),
            "feature".into(),
            None,
            false,
            sidebar::CheckStatus::Unknown,
        );
        let worktree = SidebarWorktree::from_durable(
            crate::model::WorktreeId::new(),
            "/repo/feature".into(),
            Some("feature".into()),
        );
        let mut project = SidebarProject::new(
            project,
            crate::repository::RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            vec![worktree],
            vec![row.clone()],
        );
        project.worktrees[0].set_pull_request(row);
        app.sidebar_model.projects.push(project);
        app
    }

    #[test]
    fn an_identical_completed_read_leaves_projects_and_presence_alone() {
        let mut app = watching_one_repository();
        app.complete_review_read_for_test(&[7]);
        let project = &app.sidebar_model.projects[0];
        assert!(project.pull_requests[0].needs_review);
        assert!(project.worktrees[0].pull_request().unwrap().needs_review);
        assert!(
            app.notification_ui
                .take_pending_presence_for_test()
                .is_some()
        );

        let snapshot = |app: &Cagents| {
            let (_, snapshot) = app
                .refresh_ui
                .review_requests
                .snapshots()
                .next()
                .expect("the watched repository's snapshot");
            Arc::clone(snapshot)
        };
        // A worktree's row is replaced whole, so a rewrite of it, even with
        // equal contents, shows up as a new title allocation.
        let worktree_row = |app: &Cagents| {
            app.sidebar_model.projects[0].worktrees[0]
                .pull_request()
                .expect("the worktree's pull request")
                .title
                .as_ptr()
        };
        let projects = app.sidebar_model.projects.clone();
        let revision = app.refresh_ui.review_requests.revision();
        let shown = snapshot(&app);
        let row = worktree_row(&app);
        app.complete_review_read_for_test(&[7]);
        assert_eq!(app.refresh_ui.review_requests.revision(), revision);
        assert!(
            Arc::ptr_eq(&snapshot(&app), &shown),
            "a read that found the same requests keeps the snapshot every view shares"
        );
        assert_eq!(worktree_row(&app), row, "the worktree's row is not rebuilt");
        assert_eq!(app.sidebar_model.projects, projects);
        assert!(
            app.notification_ui
                .take_pending_presence_for_test()
                .is_none(),
            "a read that found nothing new is not republished"
        );

        app.complete_review_read_for_test(&[]);
        assert!(!app.sidebar_model.projects[0].pull_requests[0].needs_review);
        assert!(
            app.notification_ui
                .take_pending_presence_for_test()
                .is_some()
        );
    }

    /// How the runtime tells subscriptions apart: a timer whose interval
    /// changes hashes differently, and is restarted at the new interval.
    fn recipe_identities(subscription: Subscription<Message>) -> Vec<u64> {
        use cosmic::iced::advanced::subscription::{Hasher, Recipe, into_recipes};
        use std::hash::Hasher as _;
        into_recipes(subscription)
            .iter()
            .map(|recipe| {
                let mut hasher = Hasher::default();
                Recipe::hash(&**recipe, &mut hasher);
                hasher.finish()
            })
            .collect()
    }

    #[test]
    fn an_unfocused_window_backs_off_even_when_the_daemon_is_healthy() {
        assert_eq!(review_interval(true, false), Duration::from_secs(10));
        assert_eq!(review_interval(true, true), Duration::from_secs(30));
        assert_eq!(review_interval(false, false), Duration::from_secs(120));
        assert_eq!(review_interval(false, true), Duration::from_secs(120));

        // The shell's timer follows its own window focus. Daemon health is
        // process-wide and other tests may move it, which only ever changes
        // the focused interval, so only the unfocused timer is compared with
        // itself.
        let mut app = watching_one_repository();
        app.refresh_ui.focused = false;
        let background = recipe_identities(app.review_request_subscription());
        assert_eq!(background.len(), 1);
        app.refresh_ui.focused = true;
        let focused = recipe_identities(app.review_request_subscription());
        assert_eq!(focused.len(), 1);
        assert_ne!(focused, background, "focus changes the review timer");
        app.refresh_ui.focused = false;
        assert_eq!(
            recipe_identities(app.review_request_subscription()),
            background,
            "losing focus again returns to the same background timer"
        );
    }

    #[test]
    fn a_tick_that_changes_nothing_does_not_republish_review_presence() {
        let mut app = watching_one_repository();
        drop(app.update_review_requests(Action::Tick));
        assert!(
            app.refresh_ui
                .review_requests
                .in_flight_for_test()
                .is_some()
        );
        assert!(
            app.notification_ui
                .take_pending_presence_for_test()
                .is_some(),
            "a newly watched repository can change what a focused review names"
        );
        for _ in 0..3 {
            drop(app.update_review_requests(Action::Tick));
        }
        assert!(
            app.notification_ui
                .take_pending_presence_for_test()
                .is_none()
        );
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
                !app.refresh_ui
                    .review_requests
                    .in_flight_for_test()
                    .is_some(),
                "only regaining focus starts a read"
            );
        }
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(main, true))));
        assert!(
            app.refresh_ui
                .review_requests
                .in_flight_for_test()
                .is_some()
        );
    }
}
