use std::{
    any::Any,
    cell::{Cell, RefCell},
    collections::HashMap,
    panic::AssertUnwindSafe,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use crossbeam_channel::{Receiver, Select, Sender, TryRecvError};
use euclid::Scale;
use servo::RenderingContext as _;
use url::Url;

use super::{
    BackendSelection, BrowserBackend, BrowserError, BrowserEvent, BrowserInput, BrowserLifecycle,
    BrowserRestoreState, DelegateRequest, DelegateRequestId, InputMethodState, PhysicalViewport,
    SurfacePlacement,
    fallback::{FallbackSurface, WaylandSubsurfaceFactory},
    handle::{CoalescedCommands, Command, CommandMailbox, EventBus},
    input,
    lifecycle::LifecycleMachine,
};

/// Tick rate while Servo reports an active animation.
const ANIMATION_CADENCE: Duration = Duration::from_millis(16);
/// Tick rate for a mapped but idle card. It is a safety net for Servo's waker
/// and keeps the shared Wayland queue drained for a surface that is on screen.
const IDLE_CADENCE: Duration = Duration::from_millis(250);

/// What a card reports when the shared engine thread unwinds under it. Every
/// card on that engine gets it: they share one thread by design.
const ENGINE_PANIC_REASON: &str =
    "the shared browser engine thread panicked; every browser card it served was released";

/// A single card's contribution to the engine's select timeout, or `None` when
/// that card needs no timer at all.
///
/// A hidden card is unmapped and its webview is throttled: it produces no
/// frames, runs no animation, and generates no Wayland traffic worth draining,
/// so waking for it every 250 ms only burned a trip through the shared Wayland
/// mutex. Blocking is safe for shutdown too:
/// `CommandMailbox::request_shutdown` sets its flag before enqueuing, and its
/// enqueue can only fail while that card's command queue is non-empty - in
/// which case the select does not block and `Engine::reap` observes the flag
/// on the very next pass.
fn select_timeout(visible: bool, animating: bool) -> Option<Duration> {
    if !visible {
        None
    } else if animating {
        Some(ANIMATION_CADENCE)
    } else {
        Some(IDLE_CADENCE)
    }
}

/// Registration of one browser card on the shared Servo engine thread.
pub(crate) struct Registration {
    pub mailbox: Arc<CommandMailbox>,
    pub events: Arc<EventBus>,
    pub restore: BrowserRestoreState,
    pub visible: bool,
    pub fallback_factory: Option<Arc<dyn WaylandSubsurfaceFactory>>,
    /// Held by the engine for as long as this card owns any Servo or Wayland
    /// resource. Nothing is ever sent on it: `BrowserHandle::shutdown` waits for
    /// the paired receiver to disconnect, which is what replaces joining a
    /// per-card thread.
    pub teardown: Sender<()>,
}

/// The live engine thread's registration channel, tagged with a generation so a
/// retiring engine can never evict its successor.
static ENGINE: Mutex<Option<(u64, Sender<Registration>)>> = Mutex::new(None);
static ENGINE_GENERATIONS: AtomicU64 = AtomicU64::new(1);

pub(crate) fn on_engine_thread() -> bool {
    crate::runtime_state::on_browser_engine_thread()
}

/// Hands a card to the shared engine thread, starting that thread if no engine
/// is currently live.
///
/// One `servo::Servo` serves every card. Six hot cards used to mean six
/// complete engines, each with its own constellation, resource and fetch
/// threads, image and font caches and JIT'd JS runtime. Only the WebRender
/// instance and the GL surface are genuinely per card, and those stay per card:
/// each webview keeps its own `RenderingContext`, so Servo gives it its own
/// painter and its own Wayland subsurface.
pub(crate) fn register(registration: Registration) -> Result<(), BrowserError> {
    let mut slot = lock_registry();
    let registration = match slot.as_ref() {
        Some((_, sender)) => match sender.send(registration) {
            Ok(()) => return Ok(()),
            // Only reachable if the engine thread died without retiring.
            Err(crossbeam_channel::SendError(registration)) => registration,
        },
        None => registration,
    };

    let generation = ENGINE_GENERATIONS.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = crossbeam_channel::unbounded();
    let _ = sender.send(registration);
    std::thread::Builder::new()
        .name("cagents-servo-engine".into())
        .spawn(move || Engine::run(generation, receiver))
        .map_err(|error| BrowserError::Servo(format!("could not start browser thread: {error}")))?;
    *slot = Some((generation, sender));
    Ok(())
}

/// Clears a retiring engine's registry slot.
///
/// Holding the registry lock across the emptiness check is what makes retiring
/// safe: `register` cannot deposit a card between the check and the clear. The
/// generation guard keeps an engine that was already replaced from evicting its
/// successor.
fn retire(generation: u64, registrations: &Receiver<Registration>) -> bool {
    let mut slot = lock_registry();
    if !registrations.is_empty() {
        return false;
    }
    if slot.as_ref().is_some_and(|(live, _)| *live == generation) {
        *slot = None;
    }
    true
}

/// Locks the engine registry, ignoring poisoning.
///
/// The registry holds one `Option<Sender>` and is only ever locked by `register`
/// and `retire`, neither of which can leave it half-updated. Treating a poisoned
/// lock as fatal would instead make one unrelated panic permanently prevent
/// every browser card from starting.
fn lock_registry() -> std::sync::MutexGuard<'static, Option<(u64, Sender<Registration>)>> {
    ENGINE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The shared Servo runtime and every browser card it serves.
struct Engine {
    generation: u64,
    /// Declared ahead of `servo` deliberately: fields drop in declaration
    /// order, and dropping every webview before the runtime handle is what
    /// lets the shared runtime finish shutting the constellation down. An
    /// engine thread that unwinds then gets the same ordering `run` applies by
    /// hand on its way out.
    cards: Vec<Card>,
    /// One runtime for every card, kept for the life of this thread.
    ///
    /// `servo::Servo` is `!Send`/`!Sync` — it is `Rc<ServoInner>` over
    /// `Rc`/`RefCell` state — so it is created on and confined to this thread.
    /// It also initializes process-global options exactly once:
    /// `opts::initialize_options` panics with "Already initialized" on a second
    /// `Servo::new`, including on a fresh thread. Dropping the runtime when the
    /// last webview closes would make the next card's engine unwind there and
    /// report [`ENGINE_PANIC_REASON`]. A closed webview is not runtime shutdown.
    servo: Option<servo::Servo>,
    import: Option<super::import::Pending>,
    /// `None` once the registry has released this engine's channel.
    registrations: Option<Receiver<Registration>>,
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
    wake_queued: Arc<AtomicBool>,
}

/// Turns an engine thread that unwound into a visible per-card failure.
///
/// A panic in the shared loop takes every card with it - that is the price of
/// one engine serving N cards - but it must never surface as a hang or as a
/// card that silently stopped painting. `run` empties `cards` on the orderly
/// path, so anything still here means the loop unwound.
///
/// Reporting has to happen before those cards drop: a card's teardown sender is
/// what releases `BrowserHandle::shutdown`, so a handle that woke from that
/// barrier with nothing on its event bus would leave a blank card on screen
/// with no error to render.
impl Drop for Engine {
    fn drop(&mut self) {
        for card in &mut self.cards {
            card.fault(ENGINE_PANIC_REASON.into());
        }
        // A card that reached the registration channel but never became a
        // `Card` has no bus of ours to report through, so fail it from its own
        // registration. A successful `retire` guarantees this channel is empty
        // and stays empty, so on the orderly path this drains nothing; failing
        // loudly is the right answer if that invariant is ever broken.
        if let Some(registrations) = self.registrations.take() {
            while let Ok(registration) = registrations.try_recv() {
                registration.mailbox.mark_engine_gone();
                registration.events.semantic(BrowserEvent::Crashed {
                    reason: ENGINE_PANIC_REASON.into(),
                    backtrace: None,
                });
            }
        }
    }
}

impl Engine {
    fn run(generation: u64, registrations: Receiver<Registration>) {
        crate::runtime_state::enter_browser_engine_thread();
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        let mut engine = Engine {
            generation,
            cards: Vec::new(),
            servo: None,
            import: None,
            registrations: Some(registrations),
            wake_tx,
            wake_rx,
            wake_queued: Arc::new(AtomicBool::new(false)),
        };
        engine.event_loop();
        // Cards go first. The runtime drops only because this thread is
        // leaving, which is not the last-card path: that one parks with the
        // runtime still installed. Shutdown can panic inside Servo; catching
        // it keeps Drop from turning a finished thread into a second fault.
        engine.cards.clear();
        let runtime = engine.servo.take();
        let _recovered = crate::instance::recovered_panic_scope();
        if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(runtime))) {
            tracing::error!(
                "browser runtime shutdown panicked: {}",
                panic_detail(payload.as_ref())
            );
        }
    }

    fn event_loop(&mut self) {
        loop {
            let timeout = self.timeout();
            let (ready, registration_op, card_base) = {
                let mut select = Select::new();
                let registration_op = self.registrations.as_ref().map(|rx| select.recv(rx));
                let wake_op = select.recv(&self.wake_rx);
                for card in &self.cards {
                    select.recv(card.mailbox.receiver());
                }
                let ready = match timeout {
                    Some(timeout) => select.ready_timeout(timeout).ok(),
                    None => Some(select.ready()),
                };
                (ready, registration_op, wake_op + 1)
            };

            match ready {
                // The cadence expired. Only an animating card needs a tick; a
                // mapped idle card just wanted its Wayland queue drained.
                None => {
                    if self.cards.iter().any(Card::animating) {
                        self.tick();
                    }
                }
                Some(op) if Some(op) == registration_op => self.accept_registrations(),
                Some(op) if op + 1 == card_base => {
                    let _ = self.wake_rx.try_recv();
                    self.wake_queued.store(false, Ordering::Release);
                    self.tick();
                }
                Some(op) => self.dispatch_command(op - card_base),
            }

            if let (Some(import), Some(servo)) = (&mut self.import, &self.servo)
                && import.step(servo)
            {
                self.import = None;
            }
            self.reap();
            self.drain_wayland();
            // An empty card list is not a reason to leave. The runtime cannot
            // be constructed again, so this thread stays registered and the
            // next card joins it. The thread ends only once its registration
            // channel is gone, which is a replacement, not a closed tab.
            if self.cards.is_empty() && self.registrations.is_none() {
                break;
            }
        }
    }

    /// The soonest cadence any card still needs, or `None` to block until a
    /// command, a registration or a Servo wake actually arrives.
    fn timeout(&self) -> Option<Duration> {
        if self.import.is_some() {
            return Some(Duration::from_millis(1));
        }
        self.cards.iter().filter_map(Card::select_timeout).min()
    }

    fn accept_registrations(&mut self) {
        loop {
            let received = match self.registrations.as_ref() {
                Some(registrations) => registrations.try_recv(),
                None => return,
            };
            match received {
                Ok(registration) => self.register(registration),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    // Nothing is buffered: `try_recv` only reports disconnection
                    // once the channel is empty. Clear the slot if this
                    // generation still owns it, then drop the receiver.
                    if let Some(registrations) = self.registrations.as_ref()
                        && !retire(self.generation, registrations)
                    {
                        return;
                    }
                    self.registrations = None;
                    return;
                }
            }
        }
    }

    fn register(&mut self, registration: Registration) {
        let Registration {
            mailbox,
            events,
            restore,
            visible,
            fallback_factory,
            teardown,
        } = registration;
        let restore = restore.normalized();
        self.cards.push(Card {
            mailbox,
            events,
            lifecycle: LifecycleMachine::new(visible),
            visible,
            residency: visible,
            // The widget publishes real geometry on its first update; until
            // then the application's residency decision alone governs.
            presentation: true,
            focused: false,
            placement: SurfacePlacement::unclipped(restore.viewport, restore.scale_factor),
            restore,
            session: None,
            fallback_factory,
            _teardown: teardown,
            closed: false,
        });
        if visible {
            self.create_session(self.cards.len() - 1);
        }
    }

    /// Gives card `index` a webview on the shared runtime.
    fn create_session(&mut self, index: usize) {
        // The card's own Wayland child and GL surface come first: a card that
        // cannot get one must not bring a Servo runtime into existence, and its
        // failure has to stay local to that card.
        let rendering = match self.cards[index].create_surface() {
            Ok(rendering) => rendering,
            Err(error) => {
                self.cards[index].fail_initialization(error);
                return;
            }
        };
        // `Servo` is an `Rc` handle; this clone exists only to keep the engine
        // borrow away from the card borrow.
        let servo = match self.servo_runtime() {
            Ok(servo) => servo,
            Err(error) => {
                self.cards[index].fail_initialization(error);
                return;
            }
        };
        self.cards[index].adopt_session(rendering, &servo);
    }

    /// The shared runtime, created on the first card that actually gets a surface.
    ///
    /// A second `Servo::new` panics process-wide. That panic used to unwind this
    /// thread and land on the new card as [`ENGINE_PANIC_REASON`]. Building is
    /// caught so a failed start faults the card and leaves the thread parked.
    fn servo_runtime(&mut self) -> Result<servo::Servo, BrowserError> {
        if let Some(servo) = self.servo.clone() {
            return Ok(servo);
        }
        let wake_tx = self.wake_tx.clone();
        let wake_queued = self.wake_queued.clone();
        let _recovered = crate::instance::recovered_panic_scope();
        let built = std::panic::catch_unwind(AssertUnwindSafe(|| {
            servo::ServoBuilder::default()
                .event_loop_waker(Box::new(BrowserWaker {
                    wake_tx,
                    wake_queued,
                }))
                .build()
        }));
        // The UI owns the process-wide log bridge; Servo's setup_logging
        // installs a second global logger and panics after UI startup.
        match built {
            Ok(servo) => {
                self.servo = Some(servo.clone());
                Ok(servo)
            }
            Err(payload) => Err(BrowserError::Servo(format!(
                "could not start the browser runtime: {}",
                panic_detail(payload.as_ref())
            ))),
        }
    }

    fn dispatch_command(&mut self, index: usize) {
        let Some(card) = self.cards.get_mut(index) else {
            return;
        };
        // A ready receiver can still come up empty when a producer lost a race.
        let Ok(command) = card.mailbox.receiver().try_recv() else {
            return;
        };
        card.mailbox.refill_deferred_releases();
        if let Command::Import(request) = command {
            if self.import.is_some() {
                let _ = request
                    .result
                    .send(Err("Another browser import is already running.".into()));
            } else if self.servo.is_none() {
                let _ = request
                    .result
                    .send(Err("Open an in-app browser tab before importing.".into()));
            } else {
                self.import = Some(request.into());
            }
            return;
        }
        if card.command(command) == CardRequest::CreateSession {
            self.create_session(index);
        }
        self.tick();
    }

    /// One engine tick: pump the shared event loop once for every card, then
    /// let each card present if Servo authorized a new frame for it.
    ///
    /// `spin_event_loop` is per engine and drives every webview's
    /// constellation traffic, while `paint` is per webview and per surface, so
    /// N cards cost one pump instead of N.
    fn tick(&mut self) {
        let spun = {
            let Some(servo) = self.servo.as_ref() else {
                return;
            };
            // Servo calls back into our delegates from this spin, and those are
            // already caught. A panic below that boundary used to unwind the
            // thread and fault every card, including one that had only just
            // registered. The cards that have a session are the ones it served.
            let _recovered = crate::instance::recovered_panic_scope();
            std::panic::catch_unwind(AssertUnwindSafe(|| {
                servo.spin_event_loop();
            }))
        };
        if let Err(payload) = spun {
            let detail = panic_detail(payload.as_ref());
            tracing::error!("the browser engine panicked while spinning: {detail}");
            for card in &mut self.cards {
                if card.session.is_some() {
                    card.fault(format!(
                        "the browser engine panicked while spinning: {detail}"
                    ));
                }
            }
            return;
        }
        for card in &mut self.cards {
            card.after_spin();
        }
    }

    /// Drops finished cards. The shared runtime stays for the life of the thread.
    fn reap(&mut self) {
        for card in &mut self.cards {
            // Our own delegate dispatch panicked for this webview. It was
            // caught at that boundary and has already reported itself, so this
            // card dies alone while every sibling keeps running.
            if card
                .session
                .as_ref()
                .is_some_and(|session| session.faulted.get())
            {
                card.closed = true;
            }
            // The shutdown flag is authoritative; its command packet is only a
            // wake-up that the queue is allowed to reject.
            if card.mailbox.shutdown_requested() {
                card.closed = true;
            }
            if card.closed {
                card.close();
            }
        }
        // The runtime stays installed after the last webview is gone. Shutting
        // it down here is what made the next card construct a second `Servo`
        // and paint [`ENGINE_PANIC_REASON`]. Hibernating a card still drops
        // that card's webview and surface; the constellation behind it remains.
        self.cards.retain(|card| !card.closed);
    }

    /// Drains Wayland events for cards that are not driving their own frames.
    fn drain_wayland(&self) {
        for card in &self.cards {
            if let Some(session) = &card.session
                && (!card.visible || !session.animating.get())
                && let Err(error) = session.dispatch_fallback()
            {
                card.events.semantic(BrowserEvent::Error(error));
            }
        }
    }
}

