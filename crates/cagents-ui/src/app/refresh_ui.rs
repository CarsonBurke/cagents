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
/// State reconciliation that has no completion event gets a bounded retry,
/// but never keeps a permanent application clock alive.
const RECONCILIATION_RETRY: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct State {
    pub(super) git: git_ui::State,
    pub(super) review_requests: review_request_ui::State,
    pub(super) focused: bool,
    pub(super) source_generation: u64,
    pub(super) source_cancel: Option<CancellationToken>,
    pub(super) projects: HashMap<ProjectId, CancellationToken>,
    /// The next retry for consumers which have no completion-driven wake.
    /// This is state, rather than a fresh `now + interval` in
    /// `subscription()`, so unrelated messages preserve the timer identity.
    reconciliation_due: Option<Instant>,
    schedule: ProjectSchedule,
}

/// When each project was last read, which ones something has made worth
/// reading before their interval is up, and how many reads in a row failed.
#[derive(Debug, Default)]
struct ProjectSchedule {
    /// A project first seen in an update is immediately due, but that due
    /// instant must remain stable while iced owns its one-shot subscription.
    unread: HashMap<ProjectId, Instant>,
    read: HashMap<ProjectId, Instant>,
    /// A wall-clock GitHub admission pause converted exactly once, in the
    /// update that observed it. Rebuilding subscriptions must not move this
    /// monotonic deadline under unrelated traffic.
    host_ready: HashMap<ProjectId, (u64, Instant)>,
    stale: HashSet<ProjectId>,
    failures: HashMap<ProjectId, u32>,
}

impl ProjectSchedule {
    fn started(&mut self, project: ProjectId, now: Instant) {
        self.unread.remove(&project);
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
        self.unread.retain(|&project, _| open(project));
        self.read.retain(|&project, _| open(project));
        self.host_ready.retain(|&project, _| open(project));
        self.stale.retain(|&project| open(project));
        self.failures.retain(|&project, _| open(project));
    }

    fn mark_stale(&mut self, project: ProjectId) {
        self.stale.insert(project);
    }

    /// Initialize a project's first wake in the update which introduced it,
    /// never while `subscription()` is merely being recomputed. A direct
    /// scheduler query may still report `now` for an unknown project, but
    /// actual subscriptions always own a stored instant.
    fn observe(&mut self, projects: impl IntoIterator<Item = ProjectId>, now: Instant) {
        for project in projects {
            if !self.read.contains_key(&project) {
                self.unread.entry(project).or_insert(now);
            }
        }
    }

    /// Translate host pacing from the wall clock only when it changes. The
    /// resulting `Instant` remains the subscription identity until an event
    /// changes the budget or the deadline is reached. One reached while the
    /// wall clock still has the host paused, because the two clocks drifted
    /// apart, is translated again: a finished one-shot keeps its identity,
    /// so an unchanged past deadline would never wake the window again.
    fn observe_host_ready(
        &mut self,
        ready: impl IntoIterator<Item = (ProjectId, Option<u64>)>,
        wall_now: u64,
        now: Instant,
    ) {
        let mut observed = HashSet::new();
        for (project, ready_at) in ready {
            observed.insert(project);
            let Some(ready_at) = ready_at.filter(|&ready_at| ready_at > wall_now) else {
                self.host_ready.remove(&project);
                continue;
            };
            if self
                .host_ready
                .get(&project)
                .is_none_or(|&(known, deadline)| known != ready_at || deadline <= now)
            {
                self.host_ready.insert(
                    project,
                    (ready_at, now + Duration::from_secs(ready_at - wall_now)),
                );
            }
        }
        self.host_ready
            .retain(|project, _| observed.contains(project));
    }

    fn host_ready_at(&self, project: ProjectId) -> Option<Instant> {
        self.host_ready.get(&project).map(|&(_, deadline)| deadline)
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
        now >= self.due_at(project, interval, eager, now)
    }

