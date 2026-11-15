//! One bounded, non-reading Wayland capability check per actual parent window.
use super::*;
use crate::browser::{BrowserError, CapabilityProbe};
use crate::capability::BrowserUnavailable;
use uuid::Uuid;

/// How long the compositor may take to answer the check, counted from the
/// first probe, which sends the check's requests. Its answer takes
/// milliseconds; only a compositor that has stopped answering takes this long.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the window may take to run the check's first callbacks. They wait
/// their turn on the UI thread, which restoring a large workspace can hold for
/// seconds, and that wait says nothing about the compositor.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Default)]
pub(super) struct State {
    parent: Option<window::Id>,
    generation: Option<Uuid>,
    phase: Phase,
    progress: Option<ProbeProgress>,
    /// Browser panes this check settled as unavailable. A later check that
    /// finds the browser available loads them again.
    settled: Vec<CardId>,
}

struct ProbeProgress {
    started: Instant,
    polls: u32,
    probes_started: u32,
    probes_completed: u32,
    pending_reported: bool,
}

#[derive(Default)]
enum Phase {
    #[default]
    Idle,
    Checking {
        /// Until the first probe answers, when the window's callbacks are
        /// overdue; after it, when the compositor's answer is.
        deadline: Instant,
        /// Whether a probe has answered, so the check's requests are out.
        requested: bool,
        binding: Option<BrowserFactoryBinding>,
        probe_pending: Option<PendingProbe>,
    },
    Available,
    Unavailable(BrowserUnavailable),
}

#[derive(Clone, Copy)]
struct PendingProbe {
    operation: Uuid,
    queued: Instant,
}

impl State {
    fn begin(&mut self, parent: window::Id, now: Instant) -> Option<Uuid> {
        if self.parent == Some(parent) && !matches!(self.phase, Phase::Idle) {
            return None;
        }
        debug_assert!(matches!(self.phase, Phase::Idle));
        let generation = Uuid::new_v4();
        self.parent = Some(parent);
        self.generation = Some(generation);
        self.phase = Phase::Checking {
            deadline: now + CALLBACK_TIMEOUT,
            requested: false,
            binding: None,
            probe_pending: None,
        };
        self.progress = Some(ProbeProgress {
            started: now,
            polls: 0,
            probes_started: 0,
            probes_completed: 0,
            pending_reported: false,
        });
        self.trace_check("started", now);
        Some(generation)
    }

    // Call only at transitions or settlement, never on every timer tick.
    fn trace_check(&self, event: &'static str, now: Instant) {
        let (
            Some(generation),
            Some(progress),
            Phase::Checking {
                binding,
                probe_pending,
                ..
            },
        ) = (self.generation, self.progress.as_ref(), &self.phase)
        else {
            return;
        };
        tracing::info!(
            target: "cagents::browser_capability",
            %generation,
            phase = event,
            elapsed_ms = now.saturating_duration_since(progress.started).as_millis() as u64,
            polls = progress.polls,
            probes_started = progress.probes_started,
            probes_completed = progress.probes_completed,
            binding_ready = binding.is_some(),
            probe_pending = probe_pending.is_some(),
            "browser capability progress"
        );
    }

    fn checking(&self, generation: Uuid, parent: Option<window::Id>) -> bool {
        self.generation == Some(generation)
            && self.parent == parent
            && matches!(self.phase, Phase::Checking { .. })
    }

    pub(super) fn unavailable(&self) -> Option<BrowserUnavailable> {
        match self.phase {
            Phase::Unavailable(reason) => Some(reason),
            _ => None,
        }
    }

    /// Lets a check that may succeed if tried again start over. A settled
    /// reason that no new check can change, such as a missing protocol, stays.
    fn reopen(&mut self) -> bool {
        if !self
            .unavailable()
            .is_some_and(BrowserUnavailable::is_recoverable)
        {
            return false;
        }
        self.retire();
        true
    }

    pub(super) fn retire(&mut self) {
        self.trace_check("retired", Instant::now());
        // Clear protocol owners explicitly: window::run closures may still
        // retain a factory Arc after the application's actual parent closes.
        if let Phase::Checking {
            binding: Some(binding),
            ..
        } = &mut self.phase
        {
            binding.parent_lifetime.deactivate();
            binding.factory.release_parent();
        }
        self.phase = Phase::Idle;
        self.parent = None;
        self.generation = None;
        self.progress = None;
    }
}