/// What a card needs from the shared engine after handling a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardRequest {
    None,
    CreateSession,
}

struct Card {
    mailbox: Arc<CommandMailbox>,
    events: Arc<EventBus>,
    restore: BrowserRestoreState,
    lifecycle: LifecycleMachine,
    /// Effective visibility: `residency && presentation`, cached so that every
    /// consumer reads one value.
    visible: bool,
    /// Application-owned input: whether this card may hold live resources at
    /// all. Written only by `BrowserHandle::set_visible`.
    residency: bool,
    /// Widget-owned input: whether any part of the card survives iced's clip.
    /// Written only by `BrowserHandle::set_presented`.
    presentation: bool,
    focused: bool,
    placement: SurfacePlacement,
    session: Option<CardSession>,
    fallback_factory: Option<Arc<dyn WaylandSubsurfaceFactory>>,
    /// Teardown barrier: dropping this card disconnects the receiver that
    /// `BrowserHandle::shutdown` blocks on, which is why the card must be
    /// dropped only after its webview, surface and lease are gone.
    _teardown: Sender<()>,
    closed: bool,
}

impl Card {
    fn select_timeout(&self) -> Option<Duration> {
        select_timeout(self.visible, self.animating())
    }

    fn animating(&self) -> bool {
        self.visible
            && self
                .session
                .as_ref()
                .is_some_and(|session| session.animating.get())
    }