    fn due_at(&self, project: ProjectId, interval: Duration, eager: bool, now: Instant) -> Instant {
        self.read.get(&project).map_or_else(
            || self.unread.get(&project).copied().unwrap_or(now),
            |&read| read + self.wait(project, interval, eager),
        )
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
    /// Request a one-shot reconciliation wake. Hooks which run outside the
    /// application's `update` path use this for work which otherwise has no
    /// event to deliver it.
    pub(super) fn arm_reconciliation(&mut self) {
        self.reconciliation_due.get_or_insert_with(Instant::now);
    }

    fn reconcile_deadline(&mut self, needed: bool) {
        if needed {
            self.reconciliation_due
                .get_or_insert_with(|| Instant::now() + RECONCILIATION_RETRY);
        } else {
            self.reconciliation_due = None;
        }
    }

    fn reconciliation_tick_fired(&mut self) {
        self.reconciliation_due = None;
    }

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

/// The exact refresh deadline is the subscription identity, so replacing it
/// cancels the older sleep instead of leaving a periodic wake behind.
#[derive(Hash)]
struct RefreshDeadline(Instant);

fn refresh_deadline_stream(
    deadline: &RefreshDeadline,
) -> impl cosmic::iced::futures::Stream<Item = Message> + use<> {
    let deadline = tokio::time::Instant::from_std(deadline.0);
    cosmic::iced::futures::stream::once(async move {
        tokio::time::sleep_until(deadline).await;
        Message::RefreshUi(Action::Tick)
    })
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
                    // The selected row is also the recovery path for an
                    // unreadable checkout.  It must remain eligible after a
                    // mount or device reconnects.
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
        self.refresh_deadline(Instant::now())
            .map_or_else(Subscription::none, |deadline| {
                Subscription::run_with(RefreshDeadline(deadline), refresh_deadline_stream)
            })
    }

    /// The one next refresh wake.  This is deliberately a deadline instead of
    /// an interval: a quiet workspace has no subscription at all, while a
    /// GitHub backoff or failed watch wakes exactly when it can do work again.
    fn refresh_deadline(&self, now: Instant) -> Option<Instant> {
        let mut deadline = None;
        let mut include = |candidate: Option<Instant>| {
            if let Some(candidate) = candidate {
                deadline =
                    Some(deadline.map_or(candidate, |current: Instant| current.min(candidate)));
            }
        };

        include(self.refresh_ui.reconciliation_due);

        if crate::device::is_remote() {
            include(self.project_refresh_deadline(now, true));
            return deadline;
        }

        include(self.refresh_ui.git.fallback_refresh_deadline(now));

        include(self.project_refresh_deadline(now, false));
        deadline
    }

    /// Consumers without their own event stream retry at a stored deadline.
    /// Their cadence is visible-work-only; chats and terminals do not enter
    /// this set once their handoff has settled.
    fn reconciliation_refresh_needed(&self) -> bool {
        self.document_tone_stale
            || !self.pending_terminal_handoffs.is_empty()
            || !self.shared_views.blocked.is_empty()
            || self.cards.iter().any(|(&card, runtime)| {
                self.workspace.visibility(card) == Some(VisibilityClass::Visible)
                    && matches!(
                        runtime,
                        CardRuntime::Repository(_) | CardRuntime::PullRequests(_)
                    )
            })
            || (self.workspace_view_shown().is_some()
                && self.refresh_ui.source_cancel.is_none()
                && self
                    .sidebar_model
                    .source_control
                    .as_ref()
                    .is_some_and(|source| !source.busy()))
    }

    pub(super) fn sync_refresh_reconciliation_deadline(&mut self) {
        self.sync_git_shown();
        let now = Instant::now();
        self.refresh_ui.schedule.observe(
            self.sidebar_model.projects.iter().map(|project| project.id),
            now,
        );
        // `polling::ready_at` is wall-clock state. Capture its translation
        // while handling an update, never while rebuilding a subscription;
        // otherwise a GitHub backoff continually changes the one-shot id.
        let wall_now = crate::github::polling::now();
        self.refresh_ui.schedule.observe_host_ready(
            self.review_repositories
                .iter()
                .map(|(&project, repository)| {
                    (
                        project,
                        crate::github::polling::ready_at(&repository.authority()),
                    )
                }),
            wall_now,
            now,
        );
        let needed = self.reconciliation_refresh_needed();
        self.refresh_ui.reconcile_deadline(needed);
    }

    /// Compute the next selected/other project poll without turning a
    /// not-yet-due project into a fixed app tick. Local windows poll GitHub;
    /// device windows use the same scheduler to reload their daemon catalog.
    fn project_refresh_deadline(&self, now: Instant, remote: bool) -> Option<Instant> {
        if self.refresh_ui.projects.iter().any(|(id, cancellation)| {
            self.repository_generations.contains_key(id) && !cancellation.is_cancelled()
        }) {
            // A result message re-arms the subscription. Starting another
            // read while one is in flight was never useful.
            return None;
        }
        let selected = sidebar::selected_project(&self.sidebar_model, &self.sidebar_state)
            .filter(|project| remote || project.kind == ProjectKind::Git)
            .map(|project| project.id);
        let project_deadline = |project: &SidebarProject, interval| {
            let due = self
                .refresh_ui
                .schedule
                .due_at(project.id, interval, false, now);
            if remote {
                return due;
            }
            self.refresh_ui
                .schedule
                .host_ready_at(project.id)
                .map(|ready_at| due.max(ready_at))
                .unwrap_or(due)
        };
        let mut earliest = selected.and_then(|id| {
            self.sidebar_model
                .project(id)
                .map(|project| project_deadline(project, SELECTED_PROJECT_INTERVAL))
        });
        for project in self.sidebar_model.projects.iter().filter(|project| {
            Some(project.id) != selected
                && project.unreachable.is_none()
                && (remote || project.kind == ProjectKind::Git)
        }) {
            let interval = if remote || project.has_open_pull_request() {
                ACTIVE_PROJECT_INTERVAL
            } else {
                IDLE_PROJECT_INTERVAL
            };
            let due = project_deadline(project, interval);
            earliest = Some(earliest.map_or(due, |current| current.min(due)));
        }
        earliest
    }

    pub(super) fn update_refresh(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Tick => {
                self.refresh_ui.reconciliation_tick_fired();
                self.refresh(false)
            }
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
        let mut dictation = Task::none();
        if !focused {
            self.tab_history.finish(self.workspace.focused());
            // Losing the window is not a choice of workspace, and the
            // release that would have made one never arrives.
            self.cancel_workspace_preview();
            // Nor does the release that would end push-to-talk.
            dictation = self.dictation_unfocused();
        }
        self.refresh_ui.focused = focused;
        crate::ui_controls::animate_activity(focused);
        if !regained {
            return Task::batch([dictation, self.accounts_focus_changed(focused, false)]);
        }
        self.pane_follow_focus_regained();
        Task::batch([
            // A request that arrived while away is offered on return.
            self.read_owed_automation_requests(),
            // Coming back to the window is coming back to the pane in use.
            self.focus_pane_keyboard_on_activation(),
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
        // Focus return may race a deadline which became due while unfocused.
        // This refresh performs the same reconciliation, so consume it rather
        // than immediately doing the work a second time on a stale Tick.
        self.refresh_ui.reconciliation_tick_fired();
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
        if self.workspace_view_shown().is_some()
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
        let browsers = self
            .cards
            .iter()
            .filter_map(|(&card, runtime)| {
                (self.workspace.visibility(card) == Some(VisibilityClass::Visible)
                    && matches!(runtime, CardRuntime::PullRequests(_)))
                .then_some(card)
            })
            .collect::<Vec<_>>();
        for card in browsers {
            tasks.push(self.apply_pull_request_review(
                card,
                if regained_focus {
                    pull_request_review::Action::BrowserFocused
                } else {
                    pull_request_review::Action::RefreshIfDue(now)
                },
            ));
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

    /// Spinners take their cadence from the main window's focus and nothing
    /// else: another window's focus changes must neither slow nor restore it.
    #[test]
    fn activity_spinners_run_at_full_cadence_only_while_the_main_window_is_focused() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let main = window::Id::RESERVED;
        app.core.set_main_window_id(Some(main));
        let other = window::Id::unique();
        assert!(!crate::ui_controls::activity_animates());

        drop(app.update(Message::RefreshUi(Action::Focused(main, true))));
        assert!(crate::ui_controls::activity_animates());
        drop(app.update(Message::RefreshUi(Action::Focused(other, false))));
        assert!(crate::ui_controls::activity_animates());

        drop(app.update(Message::RefreshUi(Action::Focused(main, false))));
        assert!(!crate::ui_controls::activity_animates());
        drop(app.update(Message::RefreshUi(Action::Focused(other, true))));
        assert!(!crate::ui_controls::activity_animates());
    }

    #[test]
    fn settled_chat_and_terminal_workspaces_do_not_arm_a_refresh_timer() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.refresh_ui.focused = true;

        assert_eq!(
            app.refresh_subscription().units(),
            0,
            "daemon wakes keep settled chats and terminals current without polling"
        );
    }

    #[test]
    fn pending_refresh_consumers_keep_the_timer_armed() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.refresh_ui.focused = true;

        app.shared_views
            .blocked
            .insert(crate::workspace_model::views::Scope::Channel(
                crate::model::ChannelId::new(),
            ));
        app.sync_refresh_reconciliation_deadline();
        assert_ne!(app.refresh_subscription().units(), 0);
        app.shared_views.blocked.clear();

        let terminal = match super::super::tests::terminal_card_state().kind {
            CardKind::Terminal(terminal) => terminal,
            _ => unreachable!("test terminal card has a terminal session"),
        };
        app.pending_terminal_handoffs.insert(terminal.id, terminal);
        app.sync_refresh_reconciliation_deadline();
        assert_ne!(app.refresh_subscription().units(), 0);
    }

    #[test]
    fn stale_document_tone_arms_a_one_shot_refresh_deadline() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.refresh_ui.focused = true;
        app.document_tone_stale = true;
        app.refresh_ui.arm_reconciliation();

        assert_ne!(app.refresh_subscription().units(), 0);
    }

    #[test]
    fn reconciliation_deadline_keeps_its_subscription_identity_during_unrelated_updates() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.refresh_ui.focused = true;
        let card = app.workspace.focused().expect("fixture has a focused card");
        app.workspace.resize_viewport(1_600.0);
        app.cards.insert(
            card,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
        );
        app.sync_refresh_reconciliation_deadline();
        let before = recipe_identities(app.refresh_subscription());
        assert_eq!(before.len(), 1);

        // A scale-factor event causes a normal app update without changing
        // the visible repository consumer or its stored refresh deadline.
        drop(app.update(Message::WindowScaleFactorChanged(
            cosmic::iced::window::Id::unique(),
            app.window_scale_factor,
        )));
        assert_eq!(recipe_identities(app.refresh_subscription()), before);
    }