impl Cagents {
    pub(super) fn browser_factory_task(&mut self) -> Task<Message> {
        if self.exit.is_some() {
            return Task::none();
        }
        let Some(parent_id) = self.core.main_window_id() else {
            // StartupWindowOpened will initiate the check when a real owner
            // exists. Never create a probe from borrowed handles elsewhere.
            return Task::none();
        };
        if self.browser_backend.parent.is_some() && self.browser_backend.parent != Some(parent_id) {
            self.release_browser_parent();
        }
        let Some(generation) = self.browser_backend.begin(parent_id, Instant::now()) else {
            return Task::none();
        };
        let binding = window::run(parent_id, move |parent| {
            tracing::info!(target: "cagents::browser_capability", %generation,
                phase = "binding_callback", "browser capability progress");
            let result = (|| {
                let display = parent
                    .display_handle()
                    .map_err(|_| BrowserUnavailable::ParentUnavailable)?
                    .as_raw();
                let window = parent
                    .window_handle()
                    .map_err(|_| BrowserUnavailable::ParentUnavailable)?
                    .as_raw();
                let parent_lifetime = Arc::new(ParentWindowLifetime::new(parent_id));
                let lifetime_guard: Arc<dyn Any + Send + Sync> = parent_lifetime.clone();
                let _probe_span = tracing::info_span!(
                    target: "cagents::browser_capability", "browser_protocol_probe", %generation
                )
                .entered();
                // SAFETY: Cagents retains this actual-parent token, joins
                // every browser worker, then explicitly clears all protocol
                // queues before the owner closes. This constructor creates
                // no queue: only an accepted parent callback starts probing.
                let factory = unsafe {
                    RawWaylandSubsurfaceFactory::from_raw_handles(display, window, lifetime_guard)
                }
                .map(Arc::new)
                .map_err(|error| match error {
                    BrowserError::Capability(reason) => reason,
                    _ => BrowserUnavailable::InitializationFailed,
                })?;
                Ok(BrowserFactoryBinding {
                    factory,
                    parent_lifetime,
                })
            })();
            Message::BrowserFactoryReady { generation, result }
        })
        .map(cosmic::Action::App);
        Task::batch([binding, probe_timer(generation)])
    }

    pub(super) fn browser_factory_ready(
        &mut self,
        generation: Uuid,
        result: Result<BrowserFactoryBinding, BrowserUnavailable>,
    ) -> Task<Message> {
        if self.exit.is_some()
            || !self
                .browser_backend
                .checking(generation, self.core.main_window_id())
        {
            if let Ok(binding) = result {
                self.discard_browser_binding(binding);
            }
            return Task::none();
        }
        // A binding that arrives late is still good: only the compositor's
        // silence after the check's requests are out can time the check out.
        match result {
            Ok(binding) => {
                if Some(binding.parent_lifetime.window_id) != self.core.main_window_id()
                    || !binding.parent_lifetime.active.load(Ordering::Acquire)
                {
                    self.browser_backend
                        .trace_check("binding_parent_unavailable", Instant::now());
                    self.discard_browser_binding(binding);
                    return self.browser_factory_unavailable(BrowserUnavailable::ParentUnavailable);
                }
                if let Phase::Checking { binding: slot, .. } = &mut self.browser_backend.phase {
                    // A duplicated completion cannot replace a live queue.
                    if slot.is_none() {
                        self.browser_parent_lifetime = Some(binding.parent_lifetime.clone());
                        *slot = Some(binding);
                        self.browser_backend
                            .trace_check("binding_ready", Instant::now());
                    }
                }
                Task::none()
            }
            Err(reason) => {
                self.browser_backend
                    .trace_check("binding_error", Instant::now());
                self.browser_factory_unavailable(reason)
            }
        }
    }

    fn discard_browser_binding(&self, binding: BrowserFactoryBinding) {
        let claimed = self
            .browser_factory
            .as_ref()
            .is_some_and(|factory| Arc::ptr_eq(factory, &binding.factory))
            || matches!(&self.browser_backend.phase,
                Phase::Checking { binding: Some(current), .. }
                if Arc::ptr_eq(&current.factory, &binding.factory));
        // A cloned completion may refer to the accepted winning factory.
        // Retiring only an unclaimed factory preserves its actual owner.
        if !claimed {
            binding.parent_lifetime.deactivate();
            binding.factory.release_parent();
        }
    }