    fn command(&mut self, command: Command) -> CardRequest {
        match command {
            Command::Import(request) => {
                let _ = request.result.send(Err(
                    "The browser import did not reach its shared runtime.".into(),
                ));
            }
            Command::WakeCoalesced => {
                return self.apply_coalesced(self.mailbox.drain_coalesced());
            }
            Command::Navigate(url) => {
                self.restore
                    .history
                    .truncate(self.restore.history_index + 1);
                self.restore.history.push(url.clone());
                self.restore.history_index = self.restore.history.len() - 1;
                // Forget the oldest entries rather than letting one long-lived
                // card turn every navigation into an ever larger serialize and
                // SQLite blob write.
                crate::model::trim_browser_history(
                    &mut self.restore.history,
                    &mut self.restore.history_index,
                );
                self.restore.url = url.clone();
                self.restore.scroll_x = 0.0;
                self.restore.scroll_y = 0.0;
                if let Some(session) = &self.session {
                    session.set_navigation(&self.restore, false);
                    session.webview.load(url);
                }
            }
            Command::Back => self.traverse(-1),
            Command::Forward => self.traverse(1),
            Command::Reload => {
                if let Some(session) = &self.session {
                    session.webview.reload();
                }
            }
            Command::Stop => {
                if let Some(session) = &self.session {
                    session
                        .webview
                        .evaluate_javascript("window.stop();", |_| {});
                }
            }
            Command::Hibernate => self.hibernate(),
            Command::Restore => return self.restore_resources(),
            Command::Input(event) => self.forward_input(event),
            Command::ClipboardText { id, result } => {
                if let Some(request) = self
                    .session
                    .as_ref()
                    .and_then(|session| session.pending_clipboard.borrow_mut().remove(&id))
                {
                    match result {
                        Ok(text) => request.success(text),
                        Err(error) => request.failure(error),
                    }
                }
            }
            Command::FilePickerResult { id, paths } => {
                if let Some(mut picker) = self
                    .session
                    .as_ref()
                    .and_then(|session| session.pending_file_pickers.borrow_mut().remove(&id))
                {
                    if let Some(paths) = paths {
                        picker.select(&paths);
                        picker.submit();
                    } else {
                        picker.dismiss();
                    }
                }
            }
            Command::Shutdown => self.closed = true,
        }
        CardRequest::None
    }

    fn apply_coalesced(&mut self, commands: CoalescedCommands) -> CardRequest {
        let mut request = CardRequest::None;
        // One coalesced batch can carry a resize, a pan and a scale change at
        // once; all three feed the same native geometry, so apply them together
        // and touch the child surface once.
        let mut geometry_changed = false;
        if let Some(viewport) = commands.viewport {
            geometry_changed |= self.restore.viewport != viewport;
            self.restore.viewport = viewport;
        }
        if let Some(placement) = commands.placement {
            geometry_changed |= self.placement != placement;
            self.placement = placement;
        }
        if let Some(scale) = commands.scale_factor {
            let scale = if scale.is_finite() {
                scale.max(0.01)
            } else {
                1.0
            };
            geometry_changed |= self.restore.scale_factor != scale;
            self.restore.scale_factor = scale;
            if let Some(session) = &self.session {
                session.webview.set_hidpi_scale_factor(Scale::new(scale));
            }
        }
        if geometry_changed
            && let Some(session) = &self.session
            && let Err(error) = session.resize_fallback(self.placement, self.restore.viewport)
        {
            self.events.semantic(BrowserEvent::Error(error));
        }

        // Effective visibility has two independent owners: the widget owns
        // presentation (real geometry) and the application owns residency
        // (hibernate/restore policy). A single shared flag let whichever wrote
        // last in a frame win, so a card that was one pixel outside its clip
        // oscillated against its own visibility class. Neither owner can now
        // observe or overwrite the other's value.
        if commands.residency.is_some() || commands.presented.is_some() {
            if let Some(residency) = commands.residency {
                self.residency = residency;
            }
            if let Some(presented) = commands.presented {
                self.presentation = presented;
            }
            let visible = self.residency && self.presentation;
            let changed = visible != self.visible;
            self.visible = visible;
            if changed
                && let Some(session) = &self.session
                && let Err(error) = session.set_visible(visible)
            {
                self.events.semantic(BrowserEvent::Error(error));
            }
            if self.session.is_some() {
                if changed && self.lifecycle.set_visible(visible).is_ok() {
                    self.events
                        .semantic(BrowserEvent::LifecycleChanged(self.lifecycle.state()));
                }
            } else if visible {
                request = self.restore_resources();
            }
        }
        if let Some(focused) = commands.focused {
            self.focused = focused;
            if let Some(session) = &self.session {
                if focused {
                    session.webview.focus();
                } else {
                    session.webview.blur();
                }
            }
        }
        for event in [
            commands.pointer_motion,
            commands.wheel,
            commands.touch_motion,
        ]
        .into_iter()
        .flatten()
        {
            self.forward_input(event);
        }
        request
    }

    fn forward_input(&mut self, event: BrowserInput) {
        if !self.visible || self.session.is_none() {
            return;
        }
        let session = self.session.as_ref().expect("session checked");
        let id = session
            .webview
            .notify_input_event(input::to_servo(event.clone()));
        let sequence = session.next_input_sequence.fetch_add(1, Ordering::Relaxed);
        session.input_sequences.borrow_mut().insert(id, sequence);
        if let BrowserInput::Wheel { x, y, .. } = event {
            self.restore.scroll_x = (self.restore.scroll_x - x).max(0.0);
            self.restore.scroll_y = (self.restore.scroll_y - y).max(0.0);
        }
    }

