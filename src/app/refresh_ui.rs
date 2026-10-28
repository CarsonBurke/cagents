use super::*;

/// How long the selected project's rows may go unread while the window is
/// focused.
const SELECTED_PROJECT_INTERVAL: Duration = Duration::from_secs(60);
/// Every other project's rows draw the same pull request and check state, so
/// they are read as well, one at a time. A project with a pull request still
/// open can see its checks or review state change at any moment.
const ACTIVE_PROJECT_INTERVAL: Duration = Duration::from_secs(120);
/// A project with no pull request open changes when someone opens one, which
/// an agent finishing its turn there already brings forward by marking it
/// stale.
const IDLE_PROJECT_INTERVAL: Duration = Duration::from_secs(600);
/// No project is read more often than this, however often an agent finishes
/// a turn in it or the window regains focus.
const MIN_PROJECT_INTERVAL: Duration = Duration::from_secs(30);
/// Reads that keep failing, a GitHub rate limit included, wait twice as long
/// after each failure, up to this.
const MAX_PROJECT_BACKOFF: Duration = Duration::from_secs(30 * 60);

pub(super) struct State {
    pub(super) git: git_ui::State,
    pub(super) review_requests: review_request_ui::State,
    pub(super) focused: bool,
    pub(super) source_generation: u64,
    pub(super) source_cancel: Option<CancellationToken>,
    pub(super) projects: HashMap<ProjectId, CancellationToken>,
    schedule: ProjectSchedule,
    last_pull_request: Instant,
}

impl Default for State {
    fn default() -> Self {
        Self {
            git: git_ui::State::default(),
            review_requests: review_request_ui::State::default(),
            focused: false,
            source_generation: 0,
            source_cancel: None,
            projects: HashMap::new(),
            schedule: ProjectSchedule::default(),
            last_pull_request: Instant::now(),
        }
    }
}

/// When each project was last read, which ones something has made worth
/// reading before their interval is up, and how many reads in a row failed.
#[derive(Debug, Default)]
struct ProjectSchedule {
    read: HashMap<ProjectId, Instant>,
    stale: HashSet<ProjectId>,
    failures: HashMap<ProjectId, u32>,
}

impl ProjectSchedule {
    fn started(&mut self, project: ProjectId, now: Instant) {
        self.read.insert(project, now);
        self.stale.remove(&project);
    }

    fn finished(&mut self, project: ProjectId, succeeded: bool) {
        if succeeded {
            self.failures.remove(&project);
        } else {
            *self.failures.entry(project).or_default() += 1;
        }
    }

    fn retain(&mut self, open: impl Fn(ProjectId) -> bool) {
        self.read.retain(|&project, _| open(project));
        self.stale.retain(|&project| open(project));
        self.failures.retain(|&project, _| open(project));
    }

    fn mark_stale(&mut self, project: ProjectId) {
        self.stale.insert(project);
    }

    /// How long `project` waits after its last read. Stale and regained
    /// focus shorten the wait to the floor; neither shortens a backoff.
    fn wait(&self, project: ProjectId, interval: Duration, eager: bool) -> Duration {
        match self.failures.get(&project) {
            Some(&failures) => interval
                .saturating_mul(1 << failures.min(8))
                .min(MAX_PROJECT_BACKOFF),
            None if eager || self.stale.contains(&project) => interval.min(MIN_PROJECT_INTERVAL),
            None => interval,
        }
    }

    fn due(&self, project: ProjectId, interval: Duration, eager: bool, now: Instant) -> bool {
        self.read
            .get(&project)
            .is_none_or(|&read| now.duration_since(read) >= self.wait(project, interval, eager))
    }

    /// The one project a refresh tick reads, if any. The selected project
    /// keeps its own cadence; the rest, each with its own interval, wait
    /// until no read is in flight, so a long project list costs one `git`
    /// and `gh` round at a time rather than a burst. The one read longest ago
    /// goes first, and a stale one only breaks a tie, so a project that keeps
    /// going stale cannot hold the others back.
    fn next(
        &self,
        selected: Option<ProjectId>,
        others: impl IntoIterator<Item = (ProjectId, Duration)>,
        in_flight: &HashMap<ProjectId, CancellationToken>,
        regained_focus: bool,
        now: Instant,
    ) -> Option<ProjectId> {
        if let Some(selected) = selected.filter(|project| !in_flight.contains_key(project))
            && self.due(selected, SELECTED_PROJECT_INTERVAL, regained_focus, now)
        {
            return Some(selected);
        }
        if !in_flight.is_empty() {
            return None;
        }
        others
            .into_iter()
            .filter(|&(project, interval)| {
                Some(project) != selected && self.due(project, interval, false, now)
            })
            .map(|(project, _)| project)
            .min_by_key(|project| (self.read.get(project), !self.stale.contains(project)))
    }
}