    pub(super) fn browser_factory_poll(&mut self, generation: Uuid) -> Task<Message> {
        if self.exit.is_some()
            || !self
                .browser_backend
                .checking(generation, self.core.main_window_id())
        {
            return Task::none();
        }
        if let Some(progress) = &mut self.browser_backend.progress {
            progress.polls = progress.polls.saturating_add(1);
        }
        let now = Instant::now();
        let Phase::Checking {
            deadline,
            requested,
            binding,
            probe_pending,
        } = &mut self.browser_backend.phase
        else {
            return Task::none();
        };
        // Past the deadline, the check still looks once more before it gives
        // up: a poll that the busy UI thread ran late proves nothing about
        // the compositor. Only a probe queued after the deadline that still
        // finds no answer times it out, in `browser_factory_probed`. Winit
        // never running the callbacks at all is bounded here.
        let overdue = match probe_pending {
            _ if !*requested => now >= *deadline,
            Some(pending) => now >= pending.queued.max(*deadline) + CALLBACK_TIMEOUT,
            None => false,
        };
        if overdue {
            self.browser_backend.trace_check("poll_timeout", now);
            return self.browser_factory_unavailable(BrowserUnavailable::TimedOut);
        }
        let timer = probe_timer(generation);
        let Some(binding) = binding.as_ref().filter(|_| probe_pending.is_none()) else {
            // Deadline advances even if Winit never supplies a queued callback.
            return timer;
        };
        let operation = Uuid::new_v4();
        *probe_pending = Some(PendingProbe {
            operation,
            queued: now,
        });
        let factory = binding.factory.clone();
        let lifetime = binding.parent_lifetime.clone();
        let first_probe = self
            .browser_backend
            .progress
            .as_mut()
            .is_some_and(|progress| {
                progress.probes_started = progress.probes_started.saturating_add(1);
                progress.probes_started == 1
            });
        if first_probe {
            self.browser_backend
                .trace_check("first_probe_queued", Instant::now());
        }
        let task = window::run(lifetime.window_id, move |parent| {
            if first_probe {
                tracing::info!(target: "cagents::browser_capability", %generation,
                    phase = "first_probe_callback", "browser capability progress");
            }
            let result = if !lifetime.active.load(Ordering::Acquire) {
                CapabilityProbe::Unavailable(BrowserUnavailable::ParentUnavailable)
            } else {
                match (parent.display_handle(), parent.window_handle()) {
                    (Ok(display), Ok(window))
                        if factory.matches_parent(display.as_raw(), window.as_raw()) =>
                    {
                        factory.poll_capability()
                    }
                    _ => CapabilityProbe::Unavailable(BrowserUnavailable::ParentUnavailable),
                }
            };
            Message::BrowserFactoryProbed {
                generation,
                operation,
                result,
            }
        })
        .map(cosmic::Action::App);
        Task::batch([task, timer])
    }

    pub(super) fn browser_factory_probed(
        &mut self,
        generation: Uuid,
        operation: Uuid,
        result: CapabilityProbe,
    ) -> Task<Message> {
        if self.exit.is_some()
            || !self
                .browser_backend
                .checking(generation, self.core.main_window_id())
        {
            return Task::none();
        }
        let now = Instant::now();
        let Phase::Checking {
            deadline,
            requested,
            probe_pending,
            ..
        } = &mut self.browser_backend.phase
        else {
            return Task::none();
        };
        let Some(pending) = probe_pending.filter(|pending| pending.operation == operation) else {
            return Task::none();
        };
        *probe_pending = None;
        // The first probe sends the check's requests, so the compositor's
        // time to answer starts with it. An answer, however late the UI
        // thread delivers it, settles the check.
        let first_answer = !*requested;
        if first_answer {
            *requested = true;
            *deadline = now + PROBE_TIMEOUT;
        }
        let silent = matches!(result, CapabilityProbe::Pending)
            && !first_answer
            && pending.queued >= *deadline;
        if let Some(progress) = &mut self.browser_backend.progress {
            progress.probes_completed = progress.probes_completed.saturating_add(1);
        }
        if silent {
            self.browser_backend
                .trace_check("probe_result_timeout", now);
            return self.browser_factory_unavailable(BrowserUnavailable::TimedOut);
        }
        match result {
            CapabilityProbe::Pending => {
                let first_pending =
                    self.browser_backend
                        .progress
                        .as_mut()
                        .is_some_and(|progress| {
                            let first = !progress.pending_reported;
                            progress.pending_reported = true;
                            first
                        });
                if first_pending {
                    self.browser_backend
                        .trace_check("first_probe_pending", Instant::now());
                }
                Task::none()
            }
            CapabilityProbe::Available => {
                self.browser_backend
                    .trace_check("probe_available", Instant::now());
                let Phase::Checking { binding, .. } = &mut self.browser_backend.phase else {
                    unreachable!()
                };
                let Some(binding) = binding.take() else {
                    return self.browser_factory_unavailable(BrowserUnavailable::ParentUnavailable);
                };
                self.browser_factory = Some(binding.factory);
                self.browser_backend.phase = Phase::Available;
                self.browser_recovered();
                self.register_deferred_browsers();
                self.lifecycle_sweep()
            }
            CapabilityProbe::Unavailable(reason) => {
                self.browser_backend
                    .trace_check("probe_unavailable", Instant::now());
                self.browser_factory_unavailable(reason)
            }
        }
    }