    fn traverse(&mut self, delta: isize) {
        let new_index = self.restore.history_index.saturating_add_signed(delta);
        if new_index >= self.restore.history.len() || new_index == self.restore.history_index {
            return;
        }
        self.restore.history_index = new_index;
        self.restore.url = self.restore.history[new_index].clone();
        self.restore.scroll_x = 0.0;
        self.restore.scroll_y = 0.0;
        if let Some(session) = &self.session {
            session.set_navigation(&self.restore, true);
            session.webview.load(self.restore.url.clone());
        }
        self.events.semantic(BrowserEvent::HistoryChanged {
            entries: self.restore.history.clone(),
            current: self.restore.history_index,
        });
    }

    /// Releases this card's webview and surface while the shared runtime and
    /// every sibling card stay hot.
    fn hibernate(&mut self) {
        if self.session.is_none() {
            return;
        }
        if self.lifecycle.begin_hibernation().is_ok() {
            self.events
                .semantic(BrowserEvent::LifecycleChanged(self.lifecycle.state()));
        }
        self.destroy_session();
        if self.lifecycle.resources_destroyed().is_ok() {
            self.events
                .semantic(BrowserEvent::LifecycleChanged(self.lifecycle.state()));
        }
    }

    fn restore_resources(&mut self) -> CardRequest {
        if self.session.is_some() || !self.visible {
            return CardRequest::None;
        }
        if self.lifecycle.state() == BrowserLifecycle::Hibernated {
            let _ = self.lifecycle.begin_restore();
            self.events
                .semantic(BrowserEvent::LifecycleChanged(self.lifecycle.state()));
        }
        CardRequest::CreateSession
    }

    fn create_surface(&self) -> Result<FallbackSurface, BrowserError> {
        let factory = self.fallback_factory.as_ref().ok_or_else(|| {
            BrowserError::Wayland(
                "a Wayland child-subsurface factory is required; the stock-wgpu external-memory bridge is disabled"
                    .into(),
            )
        })?;
        // Never initialize the external-memory bridge: stock wgpu cannot
        // serialize its queue with bridge-owned raw vkQueueSubmit calls.
        FallbackSurface::create(factory, self.restore.viewport, self.restore.scale_factor)
    }

    fn adopt_session(&mut self, rendering: FallbackSurface, servo: &servo::Servo) {
        let session = match CardSession::create(
            &self.restore,
            self.visible,
            self.events.clone(),
            rendering,
            servo,
        ) {
            Ok(session) => session,
            Err(error) => {
                self.fail_initialization(error);
                return;
            }
        };
        if let Err(error) = session.resize_fallback(self.placement, self.restore.viewport) {
            session.shutdown();
            self.fail_initialization(error);
            return;
        }
        self.session = Some(session);
        let _ = self.lifecycle.resources_created(self.visible);
        self.events.semantic(BrowserEvent::Ready {
            backend: BackendSelection {
                selected: BrowserBackend::WaylandSubsurface,
            },
        });
        self.events
            .semantic(BrowserEvent::LifecycleChanged(self.lifecycle.state()));
        if self.focused
            && let Some(session) = &self.session
        {
            session.webview.focus();
        }
    }

    fn fail_initialization(&mut self, error: BrowserError) {
        self.fault(format!("Browser initialization failed: {error}"));
    }

    /// Reports a fatal, card-local failure and schedules the card for release.
    ///
    /// `Crashed` is what the application turns into a rendered error state, and
    /// marking the mailbox is what makes every later call through the handle
    /// fail with [`BrowserError::ThreadStopped`] instead of queueing commands
    /// into a channel that will never be read again. Both matter on the engine
    /// panic path, where nothing else runs on this card's behalf.
    fn fault(&mut self, reason: String) {
        self.events.semantic(BrowserEvent::Crashed {
            reason,
            backtrace: None,
        });
        self.mailbox.mark_engine_gone();
        self.closed = true;
    }

    fn destroy_session(&mut self) {
        if let Some(session) = self.session.take() {
            session.store_navigation(&mut self.restore);
            session.shutdown();
        }
    }

    /// Releases this card's resources. The engine releases its teardown barrier
    /// only after any now-unowned shared runtime has also shut down.
    fn close(&mut self) {
        self.destroy_session();
        self.lifecycle.close();
        self.mailbox.mark_engine_gone();
        self.events
            .semantic(BrowserEvent::LifecycleChanged(BrowserLifecycle::Closed));
    }

    /// Follows one shared `spin_event_loop`. Only Servo's new-frame callback
    /// authorizes a paint, so an idle animation cannot present every 16 ms.
    fn after_spin(&mut self) {
        let Some(session) = &mut self.session else {
            return;
        };
        if session.restore_scroll_ready.replace(false) {
            let x = self.restore.scroll_x;
            let y = self.restore.scroll_y;
            session
                .webview
                .evaluate_javascript(format!("window.scrollTo({x},{y});"), |_| {});
        }
        if self.visible && session.frame_ready.replace(false) {
            session.paint(&self.events);
        }
    }
}

/// One card's webview on the shared runtime, together with the Wayland child
/// surface and rendering context that webview paints into.
struct CardSession {
    webview: servo::WebView,
    rendering: FallbackSurface,
    /// Physical size this session last asked Servo to render at.
    ///
    /// Deliberately *not* read back from the rendering context: Servo's resize
    /// destroys and recreates the surfman surface and only warns if that fails,
    /// leaving the context reporting its old size. Comparing against the
    /// context would then re-request the same failing resize on every frame, so
    /// the guard compares against exactly what was last handed to Servo.
    requested_buffer: Cell<dpi::PhysicalSize<u32>>,
    /// Set when our own delegate dispatch panicked; see [`guarded_dispatch`].
    /// `Engine::reap` closes this card and leaves its siblings alone.
    faulted: Rc<Cell<bool>>,
    animating: Rc<Cell<bool>>,
    frame_ready: Rc<Cell<bool>>,
    restore_scroll_ready: Rc<Cell<bool>>,
    input_sequences: Rc<RefCell<HashMap<servo::InputEventId, u64>>>,
    next_input_sequence: Arc<AtomicU64>,
    pending_clipboard: Rc<RefCell<HashMap<DelegateRequestId, servo::StringRequest>>>,
    pending_file_pickers: Rc<RefCell<HashMap<DelegateRequestId, servo::FilePicker>>>,
    navigation: Rc<RefCell<NavigationState>>,
}

/// The size Servo must be resized to, or `None` when it already renders at it.
///
/// Resizing destroys and recreates the rendering context's surfman surface, so
/// it may only fire on a real size change: a pan republishes the placement every
/// frame, and a card whose size is stable has to cost zero resizes.
///
/// `previous` records what was last handed to Servo rather than what the
/// rendering context reports, because those two can drift apart permanently -
/// Servo's resize only warns when surfman fails, leaving the context reporting
/// its old size while the webview believes the new one. Comparing against the
/// context would then re-issue the same failing resize forever.
///
/// The clamp keeps a degenerate size out of Servo for the same reason:
/// `WebView::resize` silently raises anything below 1x1 to its own minimum, so
/// an unclamped 0 would never compare equal to what was actually applied.
fn resize_request(
    previous: &Cell<dpi::PhysicalSize<u32>>,
    buffer: PhysicalViewport,
) -> Option<dpi::PhysicalSize<u32>> {
    let size = dpi::PhysicalSize::new(buffer.width.max(1), buffer.height.max(1));
    (previous.replace(size) != size).then_some(size)
}