impl State {
    pub(super) fn schedule_remote_started(&mut self, project_id: ProjectId) {
        self.schedule.started(project_id, Instant::now());
    }

    pub(super) fn started(&mut self, project: ProjectId) {
        self.schedule.started(project, Instant::now());
    }
    /// Something that usually changes a project's pull requests happened, such
    /// as an agent finishing a turn, so its rows are read sooner.
    pub(super) fn mark_stale(&mut self, project: ProjectId) {
        self.schedule.mark_stale(project);
    }

    /// A read finished. One that failed, or reached GitHub only to be
    /// refused, makes the project wait longer before the next.
    pub(super) fn finished(&mut self, project: ProjectId, succeeded: bool) {
        self.schedule.finished(project, succeeded);
    }

    #[cfg(test)]
    pub(super) fn is_stale(&self, project: ProjectId) -> bool {
        self.schedule.stale.contains(&project)
    }
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some(cancel) = &self.source_cancel {
            cancel.cancel();
        }
        for cancel in self.projects.values() {
            cancel.cancel();
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Tick,
    Focused(window::Id, bool),
}

impl Cagents {
    fn next_github_project(
        &self,
        selected: Option<ProjectId>,
        regained_focus: bool,
        now: Instant,
    ) -> Option<ProjectId> {
        self.refresh_ui.schedule.next(
            selected.filter(|id| {
                self.sidebar_model.project(*id).is_some_and(|project| {
                    project.kind == ProjectKind::Git && self.github_project_polling_ready(*id)
                })
            }),
            self.sidebar_model
                .projects
                .iter()
                .filter(|project| {
                    project.kind == ProjectKind::Git
                        && project.unreachable.is_none()
                        && self.github_project_polling_ready(project.id)
                })
                .map(|project| {
                    let interval = if project.has_open_pull_request() {
                        ACTIVE_PROJECT_INTERVAL
                    } else {
                        IDLE_PROJECT_INTERVAL
                    };
                    (project.id, interval)
                }),
            &self.refresh_ui.projects,
            regained_focus,
            now,
        )
    }

    pub(super) fn refresh_subscription(&self) -> Subscription<Message> {
        if !self.refresh_ui.focused || self.modal_is_open() || self.update_prepared {
            return Subscription::none();
        }
        cosmic::iced::time::every(Duration::from_secs(10)).map(|_| Message::RefreshUi(Action::Tick))
    }

    pub(super) fn update_refresh(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Tick => self.refresh(false),
            Action::Focused(window, focused) if self.core.main_window_id() == Some(window) => {
                self.main_window_focus_changed(focused)
            }
            Action::Focused(..) => Task::none(),
        }
    }

    /// Everything that follows the main window's focus. Regaining it catches
    /// up on whatever only polls, or polls quickly, while it is focused.
    fn main_window_focus_changed(&mut self, focused: bool) -> Task<Message> {
        let regained = focused && !self.refresh_ui.focused;
        if !focused {
            self.tab_history.finish(self.workspace.focused());
            // Losing the window is not a choice of workspace, and the
            // release that would have made one never arrives.
            self.cancel_workspace_preview();
        }
        self.refresh_ui.focused = focused;
        if !regained {
            return self.accounts_focus_changed(focused, false);
        }
        Task::batch([
            self.refresh(true),
            self.accounts_focus_changed(focused, true),
            self.installation_focus_regained(),
            self.review_requests_focus_regained(),
        ])
    }