    fn browser_factory_unavailable(&mut self, reason: BrowserUnavailable) -> Task<Message> {
        self.browser_backend.retire();
        // Preserve the parent-generation latch: retries may retry navigation,
        // but cannot create new protocol queues on this unavailable parent.
        self.browser_backend.parent = self.core.main_window_id();
        self.browser_backend.phase = Phase::Unavailable(reason);
        self.browser_parent_lifetime.take();
        let failed = self
            .durable
            .cards
            .iter()
            .filter(|card| matches!(card.kind, CardKind::Browser(_)))
            .filter(|card| matches!(self.cards.get(&card.id), Some(CardRuntime::Loading)))
            .map(|card| card.id)
            .collect::<Vec<_>>();
        for card in failed {
            self.settle_unavailable_browser(card, reason);
        }
        self.status = Some(
            TransientNotice::error(reason.to_string()).with_key(NoticeKey::BrowserUnavailable),
        );
        Task::none()
    }

    /// Shows a browser pane as unavailable for `reason`, and remembers it so
    /// that a later check that succeeds loads it.
    pub(super) fn settle_unavailable_browser(&mut self, card: CardId, reason: BrowserUnavailable) {
        self.fail_runtime(card, reason.to_string());
        if !self.browser_backend.settled.contains(&card) {
            self.browser_backend.settled.push(card);
        }
    }

    /// Withdraws an earlier check's report that the browser is unavailable,
    /// and returns the panes it gave up on to waiting, so they load as panes
    /// that were waiting for this check do. A pane closed since stays closed.
    fn browser_recovered(&mut self) {
        self.resolve_notice(NoticeKey::BrowserUnavailable);
        for card in std::mem::take(&mut self.browser_backend.settled) {
            if matches!(self.cards.get(&card), Some(CardRuntime::Failed(_)))
                && self.browser_system_request(card).is_some()
            {
                self.cards.insert(card, CardRuntime::Loading);
            }
        }
    }

    /// Checks again for the integrated browser after a check that may succeed
    /// on a second try, such as one the compositor was slow to answer. Panes
    /// it settled as unavailable load if this one finds the browser.
    pub(super) fn recheck_browser(&mut self) -> Task<Message> {
        if self.exit.is_some() || !self.browser_backend.reopen() {
            return Task::none();
        }
        self.browser_factory_task()
    }
}