impl CardSession {
    fn create(
        restore: &BrowserRestoreState,
        visible: bool,
        events: Arc<EventBus>,
        rendering: FallbackSurface,
        servo: &servo::Servo,
    ) -> Result<Self, BrowserError> {
        let rendering_context: Rc<dyn servo::RenderingContext> = rendering.context.clone();

        let animating = Rc::new(Cell::new(false));
        let frame_ready = Rc::new(Cell::new(true));
        let restore_scroll_ready = Rc::new(Cell::new(false));
        let input_sequences = Rc::new(RefCell::new(HashMap::new()));
        let next_input_sequence = Arc::new(AtomicU64::new(1));
        let pending_clipboard = Rc::new(RefCell::new(HashMap::new()));
        let pending_file_pickers = Rc::new(RefCell::new(HashMap::new()));
        let navigation = Rc::new(RefCell::new(NavigationState {
            url: restore.url.clone(),
            history: restore.history.clone(),
            current: restore.history_index,
            preserve_restored_history: true,
        }));
        let request_ids = Rc::new(Cell::new(1_u64));
        // Shared with both delegates so a panic caught at our dispatch boundary
        // can retire this one card instead of the whole engine thread.
        let faulted = Rc::new(Cell::new(false));
        let delegate = Rc::new(BrowserDelegate {
            events: events.clone(),
            animating: animating.clone(),
            frame_ready: frame_ready.clone(),
            restore_scroll_ready: restore_scroll_ready.clone(),
            input_sequences: input_sequences.clone(),
            pending_file_pickers: pending_file_pickers.clone(),
            request_ids: request_ids.clone(),
            ime_control: RefCell::new(None),
            navigation: navigation.clone(),
            faulted: faulted.clone(),
        });
        let clipboard = Rc::new(BrowserClipboardDelegate {
            events,
            pending: pending_clipboard.clone(),
            request_ids,
            faulted: faulted.clone(),
        });
        // A per-webview rendering context earns this card its own painter -
        // its own WebRender instance and GL surface - while the constellation,
        // resource threads and caches behind `servo` are shared. Delegates are
        // per webview too, so every event still lands on this card's bus.
        let webview = servo::WebViewBuilder::new(servo, rendering_context)
            .url(restore.url.clone())
            .hidpi_scale_factor(Scale::new(restore.scale_factor))
            .delegate(delegate)
            .clipboard_delegate(clipboard)
            .build();
        if visible {
            webview.show();
            webview.set_throttled(false);
        } else {
            webview.hide();
            webview.set_throttled(true);
        }
        // The context was just created at this size, so the first
        // `resize_fallback` for an unchanged card sends no resize at all.
        let requested_buffer = Cell::new(rendering.context.size());
        rendering.lease.set_visible(visible)?;
        rendering.lease.flush()?;

        Ok(Self {
            webview,
            rendering,
            requested_buffer,
            faulted,
            animating,
            frame_ready,
            restore_scroll_ready,
            input_sequences,
            next_input_sequence,
            pending_clipboard,
            pending_file_pickers,
            navigation,
        })
    }

    fn paint(&mut self, events: &EventBus) {
        self.webview.paint();
        // The viewport source rectangle may only grow together with the buffer
        // it crops, and the rendering context's present attaches that buffer
        // and commits the child in one step, so latch clip state right here.
        let buffer = self.rendering.context.size();
        if let Err(error) = self
            .rendering
            .lease
            .before_present(PhysicalViewport::new(buffer.width, buffer.height))
        {
            events.semantic(BrowserEvent::Error(error));
        }
        self.rendering.context.present();
        if let Err(error) = self.rendering.lease.flush() {
            events.semantic(BrowserEvent::Error(error));
        }
    }

    fn dispatch_fallback(&self) -> Result<(), BrowserError> {
        self.rendering.lease.dispatch_pending()
    }

    fn set_visible(&self, visible: bool) -> Result<(), BrowserError> {
        if visible {
            self.webview.show();
            self.webview.set_throttled(false);
            self.frame_ready.set(true);
        } else {
            self.webview.hide();
            self.webview.set_throttled(true);
        }
        self.rendering.lease.set_visible(visible)?;
        self.rendering.lease.flush()
    }

    fn resize_fallback(
        &self,
        placement: SurfacePlacement,
        buffer: PhysicalViewport,
    ) -> Result<(), BrowserError> {
        // Servo always owns the full widget rectangle, however much of it is
        // presented, so panning a card past the edge of its clip crops pixels
        // instead of reflowing the page.
        if let Some(size) = resize_request(&self.requested_buffer, buffer) {
            self.webview.resize(size);
        }
        self.rendering.lease.set_geometry(placement)?;
        self.rendering.lease.flush()
    }

    fn shutdown(self) {
        let CardSession {
            webview, rendering, ..
        } = self;
        webview.set_throttled(true);
        webview.hide();
        // Dropping the last handle to this webview closes it in the shared
        // runtime and destroys its painter, which owns its WebRender instance
        // and GL surface. The runtime and every sibling card stay alive.
        drop(webview);
        drop(rendering);
    }

    fn store_navigation(&self, restore: &mut BrowserRestoreState) {
        let navigation = self.navigation.borrow();
        restore.url = navigation.url.clone();
        restore.history = navigation.history.clone();
        restore.history_index = navigation
            .current
            .min(restore.history.len().saturating_sub(1));
    }

    fn set_navigation(&self, restore: &BrowserRestoreState, preserve_restored_history: bool) {
        *self.navigation.borrow_mut() = NavigationState {
            url: restore.url.clone(),
            history: restore.history.clone(),
            current: restore.history_index,
            preserve_restored_history,
        };
    }
}

#[derive(Clone)]
struct BrowserWaker {
    wake_tx: Sender<()>,
    wake_queued: Arc<AtomicBool>,
}

impl servo::EventLoopWaker for BrowserWaker {
    fn clone_box(&self) -> Box<dyn servo::EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        if !self.wake_queued.swap(true, Ordering::AcqRel) && self.wake_tx.try_send(()).is_err() {
            self.wake_queued.store(false, Ordering::Release);
        }
    }
}

struct BrowserDelegate {
    events: Arc<EventBus>,
    animating: Rc<Cell<bool>>,
    frame_ready: Rc<Cell<bool>>,
    restore_scroll_ready: Rc<Cell<bool>>,
    input_sequences: Rc<RefCell<HashMap<servo::InputEventId, u64>>>,
    pending_file_pickers: Rc<RefCell<HashMap<DelegateRequestId, servo::FilePicker>>>,
    request_ids: Rc<Cell<u64>>,
    ime_control: RefCell<Option<(servo::EmbedderControlId, DelegateRequestId)>>,
    navigation: Rc<RefCell<NavigationState>>,
    faulted: Rc<Cell<bool>>,
}

struct NavigationState {
    url: Url,
    history: Vec<Url>,
    current: usize,
    preserve_restored_history: bool,
}

impl BrowserDelegate {
    fn next_request(&self) -> DelegateRequestId {
        let id = self.request_ids.get();
        self.request_ids.set(id.wrapping_add(1));
        DelegateRequestId(id)
    }

    fn guard(&self, callback: impl FnOnce()) {
        guarded_dispatch(&self.events, &self.faulted, callback);
    }
}

/// Runs one delegate callback at the boundary between Servo and our code.
///
/// Servo invokes these callbacks from inside `spin_event_loop`, so a panic in
/// our dispatch would unwind through Servo's internals and take down the single
/// engine thread every card shares. Catching it here contains the blast radius
/// to the card whose callback failed: `Engine::reap` sees the flag, closes that
/// webview, and leaves every sibling running. A panic raised below this
/// boundary, inside Servo, is caught around `spin_event_loop` so it faults the
/// cards that have a session instead of unwinding the thread.
///
/// The catch deliberately wraps a whole callback body, so no `RefCell` borrow is
/// live when it fires - unwinding releases every borrow guard on the way out.
/// The card is retired regardless, because a callback that panicked halfway
/// leaves logical state nobody should have to reason about.
fn guarded_dispatch(events: &EventBus, faulted: &Cell<bool>, callback: impl FnOnce()) {
    // Outside the closure, so it survives the unwind: the panic hook runs
    // before unwinding, and without this scope every recovered delegate fault
    // is recorded as an unrecovered process panic instead of a caught fault.
    let _recovered = crate::instance::recovered_panic_scope();
    let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(callback)) else {
        return;
    };
    faulted.set(true);
    let reason = format!(
        "the browser delegate panicked while dispatching a Servo callback: {}",
        panic_detail(payload.as_ref())
    );
    tracing::error!("{reason}");
    events.semantic(BrowserEvent::Crashed {
        reason,
        backtrace: None,
    });
}

