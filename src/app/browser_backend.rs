//! One bounded, non-reading Wayland capability check per actual parent window.
use super::*;
use crate::browser::{BrowserError, CapabilityProbe};
use crate::capability::BrowserUnavailable;
use uuid::Uuid;

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Default)]
pub(super) struct State {
    parent: Option<window::Id>,
    generation: Option<Uuid>,
    phase: Phase,
    progress: Option<ProbeProgress>,
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
        deadline: Instant,
        binding: Option<BrowserFactoryBinding>,
        probe_pending: Option<Uuid>,
    },
    Available,
    Unavailable(BrowserUnavailable),
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
            deadline: now + PROBE_TIMEOUT,
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
        if let Phase::Checking { deadline, .. } = &self.browser_backend.phase
            && Instant::now() >= *deadline
        {
            self.browser_backend
                .trace_check("binding_result_timeout", Instant::now());
            if let Ok(binding) = result {
                self.discard_browser_binding(binding);
            }
            return self.browser_factory_unavailable(BrowserUnavailable::TimedOut);
        }
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
        let Phase::Checking {
            deadline,
            binding,
            probe_pending,
        } = &mut self.browser_backend.phase
        else {
            return Task::none();
        };
        if Instant::now() >= *deadline {
            self.browser_backend
                .trace_check("poll_timeout", Instant::now());
            return self.browser_factory_unavailable(BrowserUnavailable::TimedOut);
        }
        let timer = probe_timer(generation);
        let Some(binding) = binding.as_ref().filter(|_| probe_pending.is_none()) else {
            // Deadline advances even if Winit never supplies a queued callback.
            return timer;
        };
        let operation = Uuid::new_v4();
        *probe_pending = Some(operation);
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
        // A late successful result cannot override the bounded deadline.
        if let Phase::Checking { deadline, .. } = &self.browser_backend.phase
            && Instant::now() >= *deadline
        {
            self.browser_backend
                .trace_check("probe_result_timeout", Instant::now());
            return self.browser_factory_unavailable(BrowserUnavailable::TimedOut);
        }
        let Phase::Checking { probe_pending, .. } = &mut self.browser_backend.phase else {
            return Task::none();
        };
        if *probe_pending != Some(operation) {
            return Task::none();
        }
        *probe_pending = None;
        if let Some(progress) = &mut self.browser_backend.progress {
            progress.probes_completed = progress.probes_completed.saturating_add(1);
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
            self.fail_runtime(card, reason.to_string());
        }
        self.status = Some(TransientNotice::error(reason.to_string()));
        Task::none()
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
    fn one_probe_per_parent_and_settled_failure_cannot_retry() {
        let mut state = State::default();
        let parent = window::Id::unique();
        let now = Instant::now();
        let generation = state.begin(parent, now).unwrap();
        assert!(state.checking(generation, Some(parent)));
        assert!(state.begin(parent, now).is_none());
        state.phase = Phase::Unavailable(BrowserUnavailable::TimedOut);
        assert!(state.begin(parent, now + PROBE_TIMEOUT).is_none());
        assert_eq!(state.unavailable(), Some(BrowserUnavailable::TimedOut));
        assert!(!state.checking(generation, Some(parent)));
        state.retire();
        let next = state.begin(parent, now).unwrap();
        assert_ne!(generation, next);
        assert!(!state.checking(generation, Some(parent)));
        assert!(state.checking(next, Some(parent)));
        assert!(!state.checking(next, Some(window::Id::unique())));
    }

    #[test]
    fn deadline_starts_at_parent_request_not_first_dispatch() {
        let mut state = State::default();
        let now = Instant::now();
        state.begin(window::Id::unique(), now).unwrap();
        let Phase::Checking {
            deadline,
            binding,
            probe_pending,
        } = &state.phase
        else {
            panic!()
        };
        assert_eq!(*deadline, now + PROBE_TIMEOUT);
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

    #[test]
    fn timeout_settles_waiting_cards_and_late_completion_cannot_install_factory() {
        let (mut app, card, parent) = app();
        let generation = app
            .browser_backend
            .begin(parent, Instant::now() - PROBE_TIMEOUT)
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
        let _ = app.browser_factory_probed(generation, Uuid::new_v4(), CapabilityProbe::Available);
        let _ = app.browser_factory_ready(generation, Err(BrowserUnavailable::NotWayland));
        assert_eq!(
            app.browser_unavailable_reason(),
            Some(BrowserUnavailable::TimedOut)
        );
        assert!(app.browser_factory.is_none());
        let _ = app.browser_factory_task();
        assert_eq!(
            app.browser_unavailable_reason(),
            Some(BrowserUnavailable::TimedOut)
        );
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
        let focus = app.workspace.focused();
        for _ in 0..3 {
            let _ = app.retry_card(card);
            let _ = app.browser_factory_task();
        }
        assert_eq!(app.card_state(card).unwrap().kind, saved.kind);
        assert_eq!(app.card_state(card).unwrap().residency, saved.residency);
        assert_eq!(app.workspace.focused(), focus);
        assert!(app.browser_factory.is_none());
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
        assert!(app.browser_system_request(card).is_some());
    }

    #[test]
    fn new_browser_under_unavailable_parent_settles_without_engine_or_new_probe() {
        let (mut app, _, parent) = app();
        app.browser_backend.begin(parent, Instant::now()).unwrap();
        let _ = app.browser_factory_unavailable(BrowserUnavailable::NotWayland);
        let _ = app.add_browser(Url::parse("https://example.test/new").unwrap());
        let card = app.workspace.focused().unwrap();
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
        assert!(app.browser_factory.is_none());
        assert_eq!(
            app.browser_host_load(card),
            browser_host::LoadState::Unavailable
        );
        assert_eq!(
            app.browser_system_request(card).unwrap().1.as_str(),
            "https://example.test/new"
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
        let operation = Uuid::new_v4();
        if let Phase::Checking { probe_pending, .. } = &mut app.browser_backend.phase {
            *probe_pending = Some(operation);
        }
        let _ = app.browser_factory_probed(
            generation,
            Uuid::new_v4(),
            CapabilityProbe::Unavailable(BrowserUnavailable::InitializationFailed),
        );
        assert!(matches!(&app.browser_backend.phase,
            Phase::Checking { probe_pending: Some(current), .. } if *current == operation));
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
    fn delayed_binding_error_cannot_override_the_deadline() {
        let (mut app, _, parent) = app();
        let generation = app
            .browser_backend
            .begin(parent, Instant::now() - PROBE_TIMEOUT)
            .unwrap();
        let _ = app.browser_factory_ready(generation, Err(BrowserUnavailable::NotWayland));
        assert_eq!(
            app.browser_unavailable_reason(),
            Some(BrowserUnavailable::TimedOut)
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