fn probe_timer(generation: Uuid) -> Task<Message> {
    cosmic::task::future(async move {
        tokio::time::sleep(POLL_INTERVAL).await;
        Message::BrowserFactoryPoll(generation)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_probe_per_parent_and_only_a_timeout_reopens() {
        let mut state = State::default();
        let parent = window::Id::unique();
        let now = Instant::now();
        let generation = state.begin(parent, now).unwrap();
        assert!(state.checking(generation, Some(parent)));
        assert!(state.begin(parent, now).is_none());
        state.phase = Phase::Unavailable(BrowserUnavailable::NotWayland);
        assert!(!state.reopen());
        assert!(state.begin(parent, now + PROBE_TIMEOUT).is_none());
        assert_eq!(state.unavailable(), Some(BrowserUnavailable::NotWayland));
        state.phase = Phase::Unavailable(BrowserUnavailable::TimedOut);
        assert!(state.begin(parent, now).is_none());
        assert!(state.reopen());
        assert!(state.unavailable().is_none());
        let next = state.begin(parent, now).unwrap();
        assert_ne!(generation, next);
        assert!(!state.checking(generation, Some(parent)));
        assert!(state.checking(next, Some(parent)));
        assert!(!state.checking(next, Some(window::Id::unique())));
    }

    #[test]
    fn callbacks_have_their_own_bound_until_the_first_probe_answers() {
        let mut state = State::default();
        let now = Instant::now();
        state.begin(window::Id::unique(), now).unwrap();
        let Phase::Checking {
            deadline,
            requested,
            binding,
            probe_pending,
        } = &state.phase
        else {
            panic!()
        };
        assert_eq!(*deadline, now + CALLBACK_TIMEOUT);
        assert!(!requested);
        assert!(binding.is_none());
        assert!(probe_pending.is_none());
    }
}

#[cfg(test)]
mod action_tests {
    use super::*;
    use crate::capability::MissingWaylandProtocols;

    fn app() -> (Cagents, CardId, window::Id) {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let url =
            Url::parse("https://alice:password@example.test/path?secret=private#token").unwrap();
        app.card_state_mut(card).unwrap().kind = CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: url.clone(),
            history: vec![url],
            history_index: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            fidelity: BrowserRestoreFidelity::HistoryAndScroll,
        });
        let parent = window::Id::unique();
        app.core.set_main_window_id(Some(parent));
        (app, card, parent)
    }

    fn pend(app: &mut Cagents, queued: Instant) -> Uuid {
        let operation = Uuid::new_v4();
        let Phase::Checking { probe_pending, .. } = &mut app.browser_backend.phase else {
            panic!("not checking")
        };
        *probe_pending = Some(PendingProbe { operation, queued });
        operation
    }

    fn deadline(app: &Cagents) -> Instant {
        let Phase::Checking { deadline, .. } = &app.browser_backend.phase else {
            panic!("not checking")
        };
        *deadline
    }

    #[test]
    fn overdue_callbacks_time_out_and_retry_checks_again() {
        let (mut app, card, parent) = app();
        let generation = app
            .browser_backend
            .begin(parent, Instant::now() - CALLBACK_TIMEOUT)
            .unwrap();
        let _ = app.browser_factory_poll(generation);
        assert_eq!(
            app.browser_unavailable_reason(),
            Some(BrowserUnavailable::TimedOut)
        );
        assert_eq!(
            app.browser_host_load(card),
            browser_host::LoadState::Unavailable
        );
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
        assert_eq!(app.browser_backend.settled, vec![card]);
        // A retired check's results install nothing.
        let _ = app.browser_factory_probed(generation, Uuid::new_v4(), CapabilityProbe::Available);
        let _ = app.browser_factory_ready(generation, Err(BrowserUnavailable::NotWayland));
        assert_eq!(
            app.browser_unavailable_reason(),
            Some(BrowserUnavailable::TimedOut)
        );
        assert!(app.browser_factory.is_none());
        // Nothing starts a check again by itself...
        let _ = app.browser_factory_task();
        assert_eq!(
            app.browser_unavailable_reason(),
            Some(BrowserUnavailable::TimedOut)
        );
        // ...but Retry does, and the pane waits for it.
        let _ = app.retry_card(card);
        assert!(app.browser_unavailable_reason().is_none());
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert!(matches!(app.browser_backend.phase, Phase::Checking { .. }));
        assert_ne!(app.browser_backend.generation, Some(generation));
        assert_eq!(
            app.browser_host_load(card),
            browser_host::LoadState::Loading
        );
    }

    #[test]
    fn a_ui_thread_stall_before_the_first_probe_does_not_time_out_the_check() {
        let (mut app, card, parent) = app();
        // Restoring the workspace held the UI thread well past the
        // compositor's deadline before the first probe could run.
        let begun = Instant::now() - 4 * PROBE_TIMEOUT;
        let generation = app.browser_backend.begin(parent, begun).unwrap();
        let _ = app.browser_factory_poll(generation);
        assert!(app.browser_unavailable_reason().is_none());
        let first = pend(&mut app, begun + 3 * PROBE_TIMEOUT);
        let answered = Instant::now();
        let _ = app.browser_factory_probed(generation, first, CapabilityProbe::Pending);
        assert!(app.browser_unavailable_reason().is_none());
        // The compositor's time starts with the requests the first probe sent.
        assert!(deadline(&app) >= answered + PROBE_TIMEOUT);
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
    }

    #[test]
    fn a_late_poll_looks_again_and_only_silence_after_the_deadline_times_out() {
        let (mut app, card, parent) = app();
        let generation = app.browser_backend.begin(parent, Instant::now()).unwrap();
        let first = pend(&mut app, Instant::now());
        let _ = app.browser_factory_probed(generation, first, CapabilityProbe::Pending);
        // The UI thread delivers the next poll after the deadline: it looks
        // again rather than giving up on what it has not seen.
        if let Phase::Checking { deadline, .. } = &mut app.browser_backend.phase {
            *deadline = Instant::now() - POLL_INTERVAL;
        }
        let _ = app.browser_factory_poll(generation);
        assert!(app.browser_unavailable_reason().is_none());
        // A probe queued before the deadline proves nothing when it answers
        // late, however long the UI thread held it.
        let due = deadline(&app);
        let before = pend(&mut app, due - POLL_INTERVAL);
        let _ = app.browser_factory_probed(generation, before, CapabilityProbe::Pending);
        assert!(app.browser_unavailable_reason().is_none());
        // One queued after it that still finds no answer does.
        let after = pend(&mut app, due);
        let _ = app.browser_factory_probed(generation, after, CapabilityProbe::Pending);
        assert_eq!(
            app.browser_unavailable_reason(),
            Some(BrowserUnavailable::TimedOut)
        );
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
    }

    #[test]
    fn a_probe_winit_never_runs_is_bounded() {
        let (mut app, _, parent) = app();
        let generation = app.browser_backend.begin(parent, Instant::now()).unwrap();
        let first = pend(&mut app, Instant::now());
        let _ = app.browser_factory_probed(generation, first, CapabilityProbe::Pending);
        let lost = Instant::now() - PROBE_TIMEOUT - CALLBACK_TIMEOUT;
        if let Phase::Checking { deadline, .. } = &mut app.browser_backend.phase {
            *deadline = lost;
        }
        pend(&mut app, lost);
        let _ = app.browser_factory_poll(generation);
        assert_eq!(
            app.browser_unavailable_reason(),
            Some(BrowserUnavailable::TimedOut)
        );
    }

    #[test]
    fn a_check_that_succeeds_withdraws_the_report_and_loads_the_panes_an_earlier_one_gave_up_on() {
        let (mut app, card, parent) = app();
        app.browser_backend.begin(parent, Instant::now()).unwrap();
        let _ = app.browser_factory_unavailable(BrowserUnavailable::TimedOut);
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
        let closed = CardId::new();
        app.browser_backend.settled.push(closed);
        // The report the window has drained into its notices goes too.
        let report = app.status.take().unwrap();
        app.report_notice(report);
        assert!(app.notice_stands(NoticeKey::BrowserUnavailable));
        app.browser_recovered();
        assert!(!app.notice_stands(NoticeKey::BrowserUnavailable));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert!(!app.cards.contains_key(&closed));
        assert!(app.browser_backend.settled.is_empty());
    }

    /// The labels of the enabled buttons the whole window draws.
    fn buttons(app: &Cagents) -> Vec<String> {
        use iced::{
            Size,
            advanced::clipboard,
            mouse,
            runtime::user_interface::{Cache, UserInterface},
        };
        let mut renderer = super::super::tests::headless_renderer();
        let mut ui = UserInterface::build(
            cosmic::ApplicationExt::view_main(app),
            Size::new(1280.0, 900.0),
            Cache::default(),
            &mut renderer,
        );
        ui.update(
            &[],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut Vec::new(),
        );
        ui.draw(
            &mut renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            mouse::Cursor::Unavailable,
        );
        let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
        tree.root()
            .iter()
            .chain(tree.children())
            .filter(|node| {
                node.node().role() == iced_accessibility::accesskit::Role::Button
                    && !node.node().is_disabled()
            })
            .filter_map(|node| node.node().label().map(str::to_owned))
            .collect()
    }

    // The headless renderer starts its own runtime, so this test drives its
    // tasks on one of its own rather than running inside one.
    #[test]
    fn a_restored_pane_offers_the_system_browser_without_opening_it() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for (reason, retry) in [
            (BrowserUnavailable::TimedOut, true),
            (BrowserUnavailable::NotWayland, false),
        ] {
            let (mut app, card, parent) = app();
            let opened = recording(&mut app);
            app.browser_backend.begin(parent, Instant::now()).unwrap();
            // The restored pane was waiting for the check when it gave up.
            let task = app.browser_factory_unavailable(reason);
            runtime.block_on(settle(&mut app, task));
            let task = app.lifecycle_sweep();
            runtime.block_on(settle(&mut app, task));
            assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
            assert!(
                opened.lock().unwrap().is_empty(),
                "{reason:?} opened a page"
            );
            let buttons = buttons(&app);
            assert!(
                buttons
                    .iter()
                    .any(|label| label == "Open this page in your system browser"),
                "{reason:?}: {buttons:?}"
            );
            assert_eq!(
                buttons
                    .iter()
                    .any(|label| label == "Retry the integrated browser"),
                retry,
                "{reason:?}: {buttons:?}"
            );
            // The saved descriptor is what the action opens.
            let (_, url) = app.browser_system_request(card).unwrap();
            assert_eq!(url.host_str(), Some("example.test"));
            let task = app.open_browser_in_system(card);
            runtime.block_on(settle(&mut app, task));
            assert_eq!(opened.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn missing_protocol_latch_blocks_retry_without_rewriting_saved_descriptor() {
        let (mut app, card, parent) = app();
        app.browser_backend.begin(parent, Instant::now()).unwrap();
        let missing = MissingWaylandProtocols::from_advertised([
            ("wl_compositor", 1),
            ("wl_subcompositor", 1),
        ]);
        let _ = app.browser_factory_unavailable(BrowserUnavailable::MissingProtocols(missing));
        let saved = app.card_state(card).unwrap().clone();
        let residency = app.residency(card).clone();
        let focus = app.workspace.focused();
        for _ in 0..3 {
            let _ = app.retry_card(card);
            let _ = app.browser_factory_task();
        }
        assert_eq!(app.card_state(card).unwrap().kind, saved.kind);
        assert_eq!(*app.residency(card), residency);
        assert_eq!(app.workspace.focused(), focus);
        assert!(app.browser_factory.is_none());
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
        assert!(app.browser_system_request(card).is_some());
    }

    async fn settle(app: &mut Cagents, task: Task<Message>) -> Vec<Message> {
        use cosmic::iced::futures::StreamExt;
        let mut messages = Vec::new();
        if let Some(mut stream) = iced::runtime::task::into_stream(task) {
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                    messages.push(message);
                }
            }
        }
        for message in &messages {
            if let Message::BrowserFallbackOpened { result, reason } = message {
                let _ = app.update(Message::BrowserFallbackOpened {
                    result: result.clone(),
                    reason: *reason,
                });
            }
        }
        messages
    }

    fn recording(app: &mut Cagents) -> Arc<std::sync::Mutex<Vec<Url>>> {
        let opened = Arc::default();
        app.system_url_opener = file_manager::SystemUrlOpener::Recording(Arc::clone(&opened));
        opened
    }

    #[tokio::test]
    async fn an_explicit_open_goes_to_the_system_browser_while_unavailable() {
        let (mut app, _, parent) = app();
        let opened = recording(&mut app);
        app.browser_backend.begin(parent, Instant::now()).unwrap();
        let _ = app.browser_factory_unavailable(BrowserUnavailable::NotWayland);
        let cards = app.durable.cards.len();
        let focused = app.workspace.focused();
        let page = Url::parse("https://example.test/new").unwrap();
        let task = app.add_browser(page.clone());
        settle(&mut app, task).await;
        assert_eq!(*opened.lock().unwrap(), vec![page]);
        // No dead-end pane, and the reason is in the notice.
        assert_eq!(app.durable.cards.len(), cards);
        assert_eq!(app.workspace.focused(), focused);
        let notice = &app
            .notices
            .entries
            .iter()
            .find(|entry| entry.notice.message.contains("system browser"))
            .expect("a notice saying where the page went")
            .notice;
        assert_eq!(
            notice.details.as_deref(),
            Some(BrowserUnavailable::NotWayland.to_string().as_str())
        );
        // A settled reason no check can change starts no new one.
        assert_eq!(
            app.browser_unavailable_reason(),
            Some(BrowserUnavailable::NotWayland)
        );
        assert!(app.browser_factory.is_none());
    }

    #[tokio::test]
    async fn an_explicit_open_after_a_timeout_also_checks_again() {
        let (mut app, _, parent) = app();
        let opened = recording(&mut app);
        app.browser_backend.begin(parent, Instant::now()).unwrap();
        let _ = app.browser_factory_unavailable(BrowserUnavailable::TimedOut);
        let page = Url::parse("https://example.test/next").unwrap();
        let task = app.add_browser(page.clone());
        // The check starts again so that the next page can open here.
        assert!(matches!(app.browser_backend.phase, Phase::Checking { .. }));
        let messages = settle(&mut app, task).await;
        assert_eq!(*opened.lock().unwrap(), vec![page]);
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, Message::BrowserFallbackOpened { .. }))
        );
    }

    #[tokio::test]
    async fn a_page_the_system_browser_cannot_show_still_gets_a_pane() {
        let (mut app, _, parent) = app();
        let opened = recording(&mut app);
        app.browser_backend.begin(parent, Instant::now()).unwrap();
        let _ = app.browser_factory_unavailable(BrowserUnavailable::NotWayland);
        let task = app.add_browser(Url::parse("about:blank").unwrap());
        settle(&mut app, task).await;
        assert!(opened.lock().unwrap().is_empty());
        let card = app.workspace.focused().unwrap();
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
        assert!(app.browser_factory.is_none());
        assert_eq!(
            app.browser_host_load(card),
            browser_host::LoadState::Unavailable
        );
    }

    #[test]
    fn old_generation_result_cannot_settle_a_new_parent() {
        let (mut app, _, parent) = app();
        let old = app.browser_backend.begin(parent, Instant::now()).unwrap();
        app.release_browser_parent();
        let next_parent = window::Id::unique();
        app.core.set_main_window_id(Some(next_parent));
        let next = app
            .browser_backend
            .begin(next_parent, Instant::now())
            .unwrap();
        let _ = app.browser_factory_ready(old, Err(BrowserUnavailable::NotWayland));
        let _ = app.browser_factory_probed(old, Uuid::new_v4(), CapabilityProbe::Available);
        assert!(app.browser_backend.checking(next, Some(next_parent)));
        assert!(app.browser_unavailable_reason().is_none());
        assert!(app.browser_factory.is_none());
    }
    #[test]
    fn prior_poll_completion_cannot_clear_a_new_pending_poll() {
        let (mut app, _, parent) = app();
        let generation = app.browser_backend.begin(parent, Instant::now()).unwrap();
        let operation = pend(&mut app, Instant::now());
        let _ = app.browser_factory_probed(
            generation,
            Uuid::new_v4(),
            CapabilityProbe::Unavailable(BrowserUnavailable::InitializationFailed),
        );
        assert!(matches!(&app.browser_backend.phase,
            Phase::Checking { probe_pending: Some(current), .. } if current.operation == operation));
        assert_eq!(
            app.browser_backend
                .progress
                .as_ref()
                .unwrap()
                .probes_completed,
            0
        );
        let _ = app.browser_factory_probed(generation, operation, CapabilityProbe::Pending);
        assert!(matches!(
            &app.browser_backend.phase,
            Phase::Checking {
                probe_pending: None,
                ..
            }
        ));
        let progress = app.browser_backend.progress.as_ref().unwrap();
        assert_eq!(progress.probes_completed, 1);
        assert!(progress.pending_reported);
        app.release_browser_parent();
        assert!(app.browser_backend.progress.is_none());
        app.browser_backend.begin(parent, Instant::now()).unwrap();
        let progress = app.browser_backend.progress.as_ref().unwrap();
        assert_eq!(progress.probes_completed, 0);
        assert!(!progress.pending_reported);
    }

    #[test]
    fn a_late_binding_error_still_reports_its_own_reason() {
        let (mut app, _, parent) = app();
        let generation = app
            .browser_backend
            .begin(parent, Instant::now() - PROBE_TIMEOUT)
            .unwrap();
        let _ = app.browser_factory_ready(generation, Err(BrowserUnavailable::NotWayland));
        assert_eq!(
            app.browser_unavailable_reason(),
            Some(BrowserUnavailable::NotWayland)
        );
    }
    #[cfg(feature = "browser-protocol-fixture")]
    #[test]
    fn cloned_binding_completion_preserves_the_accepted_factory_until_parent_release() {
        let (mut app, _, parent) = app();
        let factory = RawWaylandSubsurfaceFactory::prepared_registry_fixture();
        let lifetime = Arc::new(ParentWindowLifetime::new(parent));
        app.browser_factory = Some(factory.clone());
        app.browser_parent_lifetime = Some(lifetime.clone());
        app.browser_backend.parent = Some(parent);
        app.browser_backend.phase = Phase::Available;
        let _ = app.browser_factory_ready(
            Uuid::new_v4(),
            Ok(BrowserFactoryBinding {
                factory: factory.clone(),
                parent_lifetime: lifetime.clone(),
            }),
        );
        assert!(lifetime.active.load(Ordering::Acquire));
        use crate::browser::WaylandSubsurfaceFactory;
        assert_eq!(factory.availability(), CapabilityProbe::Available);
        app.release_browser_parent();
        assert!(!lifetime.active.load(Ordering::Acquire));
        assert_eq!(
            factory.availability(),
            CapabilityProbe::Unavailable(BrowserUnavailable::ParentUnavailable)
        );
    }
}