fn panic_detail(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic payload")
}

impl servo::WebViewDelegate for BrowserDelegate {
    fn notify_url_changed(&self, _webview: servo::WebView, url: Url) {
        self.guard(|| {
            self.navigation.borrow_mut().url = url.clone();
            self.events.semantic(BrowserEvent::UrlChanged(url));
        });
    }

    fn notify_page_title_changed(&self, _webview: servo::WebView, title: Option<String>) {
        self.guard(|| {
            self.events.semantic(BrowserEvent::TitleChanged(title));
        });
    }

    fn notify_focus_changed(&self, _webview: servo::WebView, focused: bool) {
        self.guard(|| {
            self.events.semantic(BrowserEvent::FocusChanged(focused));
        });
    }

    fn notify_animating_changed(&self, _webview: servo::WebView, animating: bool) {
        self.guard(|| self.animating.set(animating));
    }

    fn notify_load_status_changed(&self, _webview: servo::WebView, status: servo::LoadStatus) {
        self.guard(|| {
            self.events
                .semantic(BrowserEvent::LoadStateChanged(format!("{status:?}")));
            if status == servo::LoadStatus::Complete {
                self.restore_scroll_ready.set(true);
                self.navigation.borrow_mut().preserve_restored_history = false;
            }
        });
    }

    fn notify_new_frame_ready(&self, _webview: servo::WebView) {
        self.guard(|| self.frame_ready.set(true));
    }

    fn notify_history_changed(&self, _webview: servo::WebView, entries: Vec<Url>, current: usize) {
        self.guard(|| {
            let mut navigation = self.navigation.borrow_mut();
            if navigation.preserve_restored_history && entries.len() <= 1 {
                return;
            }
            navigation.history = entries.clone();
            navigation.current = current.min(entries.len().saturating_sub(1));
            if let Some(url) = entries.get(navigation.current) {
                navigation.url = url.clone();
            }
            drop(navigation);
            self.events
                .semantic(BrowserEvent::HistoryChanged { entries, current });
        });
    }

    fn notify_closed(&self, _webview: servo::WebView) {
        self.guard(|| {
            self.events.semantic(BrowserEvent::ClosedByContent);
        });
    }

    fn notify_input_event_handled(
        &self,
        _webview: servo::WebView,
        id: servo::InputEventId,
        result: servo::InputEventResult,
    ) {
        self.guard(|| {
            if let Some(sequence) = self.input_sequences.borrow_mut().remove(&id) {
                self.events.semantic(BrowserEvent::InputHandled {
                    sequence,
                    consumed: result.intersects(
                        servo::InputEventResult::Consumed
                            | servo::InputEventResult::DefaultPrevented
                            | servo::InputEventResult::DispatchFailed,
                    ),
                });
            }
        });
    }

    fn notify_crashed(&self, _webview: servo::WebView, reason: String, backtrace: Option<String>) {
        self.guard(|| {
            self.events
                .semantic(BrowserEvent::Crashed { reason, backtrace });
        });
    }

    fn request_create_new(
        &self,
        _parent_webview: servo::WebView,
        _request: servo::CreateNewWebViewRequest,
    ) {
        self.guard(|| {
            if !self
                .events
                .semantic(BrowserEvent::Delegate(DelegateRequest::NewWindow {
                    url: None,
                }))
            {
                tracing::warn!(
                    "rejected new-window request because the browser event queue is full"
                );
            }
        });
        // Dropping the request explicitly rejects creation; the application can
        // create a separate BrowserHandle in response to the semantic event.
    }

    fn show_embedder_control(&self, _webview: servo::WebView, control: servo::EmbedderControl) {
        self.guard(|| match control {
            servo::EmbedderControl::InputMethod(control) => {
                let id = self.next_request();
                let rect = control.position();
                *self.ime_control.borrow_mut() = Some((control.id(), id));
                if !self
                    .events
                    .semantic(BrowserEvent::InputMethod(Some(InputMethodState {
                        id,
                        text: control.text(),
                        insertion_point: control.insertion_point(),
                        device_rect: (rect.min.x, rect.min.y, rect.width(), rect.height()),
                        multiline: control.multiline(),
                        allow_virtual_keyboard: control.allow_virtual_keyboard(),
                        secure: control.input_method_type() == servo::InputMethodType::Password,
                    })))
                {
                    self.ime_control.borrow_mut().take();
                }
            }
            servo::EmbedderControl::FilePicker(picker) => {
                let id = self.next_request();
                let request = DelegateRequest::FilePicker {
                    id,
                    multiple: picker.allow_select_multiple(),
                    current_paths: picker.current_paths().to_vec(),
                    filters: picker
                        .filter_patterns()
                        .iter()
                        .map(|pattern| pattern.0.clone())
                        .collect(),
                };
                self.pending_file_pickers.borrow_mut().insert(id, picker);
                if !self.events.semantic(BrowserEvent::Delegate(request))
                    && let Some(picker) = self.pending_file_pickers.borrow_mut().remove(&id)
                {
                    picker.dismiss();
                }
            }
            // Servo owns the response defaults for the remaining controls.
            // Dropping them dismisses without spoofable browser-chrome UI.
            _ => {}
        });
    }

    fn hide_embedder_control(
        &self,
        _webview: servo::WebView,
        control_id: servo::EmbedderControlId,
    ) {
        self.guard(|| {
            if self
                .ime_control
                .borrow()
                .as_ref()
                .is_some_and(|(servo_id, _)| *servo_id == control_id)
            {
                self.ime_control.borrow_mut().take();
                self.events.semantic(BrowserEvent::InputMethod(None));
            }
        });
    }
}

struct BrowserClipboardDelegate {
    events: Arc<EventBus>,
    pending: Rc<RefCell<HashMap<DelegateRequestId, servo::StringRequest>>>,
    request_ids: Rc<Cell<u64>>,
    faulted: Rc<Cell<bool>>,
}

impl BrowserClipboardDelegate {
    fn guard(&self, callback: impl FnOnce()) {
        guarded_dispatch(&self.events, &self.faulted, callback);
    }
}

impl servo::ClipboardDelegate for BrowserClipboardDelegate {
    fn clear(&self, _webview: servo::WebView) {
        self.guard(|| {
            if !self
                .events
                .semantic(BrowserEvent::Delegate(DelegateRequest::ClipboardClear))
            {
                tracing::warn!("rejected clipboard clear because the browser event queue is full");
            }
        });
    }

    fn get_text(&self, _webview: servo::WebView, request: servo::StringRequest) {
        self.guard(|| {
            let id = DelegateRequestId(self.request_ids.get());
            self.request_ids.set(id.0.wrapping_add(1));
            self.pending.borrow_mut().insert(id, request);
            if !self
                .events
                .semantic(BrowserEvent::Delegate(DelegateRequest::ClipboardRead {
                    id,
                }))
                && let Some(request) = self.pending.borrow_mut().remove(&id)
            {
                request.failure("browser event queue is full".into());
            }
        });
    }