    #[test]
    fn visible_repository_keeps_the_timer_armed() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.refresh_ui.focused = true;
        let card = app.workspace.focused().expect("fixture has a visible card");
        app.workspace.resize_viewport(1_600.0);
        app.cards.insert(
            card,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
        );
        app.sync_refresh_reconciliation_deadline();

        assert_ne!(app.refresh_subscription().units(), 0);
    }

    #[test]
    fn visible_pull_request_pane_gets_its_own_refresh_deadline() {
        let (mut app, project) = super::super::tests::pull_request_test_app(&[]);
        app.active_workspace = Some("/repo".into());
        let (browser, _) = app.add_pull_requests_card("/repo".into());
        drop(app.new_native_chat(crate::agent::Provider::Codex, Some("/repo".into())));
        app.workspace.resize_viewport(2_400.0);
        app.workspace.native_scroll(0.0, 2_400.0);
        assert_eq!(
            app.workspace.visibility(browser),
            Some(VisibilityClass::Visible)
        );
        app.refresh_ui.focused = true;
        let now = Instant::now();
        app.refresh_ui.schedule.started(project, now);
        app.sync_refresh_reconciliation_deadline();

        assert!(
            app.refresh_deadline(Instant::now())
                .is_some_and(|deadline| deadline <= Instant::now() + RECONCILIATION_RETRY)
        );
    }

    #[test]
    fn shown_source_control_gets_its_own_refresh_deadline() {
        use super::super::context_sections::{Action as ContextAction, WorkspaceView};

        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().expect("fixture has a focused card");
        app.sidebar_model = sidebar::test_support::panel_model();
        app.sidebar_state = sidebar::test_support::panel_state(&app.sidebar_model);
        app.open_pane_inspector_now(card);
        drop(app.handle_context_section(ContextAction::Show(WorkspaceView::SourceControl)));
        assert!(app.workspace_view_shown().is_some());
        assert!(
            app.sidebar_model
                .source_control
                .as_ref()
                .is_some_and(|source| !source.busy())
        );
        app.refresh_ui.focused = true;
        let now = Instant::now();
        for project in &app.sidebar_model.projects {
            app.refresh_ui.schedule.started(project.id, now);
        }
        app.sync_refresh_reconciliation_deadline();

        assert!(
            app.refresh_deadline(Instant::now())
                .is_some_and(|deadline| deadline <= Instant::now() + RECONCILIATION_RETRY)
        );
    }

    #[test]
    fn failed_github_project_keeps_its_retry_subscription_during_backoff() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.refresh_ui.focused = true;
        let project = ProjectId::new();
        let repository = PathBuf::from("/repository");
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repository".into(),
            repository.clone(),
            Vec::new(),
            Vec::new(),
        ));
        app.sidebar_state
            .apply(&sidebar::SidebarAction::OpenRepository {
                project_id: project,
                repository,
            });
        let started = Instant::now();
        app.refresh_ui.schedule.started(project, started);
        app.refresh_ui.schedule.finished(project, false);

        // The scheduling clock correctly defers the failed read, while the
        // subscription remains armed to reach the later retry.
        assert_eq!(app.next_github_project(None, false, Instant::now()), None);
        assert_ne!(app.refresh_subscription().units(), 0);
        let now = Instant::now();
        assert_eq!(
            app.refresh_deadline(now),
            Some(started + SELECTED_PROJECT_INTERVAL * 2),
            "the schedule backoff itself supplies the next actionable wake"
        );
    }

    #[test]
    fn selected_unreadable_project_keeps_its_recovery_deadline() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.refresh_ui.focused = true;
        let project = ProjectId::new();
        let repository = PathBuf::from("/unavailable-mount");
        let mut row = SidebarProject::new(
            project,
            RepositoryId::new(),
            "Unavailable".into(),
            repository.clone(),
            Vec::new(),
            Vec::new(),
        );
        row.unreachable = Some("mount unavailable".into());
        app.sidebar_model.projects.push(row);
        app.sidebar_state
            .apply(&sidebar::SidebarAction::OpenRepository {
                project_id: project,
                repository,
            });

        let now = Instant::now();
        assert_eq!(
            app.next_github_project(Some(project), false, now),
            Some(project)
        );
        assert_eq!(app.project_refresh_deadline(now, false), Some(now));
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
        crate::device::select(Some("focus-test")).unwrap();
        let mut app = super::super::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        let mut row = SidebarProject::new(
            project,
            RepositoryId::new(),
            "remote".into(),
            PathBuf::from("/remote"),
            Vec::new(),
            Vec::new(),
        );
        row.unreachable = Some("device disconnected".into());
        app.sidebar_model.projects.push(row);
        app.sidebar_state
            .apply(&sidebar::SidebarAction::OpenRepository {
                project_id: project,
                repository: PathBuf::from("/remote"),
            });
        let window = window::Id::RESERVED;
        app.core.set_main_window_id(Some(window));
        let source_generation = app.refresh_ui.source_generation;
        app.refresh_ui.focused = true;
        let now = Instant::now();
        assert_ne!(
            app.refresh_subscription().units(),
            0,
            "the selected disconnected device project arms its own scheduled wake"
        );
        assert_eq!(app.project_refresh_deadline(now, true), Some(now));
        app.refresh_ui.focused = false;
        drop(app.update(Message::RefreshUi(Action::Focused(window, true))));
        assert!(app.refresh_ui.focused);
        assert_eq!(app.refresh_ui.source_generation, source_generation);
        assert!(app.refresh_ui.projects.contains_key(&project));
        drop(app.update(Message::RefreshUi(Action::Tick)));
        assert_eq!(app.refresh_ui.source_generation, source_generation);
        assert!(app.refresh_ui.projects.contains_key(&project));
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
            limit: crate::github::polling::RateLimit::secondary(
                crate::github::polling::now() + 120,
            ),
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
            &key.authority(),
            &crate::github::GitHubError::Inspection("offline".into()),
        );
        let sequence = app.repository_load_sequence;
        drop(app.github_read_task(project));
        assert_eq!(app.repository_load_sequence, sequence);
        assert!(app.repository_generations.is_empty());
        assert!(app.refresh_ui.projects.is_empty());
    }

    #[test]
    fn host_backoff_moves_a_past_project_deadline_to_the_ready_instant() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.refresh_ui.focused = true;
        let project = ProjectId::new();
        let key = GitHubRepoKey::new("refresh-deadline-backoff.invalid", None, "o", "r").unwrap();
        app.review_repositories.insert(project, key.clone());
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "paused".into(),
            PathBuf::from("/refresh-deadline-backoff"),
            Vec::new(),
            Vec::new(),
        ));
        app.sidebar_state
            .apply(&sidebar::SidebarAction::OpenRepository {
                project_id: project,
                repository: PathBuf::from("/refresh-deadline-backoff"),
            });
        let now = Instant::now();
        app.refresh_ui
            .schedule
            .started(project, now.checked_sub(SELECTED_PROJECT_INTERVAL).unwrap());
        crate::github::polling::failed(
            &key.authority(),
            &crate::github::GitHubError::Inspection("offline".into()),
        );

        app.sync_refresh_reconciliation_deadline();
        let expected = app.refresh_ui.schedule.host_ready_at(project).unwrap();
        assert_eq!(
            app.project_refresh_deadline(now, false),
            Some(expected),
            "a refused due read must wait for the host budget rather than keep a past timer id"
        );

        // Rebuilding subscriptions repeatedly runs this synchronizer. It must
        // retain the original monotonic conversion rather than translating
        // the same wall-clock deadline from a fresh `now` each time.
        let before = app.refresh_ui.schedule.host_ready_at(project);
        app.sync_refresh_reconciliation_deadline();
        assert_eq!(app.refresh_ui.schedule.host_ready_at(project), before);
        assert_eq!(
            app.project_refresh_deadline(Instant::now(), false),
            Some(expected)
        );
    }

    #[test]
    fn a_host_pause_outlasting_its_deadline_is_translated_again() {
        let mut schedule = ProjectSchedule::default();
        let project = ProjectId::new();
        let start = Instant::now();
        schedule.observe_host_ready([(project, Some(1_030))], 1_000, start);
        let first = schedule.host_ready_at(project).unwrap();
        assert_eq!(first, start + Duration::from_secs(30));
        schedule.observe_host_ready(
            [(project, Some(1_030))],
            1_010,
            start + Duration::from_secs(10),
        );
        assert_eq!(
            schedule.host_ready_at(project),
            Some(first),
            "unchanged while pending"
        );
        // The monotonic deadline passed, but the wall clock lags a second.
        let reached = first + Duration::from_millis(5);
        schedule.observe_host_ready([(project, Some(1_030))], 1_029, reached);
        assert_eq!(
            schedule.host_ready_at(project),
            Some(reached + Duration::from_secs(1)),
            "the same past deadline would never fire again"
        );
    }

    #[test]
    fn stale_in_flight_project_does_not_block_the_next_deadline() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "project".into(),
            PathBuf::from("/project"),
            Vec::new(),
            Vec::new(),
        ));
        app.sidebar_state
            .apply(&sidebar::SidebarAction::OpenRepository {
                project_id: project,
                repository: PathBuf::from("/project"),
            });
        let now = Instant::now();
        app.refresh_ui.schedule.started(project, now);
        app.refresh_ui
            .projects
            .insert(ProjectId::new(), CancellationToken::new());

        assert_eq!(
            app.project_refresh_deadline(now, false),
            Some(now + SELECTED_PROJECT_INTERVAL)
        );
    }

    #[test]
    fn stale_repository_completion_preserves_the_current_poll_fence() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let project = ProjectId::new();
        app.repository_generations.insert(project, 2);
        app.refresh_ui
            .projects
            .insert(project, CancellationToken::new());

        drop(app.repository_loaded(false, project, 1, Err("late reply".into())));

        assert!(
            app.refresh_ui.projects.contains_key(&project),
            "a late result must not remove a newer read's in-flight token"
        );
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
                limit: crate::github::polling::RateLimit::secondary(
                    crate::github::polling::now() + 120,
                ),
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