    fn refresh(&mut self, regained_focus: bool) -> Task<Message> {
        // Device windows refresh their navigation catalog through the daemon.
        // Git, editor and filesystem refreshes below remain local-only.
        if !self.refresh_ui.focused || self.modal_is_open() || self.update_prepared {
            return Task::none();
        }
        if crate::device::is_remote() {
            self.refresh_ui.projects.retain(|id, cancellation| {
                self.repository_generations.contains_key(id) && !cancellation.is_cancelled()
            });
            let projects = &self.sidebar_model.projects;
            self.refresh_ui
                .schedule
                .retain(|id| projects.iter().any(|project| project.id == id));
            let selected = sidebar::selected_project(&self.sidebar_model, &self.sidebar_state)
                .map(|project| project.id);
            let target = self.refresh_ui.schedule.next(
                selected,
                projects
                    .iter()
                    .filter(|project| project.unreachable.is_none())
                    .map(|project| (project.id, ACTIVE_PROJECT_INTERVAL)),
                &self.refresh_ui.projects,
                regained_focus,
                Instant::now(),
            );
            return target
                .and_then(|id| self.sidebar_model.project(id))
                .map(|project| (project.id, project.repository.clone(), project.kind))
                .map_or_else(Task::none, |(id, repository, kind)| {
                    self.project_load_task(repository, Some(id), false, Some(kind))
                });
        }
        let mut tasks = Vec::new();
        tasks.push(self.git_fallback_tick());
        tasks.push(self.commit_terminal_handoffs());
        for (&card, runtime) in &mut self.cards {
            if self.workspace.visibility(card) == Some(VisibilityClass::Visible)
                && let CardRuntime::Repository(runtime) = runtime
            {
                tasks.push(repository_task(runtime.refresh_visible(card)));
            }
        }
        if self.context_inspector_open
            && matches!(self.context_tab, ContextTab::Files | ContextTab::Changes)
            && self.refresh_ui.source_cancel.is_none()
            && let Some(key) = self
                .sidebar_model
                .source_control
                .as_ref()
                .filter(|source| !source.busy())
                .map(|source| source.key.clone())
        {
            tasks.push(self.reload_source_control(&key));
        }
        let now = Instant::now();
        if let Some(card) = self.workspace.focused()
            && matches!(self.cards.get(&card), Some(CardRuntime::PullRequests(_)))
        {
            tasks.push(
                self.apply_pull_request_review(
                    card,
                    pull_request_review::Action::RefreshIfDue(now),
                ),
            );
        }
        if regained_focus
            || now.duration_since(self.refresh_ui.last_pull_request) >= Duration::from_secs(30)
        {
            self.refresh_ui.last_pull_request = now;
            tasks.push(self.maybe_refresh_pull_request());
        }
        let projects = &self.sidebar_model.projects;
        self.refresh_ui
            .schedule
            .retain(|id| projects.iter().any(|project| project.id == id));
        self.refresh_ui.projects.retain(|id, cancellation| {
            self.repository_generations.contains_key(id) && !cancellation.is_cancelled()
        });
        // A picker read has a provisional ID until discovery identifies its
        // repository. Prioritize that explicit intent: a focus-regain poll of
        // the existing row must not supersede its listing under another ID.
        // Only live reads block polling; failed, cancelled, and retired opens
        // cannot leave automatic refresh waiting for an acknowledgement.
        if self
            .refresh_ui
            .projects
            .keys()
            .any(|id| self.project_ui.reopening.contains_key(id))
        {
            return Task::batch(tasks);
        }
        // A project whose checkout could not be read is retried from its row
        // rather than in the background, unless it is selected.
        if let Some(id) = self.next_github_project(
            sidebar::selected_project(&self.sidebar_model, &self.sidebar_state)
                .map(|project| project.id),
            regained_focus,
            now,
        ) {
            tasks.push(self.github_read_task(id));
        }
        Task::batch(tasks)
    }

    pub(super) fn cancel_source_read(&mut self) {
        if let Some(cancel) = self.refresh_ui.source_cancel.take() {
            cancel.cancel();
        }
        self.refresh_ui.source_generation = self.refresh_ui.source_generation.wrapping_add(1);
    }