    fn set_text(&self, _webview: servo::WebView, text: String) {
        self.guard(|| {
            if !self
                .events
                .semantic(BrowserEvent::Delegate(DelegateRequest::ClipboardWrite {
                    text,
                }))
            {
                tracing::warn!("rejected clipboard write because the browser event queue is full");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restore_state() -> BrowserRestoreState {
        let url = Url::parse("https://example.com").unwrap();
        BrowserRestoreState {
            url: url.clone(),
            history: vec![url],
            history_index: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            scale_factor: 1.0,
            viewport: PhysicalViewport::MIN,
        }
    }

    /// A card with no Servo session, which is the only shape constructible
    /// without a compositor. Visibility bookkeeping never touches the session.
    fn sessionless_card(visible: bool) -> Card {
        // Nothing waits on the paired receiver: this card is never registered
        // on an engine.
        card_with_teardown(visible).0
    }

    /// As [`sessionless_card`], keeping the teardown receiver that
    /// `BrowserHandle::shutdown` blocks on.
    fn card_with_teardown(visible: bool) -> (Card, Receiver<()>) {
        let (teardown, teardown_done) = crossbeam_channel::bounded(0);
        let card = Card {
            mailbox: CommandMailbox::new(),
            events: EventBus::new(),
            restore: restore_state(),
            lifecycle: LifecycleMachine::new(visible),
            visible,
            residency: visible,
            presentation: true,
            focused: false,
            placement: SurfacePlacement::unclipped(PhysicalViewport::MIN, 1.0),
            session: None,
            fallback_factory: None,
            _teardown: teardown,
            closed: false,
        };
        (card, teardown_done)
    }

    #[test]
    fn presentation_and_residency_are_independent_visibility_owners() {
        let mut card = sessionless_card(false);
        assert!(!card.visible);

        // The widget reports that the card is entirely off screen.
        card.apply_coalesced(CoalescedCommands {
            presented: Some(false),
            ..CoalescedCommands::default()
        });
        assert!(!card.visible);

        // The application then classifies the card as visible. With a single
        // shared flag this write won, and the card oscillated every frame.
        card.apply_coalesced(CoalescedCommands {
            residency: Some(true),
            ..CoalescedCommands::default()
        });
        assert!(card.residency);
        assert!(!card.presentation);
        assert!(!card.visible, "geometry must veto the residency class");

        // Only agreement between both owners makes the card visible, and only
        // then does the card ask the engine for a session.
        assert_eq!(
            card.apply_coalesced(CoalescedCommands {
                presented: Some(true),
                ..CoalescedCommands::default()
            }),
            CardRequest::CreateSession
        );
        assert!(card.visible);

        // And the reverse: residency alone vetoes real geometry.
        let mut card = sessionless_card(false);
        assert_eq!(
            card.apply_coalesced(CoalescedCommands {
                presented: Some(true),
                residency: Some(false),
                ..CoalescedCommands::default()
            }),
            CardRequest::None
        );
        assert!(card.presentation);
        assert!(!card.visible);
    }

    #[test]
    fn a_hidden_card_blocks_instead_of_waking_on_a_cadence() {
        assert_eq!(select_timeout(false, false), None);
        // A hidden card cannot animate, but a stale animating flag must not
        // resurrect the timer either.
        assert_eq!(select_timeout(false, true), None);
        assert_eq!(select_timeout(true, true), Some(ANIMATION_CADENCE));
        assert_eq!(select_timeout(true, false), Some(IDLE_CADENCE));

        let mut card = sessionless_card(false);
        assert_eq!(card.select_timeout(), None);
        card.visible = true;
        // No session, so nothing animates: the idle safety net applies.
        assert!(!card.animating());
        assert_eq!(card.select_timeout(), Some(IDLE_CADENCE));
    }

    #[test]
    fn a_pan_updates_the_placement_without_disturbing_the_servo_viewport() {
        let mut card = sessionless_card(true);
        let buffer = PhysicalViewport::new(480, 600);
        let clipped = SurfacePlacement {
            logical_x: 12,
            logical_y: 44,
            logical_width: 400,
            logical_height: 600,
            source_x: 80,
            source_y: 0,
            source_width: 400,
            source_height: 600,
        };
        card.apply_coalesced(CoalescedCommands {
            viewport: Some(buffer),
            placement: Some(clipped),
            ..CoalescedCommands::default()
        });

        // The clip lives entirely in the placement: Servo still renders and
        // lays out the full card, so panning cannot reflow the page.
        assert_eq!(card.restore.viewport, buffer);
        assert_eq!(card.placement, clipped);
        assert!(card.visible);
    }

    #[test]
    fn restore_normalization_keeps_current_url_history_and_scroll() {
        let first = Url::parse("https://example.com/one").unwrap();
        let current = Url::parse("https://example.com/two").unwrap();
        let state = BrowserRestoreState {
            url: Url::parse("https://wrong.invalid").unwrap(),
            history: vec![first.clone(), current.clone()],
            history_index: 1,
            scroll_x: 42.5,
            scroll_y: 99.25,
            scale_factor: 1.25,
            viewport: PhysicalViewport::new(1000, 700),
        }
        .normalized();
        assert_eq!(state.url, current);
        assert_eq!(state.history, vec![first, state.url.clone()]);
        assert_eq!((state.scroll_x, state.scroll_y), (42.5, 99.25));
        assert_eq!(state.scale_factor, 1.25);
        assert_eq!(state.viewport, PhysicalViewport::new(1000, 700));
    }

    #[test]
    fn visible_initialization_failure_closes_only_the_failing_card() {
        let mailbox = CommandMailbox::new();
        let events = EventBus::new();
        let (teardown, teardown_done) = crossbeam_channel::bounded(0);
        let (registrations, engine_registrations) = crossbeam_channel::unbounded();
        registrations
            .send(Registration {
                mailbox: mailbox.clone(),
                events: events.clone(),
                restore: restore_state(),
                visible: true,
                fallback_factory: None,
                teardown,
            })
            .unwrap();
        // Closing the registration channel retires the engine as soon as it
        // holds no cards, which is what lets this run to completion inline.
        drop(registrations);

        Engine::run(0, engine_registrations);

        let queued = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert!(queued.iter().any(|event| matches!(
            event,
            BrowserEvent::Crashed { reason, backtrace: None }
                if reason.contains("Browser initialization failed:")
                    && reason.contains("Wayland child-subsurface factory")
        )));
        assert!(queued.iter().any(|event| matches!(
            event,
            BrowserEvent::LifecycleChanged(BrowserLifecycle::Closed)
        )));
        // The engine released the card, so a handle waiting on this barrier is
        // free to return. No Servo runtime was ever built: the card could not
        // get a Wayland child, so it never reached the shared engine.
        assert!(teardown_done.recv().is_err());
        // A released card refuses further commands instead of filling a queue
        // that nothing will ever read.
        assert!(matches!(
            mailbox.send(Command::Reload),
            Err(BrowserError::ThreadStopped)
        ));
    }

    /// An engine thread that unwinds takes every card with it - one engine for N
    /// cards is the whole point - but it must never present as a hang or as a
    /// card that silently stopped painting. Dropping a populated `Engine` is
    /// exactly what unwinding out of `event_loop` does.
    #[test]
    fn an_unwinding_engine_reports_every_card_it_releases() {
        let (card, teardown_done) = card_with_teardown(true);
        let events = card.events.clone();
        let mailbox = card.mailbox.clone();

        // A card that reached the registration channel but was never accepted
        // has to fail too; it has no `Card` of its own to report through.
        let pending_mailbox = CommandMailbox::new();
        let pending_events = EventBus::new();
        let (pending_teardown, pending_done) = crossbeam_channel::bounded(0);
        let (registrations, engine_registrations) = crossbeam_channel::unbounded();
        registrations
            .send(Registration {
                mailbox: pending_mailbox.clone(),
                events: pending_events.clone(),
                restore: restore_state(),
                visible: true,
                fallback_factory: None,
                teardown: pending_teardown,
            })
            .unwrap();

        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        drop(Engine {
            generation: 0,
            cards: vec![card],
            servo: None,
            import: None,
            registrations: Some(engine_registrations),
            wake_tx,
            wake_rx,
            wake_queued: Arc::new(AtomicBool::new(false)),
        });

        for (bus, barrier) in [(&events, &teardown_done), (&pending_events, &pending_done)] {
            // Nobody blocks forever: the barrier `BrowserHandle::shutdown` waits
            // on is released by the card's drop either way.
            assert!(barrier.recv().is_err());
            // And it is released with an explanation the UI can render.
            let queued = std::iter::from_fn(|| bus.try_recv().ok()).collect::<Vec<_>>();
            assert!(
                queued.iter().any(|event| matches!(
                    event,
                    BrowserEvent::Crashed { reason, .. } if reason == ENGINE_PANIC_REASON
                )),
                "expected a crash report, got {queued:?}"
            );
        }
        for mailbox in [&mailbox, &pending_mailbox] {
            assert!(matches!(
                mailbox.send(Command::Reload),
                Err(BrowserError::ThreadStopped)
            ));
            assert!(matches!(
                mailbox.update_coalesced(|state| state.focused = Some(true)),
                Err(BrowserError::ThreadStopped)
            ));
        }
    }

    /// The delegate boundary is the one panic source we can contain: catching it
    /// keeps a bug in our dispatch from unwinding through Servo and killing the
    /// engine thread that every other card shares.
    #[test]
    fn a_panicking_delegate_callback_faults_only_its_own_card() {
        let events = EventBus::new();
        let faulted = Cell::new(false);

        guarded_dispatch(&events, &faulted, || {
            events.semantic(BrowserEvent::TitleChanged(Some("fine".into())));
        });
        assert!(!faulted.get());

        // A live `RefCell` borrow inside the callback must not survive the
        // panic: the catch sits outside the whole body, so unwinding releases
        // every borrow guard before it fires. The panic message this prints to
        // the test log is deliberate.
        let state = RefCell::new(0_u32);
        guarded_dispatch(&events, &faulted, || {
            *state.borrow_mut() += 1;
            panic!("delegate exploded");
        });

        assert!(faulted.get(), "the engine must be told to close this card");
        assert!(
            state.try_borrow_mut().is_ok(),
            "the borrow outlived the panic"
        );
        let queued = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert!(
            queued.iter().any(|event| matches!(
                event,
                BrowserEvent::Crashed { reason, .. } if reason.contains("delegate exploded")
            )),
            "expected the panic payload in a crash report, got {queued:?}"
        );
    }

    #[test]
    fn a_stable_card_never_asks_servo_to_resize() {
        let requested = Cell::new(dpi::PhysicalSize::new(480, 600));

        // A pan republishes the same buffer size every frame. Servo's resize
        // destroys and recreates a surfman surface, so this must stay silent.
        for _ in 0..120 {
            assert_eq!(
                resize_request(&requested, PhysicalViewport::new(480, 600)),
                None
            );
        }

        assert_eq!(
            resize_request(&requested, PhysicalViewport::new(480, 601)),
            Some(dpi::PhysicalSize::new(480, 601))
        );
        assert_eq!(
            resize_request(&requested, PhysicalViewport::new(480, 601)),
            None
        );
    }

    /// A degenerate size would be silently raised to Servo's own 1x1 minimum,
    /// so an unclamped request could never match what was applied - every
    /// frame would then re-issue the same resize.
    #[test]
    fn a_degenerate_buffer_is_clamped_before_it_reaches_servo() {
        let requested = Cell::new(dpi::PhysicalSize::new(480, 600));
        let degenerate = PhysicalViewport {
            width: 0,
            height: 0,
        };

        assert_eq!(
            resize_request(&requested, degenerate),
            Some(dpi::PhysicalSize::new(1, 1))
        );
        assert_eq!(resize_request(&requested, degenerate), None);
    }

    /// Both registry tests share the process-wide engine slot.
    fn engine_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn engine_generation() -> Option<u64> {
        lock_registry().as_ref().map(|(generation, _)| *generation)
    }

    fn crash_reasons(events: &EventBus) -> Vec<String> {
        std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                BrowserEvent::Crashed { reason, .. } => Some(reason),
                _ => None,
            })
            .collect()
    }

    /// `retire` never runs on the panic path, so the registry keeps pointing at
    /// the engine that unwound. The next card must notice that channel is dead
    /// and start a fresh engine rather than register onto a corpse and hang.
    #[test]
    fn a_card_registered_after_an_engine_panic_starts_a_fresh_engine() {
        let _registry = engine_test_lock();
        // Exactly the state an unwound engine leaves behind: the registry's
        // sender is live, but the receiver went with the thread.
        let (dead_sender, dead_receiver) = crossbeam_channel::unbounded();
        drop(dead_receiver);
        let dead_generation = ENGINE_GENERATIONS.fetch_add(1, Ordering::Relaxed);
        *lock_registry() = Some((dead_generation, dead_sender));

        let events = EventBus::new();
        let (teardown, teardown_done) = crossbeam_channel::bounded(0);
        register(Registration {
            mailbox: CommandMailbox::new(),
            events: events.clone(),
            restore: restore_state(),
            visible: true,
            fallback_factory: None,
            teardown,
        })
        .expect("a dead registry entry must be replaced, not reported as failure");

        // A replacement engine really ran this card: with no Wayland factory it
        // fails initialization and is released, which is what disconnects the
        // barrier. A timeout here would mean the card registered onto the dead
        // channel and nothing ever serviced it.
        assert!(matches!(
            teardown_done.recv_timeout(Duration::from_secs(10)),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        ));
        let queued = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert!(
            queued
                .iter()
                .any(|event| matches!(event, BrowserEvent::Crashed { .. })),
            "expected the card's failure to be reported, got {queued:?}"
        );
        assert!(
            lock_registry()
                .as_ref()
                .is_none_or(|(live, _)| *live != dead_generation),
            "the dead generation must not survive in the registry"
        );
    }

    /// Closing the last card must not retire the runtime. Servo can be
    /// constructed once per process, so a replacement thread's `Servo::new`
    /// panics ("Already initialized") and that unwind is what the next card
    /// renders as [`ENGINE_PANIC_REASON`].
    #[test]
    fn closing_the_last_card_reuses_the_engine_for_the_next_one() {
        let _registry = engine_test_lock();
        let open = |events: Arc<EventBus>| {
            let (teardown, teardown_done) = crossbeam_channel::bounded(0);
            register(Registration {
                mailbox: CommandMailbox::new(),
                events,
                restore: restore_state(),
                visible: true,
                fallback_factory: None,
                teardown,
            })
            .expect("registration reaches a live engine");
            assert!(
                matches!(
                    teardown_done.recv_timeout(Duration::from_secs(10)),
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected)
                ),
                "the card was not released"
            );
        };

        let first = EventBus::new();
        open(first.clone());
        let generation =
            engine_generation().expect("the engine stays registered after the last card");
        let first_reasons = crash_reasons(&first);
        assert!(
            first_reasons
                .iter()
                .all(|reason| reason != ENGINE_PANIC_REASON),
            "closing the last card faulted the engine: {first_reasons:?}"
        );
        assert!(
            first_reasons
                .iter()
                .any(|reason| reason.contains("initialization failed")),
            "the card should fail for lack of a surface, not vanish: {first_reasons:?}"
        );

        let second = EventBus::new();
        open(second.clone());
        assert_eq!(
            engine_generation(),
            Some(generation),
            "the next card must join the parked engine instead of constructing another Servo"
        );
        let second_reasons = crash_reasons(&second);
        assert!(
            second_reasons
                .iter()
                .all(|reason| reason != ENGINE_PANIC_REASON),
            "the card opened after close rendered the engine panic: {second_reasons:?}"
        );
        assert!(
            second_reasons
                .iter()
                .any(|reason| reason.contains("initialization failed")),
            "the second card was not served by the parked engine: {second_reasons:?}"
        );
    }

    #[test]
    fn lazy_history_traversal_has_stable_bounds() {
        let urls = ["one", "two", "three"]
            .map(|path| Url::parse(&format!("https://example.com/{path}")).unwrap());
        let mut state = BrowserRestoreState {
            url: urls[1].clone(),
            history: urls.to_vec(),
            history_index: 1,
            scroll_x: 0.0,
            scroll_y: 0.0,
            scale_factor: 1.0,
            viewport: PhysicalViewport::MIN,
        };
        state.history_index = state.history_index.saturating_add_signed(-1);
        assert_eq!(state.history[state.history_index], urls[0]);
        state.history_index = state.history_index.saturating_add_signed(-1);
        assert_eq!(state.history_index, 0);
        state.history_index = state.history_index.saturating_add_signed(2);
        assert_eq!(state.history[state.history_index], urls[2]);
    }
}