    pub(super) fn repository_read_task(
        &mut self,
        path: PathBuf,
        project_id: ProjectId,
        generation: u64,
        open_cards: bool,
        kind: Option<ProjectKind>,
    ) -> Task<Message> {
        if crate::device::is_remote() {
            return self.device_project_read_task(path, project_id, generation, kind);
        }
        self.refresh_ui.schedule.started(project_id, Instant::now());
        self.refresh_ui.git.forget(project_id);
        let cancellation = CancellationToken::default();
        if let Some(previous) = self
            .refresh_ui
            .projects
            .insert(project_id, cancellation.clone())
        {
            previous.cancel();
        }
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        let kind = kind
            .or_else(|| {
                self.sidebar_model
                    .projects
                    .iter()
                    .find(|project| project.id == project_id || project.repository == canonical)
                    .map(|project| project.kind)
            })
            .or_else(|| {
                self.workspace_model
                    .project(&canonical)
                    .map(|project| project.kind)
            });
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                load_project(&path, Some(project_id), cancellation, kind)
            })
            .await
            .map_err(|error| format!("repository loader join failed: {error}"))
            .and_then(|result| result)
            .map(Box::new);
            Message::RepositoryLoaded {
                open_cards,
                project_id,
                generation,
                result,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projects<const N: usize>() -> [ProjectId; N] {
        std::array::from_fn(|_| ProjectId::new())
    }

    fn active<const N: usize>(projects: [ProjectId; N]) -> [(ProjectId, Duration); N] {
        projects.map(|project| (project, ACTIVE_PROJECT_INTERVAL))
    }

    #[test]
    fn the_selected_project_keeps_its_own_cadence() {
        let [selected, other] = projects();
        let start = Instant::now();
        let mut schedule = ProjectSchedule::default();
        schedule.started(selected, start);
        schedule.started(other, start);
        let in_flight = HashMap::new();
        let next = |schedule: &ProjectSchedule, regained_focus, elapsed| {
            schedule.next(
                Some(selected),
                active([selected, other]),
                &in_flight,
                regained_focus,
                start + elapsed,
            )
        };
        assert_eq!(next(&schedule, false, MIN_PROJECT_INTERVAL), None);
        // Regaining focus reads it early, but never twice within the floor.
        assert_eq!(next(&schedule, true, Duration::from_secs(10)), None);
        assert_eq!(next(&schedule, true, MIN_PROJECT_INTERVAL), Some(selected));
        assert_eq!(
            next(&schedule, false, SELECTED_PROJECT_INTERVAL),
            Some(selected)
        );
        schedule.started(selected, start + SELECTED_PROJECT_INTERVAL);
        assert_eq!(next(&schedule, false, SELECTED_PROJECT_INTERVAL), None);
        // Every other project is read too, once its longer interval is up.
        schedule.started(selected, start + ACTIVE_PROJECT_INTERVAL);
        assert_eq!(next(&schedule, false, ACTIVE_PROJECT_INTERVAL), Some(other));
    }

    #[test]
    fn other_projects_are_read_one_at_a_time_oldest_first() {
        let [selected, older, newer, unread] = projects();
        let start = Instant::now();
        let mut schedule = ProjectSchedule::default();
        schedule.started(selected, start + ACTIVE_PROJECT_INTERVAL);
        schedule.started(older, start);
        schedule.started(newer, start + Duration::from_secs(10));
        let now = start + ACTIVE_PROJECT_INTERVAL + Duration::from_secs(10);
        let others = active([selected, newer, older]);
        assert_eq!(
            schedule.next(Some(selected), others, &HashMap::new(), false, now),
            Some(older)
        );
        // A project never read goes before any that was.
        assert_eq!(
            schedule.next(
                Some(selected),
                active([selected, newer, older, unread]),
                &HashMap::new(),
                false,
                now
            ),
            Some(unread)
        );
        // Nothing else starts while a read is in flight.
        let in_flight = HashMap::from([(older, CancellationToken::default())]);
        assert_eq!(
            schedule.next(Some(selected), others, &in_flight, false, now),
            None
        );
    }

    #[test]
    fn a_project_without_an_open_pull_request_is_read_least_often() {
        let [idle, open] = projects();
        let start = Instant::now();
        let mut schedule = ProjectSchedule::default();
        schedule.started(idle, start);
        schedule.started(open, start);
        let others = [
            (idle, IDLE_PROJECT_INTERVAL),
            (open, ACTIVE_PROJECT_INTERVAL),
        ];
        let next = |schedule: &ProjectSchedule, elapsed| {
            schedule.next(None, others, &HashMap::new(), false, start + elapsed)
        };
        assert_eq!(next(&schedule, ACTIVE_PROJECT_INTERVAL), Some(open));
        schedule.started(open, start + ACTIVE_PROJECT_INTERVAL);
        assert_eq!(next(&schedule, ACTIVE_PROJECT_INTERVAL), None);
        assert_eq!(next(&schedule, IDLE_PROJECT_INTERVAL), Some(idle));
    }

    #[test]
    fn a_stale_project_is_read_early_but_not_ahead_of_older_reads() {
        let [selected, other, stale] = projects();
        let start = Instant::now();
        let mut schedule = ProjectSchedule::default();
        for project in [selected, other, stale] {
            schedule.started(project, start);
        }
        schedule.mark_stale(stale);
        let next = |schedule: &ProjectSchedule, others: &[ProjectId], in_flight, now| {
            schedule.next(
                Some(selected),
                others
                    .iter()
                    .map(|&project| (project, ACTIVE_PROJECT_INTERVAL)),
                in_flight,
                false,
                now,
            )
        };
        let idle = HashMap::new();
        // Stale shortens the wait to the floor rather than skipping it.
        assert_eq!(
            next(
                &schedule,
                &[selected, other, stale],
                &idle,
                start + Duration::from_secs(10)
            ),
            None
        );
        let now = start + MIN_PROJECT_INTERVAL;
        assert_eq!(
            next(&schedule, &[selected, other, stale], &idle, now),
            Some(stale)
        );
        schedule.started(stale, now);
        assert_eq!(next(&schedule, &[selected, other, stale], &idle, now), None);
        // Going stale again cannot put it ahead of a project read longer ago.
        schedule.mark_stale(stale);
        let later = start + ACTIVE_PROJECT_INTERVAL;
        schedule.started(selected, later);
        assert_eq!(
            next(&schedule, &[selected, other, stale], &idle, later),
            Some(other)
        );
        // Among reads started together, the stale one goes first.
        schedule.started(other, now);
        assert_eq!(
            next(&schedule, &[selected, other, stale], &idle, later),
            Some(stale)
        );
        // The selected project is read first when it is the stale one.
        schedule.mark_stale(selected);
        let in_flight = HashMap::from([(other, CancellationToken::default())]);
        assert_eq!(
            next(
                &schedule,
                &[selected],
                &in_flight,
                later + MIN_PROJECT_INTERVAL
            ),
            Some(selected)
        );
    }

    #[test]
    fn failed_reads_back_off_until_one_succeeds() {
        let [project] = projects();
        let start = Instant::now();
        let mut schedule = ProjectSchedule::default();
        let next = |schedule: &ProjectSchedule, regained_focus, elapsed| {
            schedule.next(
                Some(project),
                [],
                &HashMap::new(),
                regained_focus,
                start + elapsed,
            )
        };
        schedule.started(project, start);
        schedule.finished(project, false);
        schedule.mark_stale(project);
        // Neither stale nor focus shortens a backoff.
        assert_eq!(next(&schedule, true, SELECTED_PROJECT_INTERVAL), None);
        assert_eq!(
            next(&schedule, false, SELECTED_PROJECT_INTERVAL * 2),
            Some(project)
        );
        schedule.finished(project, false);
        assert_eq!(next(&schedule, false, SELECTED_PROJECT_INTERVAL * 2), None);
        assert_eq!(
            next(&schedule, false, SELECTED_PROJECT_INTERVAL * 4),
            Some(project)
        );
        for _ in 0..40 {
            schedule.finished(project, false);
        }
        assert_eq!(next(&schedule, false, MAX_PROJECT_BACKOFF), Some(project));
        schedule.finished(project, true);
        assert_eq!(
            next(&schedule, false, SELECTED_PROJECT_INTERVAL),
            Some(project)
        );
    }

    #[test]
    fn remote_windows_record_focus_without_scheduling_local_repository_refresh() {
        const CHILD: &str = "CAGENTS_TEST_REMOTE_WINDOW_FOCUS";
        const TEST: &str = "app::refresh_ui::tests::remote_windows_record_focus_without_scheduling_local_repository_refresh";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "1")
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        crate::device::save(crate::device::Device {
            name: "focus-test".into(),
            host: "build-host".into(),
            executable: "cagents".into(),
        })
        .unwrap();
        crate::device::select(Some("focus-test".into())).unwrap();
        let mut app = super::super::tests::context_inspector_test_app(false);
        let window = window::Id::RESERVED;
        app.core.set_main_window_id(Some(window));
        let source_generation = app.refresh_ui.source_generation;
        let projects = app.refresh_ui.projects.len();
        drop(app.update(Message::RefreshUi(Action::Focused(window, true))));
        assert!(app.refresh_ui.focused);
        assert_eq!(app.refresh_ui.source_generation, source_generation);
        assert_eq!(app.refresh_ui.projects.len(), projects);
        drop(app.update(Message::RefreshUi(Action::Tick)));
        assert_eq!(app.refresh_ui.source_generation, source_generation);
        assert_eq!(app.refresh_ui.projects.len(), projects);
        drop(app.update(Message::RefreshUi(Action::Focused(window, false))));
        assert!(!app.refresh_ui.focused);
    }

    #[test]
    fn closed_projects_leave_the_schedule() {
        let [open, closed] = projects();
        let mut schedule = ProjectSchedule::default();
        schedule.started(open, Instant::now());
        schedule.started(closed, Instant::now());
        schedule.mark_stale(closed);
        schedule.finished(closed, false);
        schedule.retain(|project| project == open);
        assert_eq!(schedule.read.keys().collect::<Vec<_>>(), [&open]);
        assert!(schedule.stale.is_empty());
        assert!(schedule.failures.is_empty());
    }

    #[test]
    fn review_project_rate_limits_remain_failures_in_both_completion_routes() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        let availability = || GitHubAvailability::RateLimited {
            limit: crate::github::polling::RateLimit {
                retry_at: crate::github::polling::now() + 120,
            },
        };
        app.repository_generations.insert(project, 1);
        drop(app.update(Message::GitUi(git_ui::Action::Remote {
            project,
            generation: 1,
            result: Ok((availability(), Vec::new())),
        })));
        assert_eq!(app.refresh_ui.schedule.failures.get(&project), Some(&1));
        drop(app.update(Message::RepositoryLoaded {
            open_cards: false,
            project_id: project,
            generation: 1,
            result: Ok(Box::new(LoadedRepository {
                project: ProjectState {
                    id: project,
                    generation: 0,
                    name: "paused".into(),
                    repository: "/review-paused-project".into(),
                    existence: Existence::Open,
                    kind: ProjectKind::Git,
                },
                repository: None,
                worktrees: Vec::new(),
                github: availability(),
                review_repository: None,
                head_repositories: HashMap::new(),
            })),
        }));
        assert_eq!(app.refresh_ui.schedule.failures.get(&project), Some(&2));
    }

    #[test]
    fn review_project_polling_skips_shared_backoff_before_repository_discovery() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        let key = GitHubRepoKey::new("review-project-backoff.invalid", None, "o", "r").unwrap();
        app.review_repositories.insert(project, key.clone());
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "paused".into(),
            "/review-paused-project".into(),
            Vec::new(),
            Vec::new(),
        ));
        crate::github::polling::failed(
            &key.host,
            &crate::github::GitHubError::Inspection("offline".into()),
        );
        let sequence = app.repository_load_sequence;
        drop(app.github_read_task(project));
        assert_eq!(app.repository_load_sequence, sequence);
        assert!(app.repository_generations.is_empty());
        assert!(app.refresh_ui.projects.is_empty());
    }

    #[test]
    fn review_project_scheduler_skips_paused_selected_and_oldest_hosts_without_starving_others() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let [paused, ready] = projects();
        for (project, host) in [
            (paused, "review-scheduler-paused.invalid"),
            (ready, "review-scheduler-ready.invalid"),
        ] {
            app.review_repositories
                .insert(project, GitHubRepoKey::new(host, None, "o", "r").unwrap());
            app.sidebar_model.projects.push(SidebarProject::new(
                project,
                RepositoryId::new(),
                host.into(),
                PathBuf::from("/review-scheduler").join(host),
                Vec::new(),
                Vec::new(),
            ));
        }
        crate::github::polling::failed(
            "review-scheduler-paused.invalid",
            &crate::github::GitHubError::RateLimited {
                limit: crate::github::polling::RateLimit {
                    retry_at: crate::github::polling::now() + 120,
                },
            },
        );
        let now = Instant::now();
        assert_eq!(
            app.next_github_project(Some(paused), false, now),
            Some(ready)
        );
        assert_eq!(app.next_github_project(None, false, now), Some(ready));
        app.refresh_ui.schedule.started(ready, now);
        assert_eq!(app.next_github_project(Some(paused), false, now), None);
    }
}
