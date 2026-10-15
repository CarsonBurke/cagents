use std::{
    collections::VecDeque,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use url::Url;

use super::{
    BrowserError, BrowserEvent, BrowserInput, BrowserRestoreState, DelegateRequest,
    DelegateRequestId, PhysicalViewport, SurfacePlacement, fallback::WaylandSubsurfaceFactory,
    runtime,
};

const COMMAND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 128;
const CRITICAL_EVENT_RESERVE: usize = 8;
const ADMITTED_KEY_LIMIT: usize = 512;
static EVENT_SOURCE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Default)]
struct KeyboardInput {
    held: Vec<(cosmic::iced::keyboard::key::Physical, BrowserInput)>,
    released: Vec<cosmic::iced::keyboard::key::Physical>,
    modifiers: super::BrowserModifiers,
}

impl KeyboardInput {
    fn release_modifiers(&mut self, release: &mut BrowserInput) {
        let BrowserInput::Key { code, modifiers, .. } = release else {
            return;
        };
        // The event snapshot at keydown may predate a normally delivered
        // modifier release. Release remaining owned modifiers in order, and
        // keep an aggregate bit until both left and right keys are released.
        let remaining = |left: &str, right: &str| {
            self.held.iter().any(|(_, input)| matches!(input,
                BrowserInput::Key { code, .. } if code == left || code == right))
        };
        match code.as_str() {
            "ControlLeft" | "ControlRight" => {
                self.modifiers.control = remaining("ControlLeft", "ControlRight");
            }
            "ShiftLeft" | "ShiftRight" => {
                self.modifiers.shift = remaining("ShiftLeft", "ShiftRight");
            }
            "AltLeft" | "AltRight" => {
                self.modifiers.alt = remaining("AltLeft", "AltRight");
            }
            "SuperLeft" | "SuperRight" | "MetaLeft" | "MetaRight" => {
                self.modifiers.meta = remaining("MetaLeft", "MetaRight")
                    || remaining("SuperLeft", "SuperRight");
            }
            _ => {}
        }
        *modifiers = self.modifiers;
    }
}

#[derive(Debug)]
pub(crate) enum Command {
    Import(super::ImportRequest),
    Navigate(Url),
    Back,
    Forward,
    Reload,
    Stop,
    Hibernate,
    Restore,
    Input(BrowserInput),
    ClipboardText {
        id: DelegateRequestId,
        result: Result<String, String>,
    },
    FilePickerResult {
        id: DelegateRequestId,
        paths: Option<Vec<PathBuf>>,
    },
    Shutdown,
    WakeCoalesced,
}

#[derive(Debug, Default)]
pub(crate) struct CoalescedCommands {
    /// Full physical size Servo renders this card at, clip or no clip.
    pub viewport: Option<PhysicalViewport>,
    /// Where the child surface sits and which part of that buffer it presents.
    pub placement: Option<SurfacePlacement>,
    pub scale_factor: Option<f32>,
    /// Application-owned hibernate/restore policy; see [`BrowserHandle::set_visible`].
    pub residency: Option<bool>,
    /// Widget-owned real geometry; see [`BrowserHandle::set_presented`].
    pub presented: Option<bool>,
    pub focused: Option<bool>,
    pub pointer_motion: Option<BrowserInput>,
    pub wheel: Option<BrowserInput>,
    pub touch_motion: Option<BrowserInput>,
}

impl CoalescedCommands {
    fn is_empty(&self) -> bool {
        self.viewport.is_none()
            && self.placement.is_none()
            && self.scale_factor.is_none()
            && self.residency.is_none()
            && self.presented.is_none()
            && self.focused.is_none()
            && self.pointer_motion.is_none()
            && self.wheel.is_none()
            && self.touch_motion.is_none()
    }
}

#[derive(Debug)]
pub(crate) struct CommandMailbox {
    tx: Sender<Command>,
    rx: Receiver<Command>,
    coalesced: Mutex<CoalescedCommands>,
    keyboard: Mutex<KeyboardInput>,
    deferred_releases: Mutex<VecDeque<BrowserInput>>,
    wake_admission: Mutex<()>,
    wake_pending: AtomicBool,
    shutdown_requested: AtomicBool,
    /// Set by the engine once this card can never be serviced again: it was
    /// closed, or the engine thread unwound out from under it. Commands then
    /// fail loudly instead of accumulating in a queue that has no reader.
    engine_gone: AtomicBool,
}

impl CommandMailbox {
    pub(crate) fn new() -> Arc<Self> {
        let (tx, rx) = crossbeam_channel::bounded(COMMAND_CAPACITY);
        Arc::new(Self {
            tx,
            rx,
            coalesced: Mutex::new(CoalescedCommands::default()),
            keyboard: Mutex::new(KeyboardInput::default()),
            deferred_releases: Mutex::new(VecDeque::new()),
            wake_admission: Mutex::new(()),
            wake_pending: AtomicBool::new(false),
            shutdown_requested: AtomicBool::new(false),
            engine_gone: AtomicBool::new(false),
        })
    }

    pub(crate) fn receiver(&self) -> &Receiver<Command> {
        &self.rx
    }

    pub(crate) fn send(&self, command: Command) -> Result<(), BrowserError> {
        self.check_live()?;
        // A blur/eviction release is ordered after every previously admitted
        // press, even when the bounded engine queue is full. A later press
        // must never overtake that deferred release.
        if matches!(&command, Command::Input(_)) {
            let mut pending = self
                .deferred_releases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.refill_releases(&mut pending);
            if !pending.is_empty() {
                return Err(BrowserError::CommandQueueFull);
            }
            return self.tx.try_send(command).map_err(map_send_error);
        }
        self.tx.try_send(command).map_err(map_send_error)
    }

    fn refill_releases(&self, pending: &mut VecDeque<BrowserInput>) {
        while let Some(input) = pending.pop_front() {
            match self.tx.try_send(Command::Input(input)) {
                Ok(()) => {}
                Err(TrySendError::Full(Command::Input(input))) => {
                    pending.push_front(input);
                    break;
                }
                Err(_) => {
                    pending.clear();
                    break;
                }
            }
        }
    }

    /// Called after each engine receive, before admitting later UI input.
    pub(crate) fn refill_after_receive(&self) {
        let mut pending = self
            .deferred_releases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.refill_releases(&mut pending);
        drop(pending);
        // A full queue may have refused the blur wake. Every receive creates
        // another opportunity, including when the originating tree is parked.
        let needs_wake = !self.coalesced().is_empty();
        if needs_wake {
            let _ = self.wake_coalesced();
        }
    }

    pub(crate) fn observe_modifiers(&self, modifiers: super::BrowserModifiers) {
        self.keyboard.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .modifiers = modifiers;
    }

    pub(crate) fn press_key(
        &self,
        physical: cosmic::iced::keyboard::key::Physical,
        press: BrowserInput,
        release: BrowserInput,
    ) -> Result<(), BrowserError> {
        let mut keyboard = self
            .keyboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if keyboard.held.len() >= ADMITTED_KEY_LIMIT
            && !keyboard.held.iter().any(|(key, _)| *key == physical)
        {
            return Err(BrowserError::CommandQueueFull);
        }
        let observed = match &press {
            BrowserInput::Key { modifiers, .. } => Some(*modifiers),
            _ => None,
        };
        self.send(Command::Input(press))?;
        if let Some(modifiers) = observed {
            keyboard.modifiers = modifiers;
        }
        keyboard.released.retain(|key| *key != physical);
        if !keyboard.held.iter().any(|(key, _)| *key == physical) {
            keyboard.held.push((physical, release));
        }
        Ok(())
    }

    pub(crate) fn release_key(
        &self,
        physical: cosmic::iced::keyboard::key::Physical,
        release: BrowserInput,
    ) -> Result<bool, BrowserError> {
        let mut keyboard = self
            .keyboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let BrowserInput::Key { modifiers, .. } = &release {
            keyboard.modifiers = *modifiers;
        }
        if let Some(index) = keyboard.released.iter().position(|key| *key == physical) {
            keyboard.released.swap_remove(index);
            return Ok(true);
        }
        let Some(index) = keyboard.held.iter().position(|(key, _)| *key == physical) else {
            return Ok(false);
        };
        self.send(Command::Input(release))?;
        keyboard.held.swap_remove(index);
        Ok(true)
    }

    pub(crate) fn release_pressed_keys(&self) {
        let mut keyboard = self
            .keyboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if keyboard.held.is_empty() {
            return;
        }
        let mut pending = self
            .deferred_releases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while let Some((physical, mut release)) = keyboard.held.pop() {
            keyboard.release_modifiers(&mut release);
            pending.push_back(release);
            if !keyboard.released.contains(&physical) {
                if keyboard.released.len() == ADMITTED_KEY_LIMIT {
                    keyboard.released.remove(0);
                }
                keyboard.released.push(physical);
            }
        }
        self.refill_releases(&mut pending);
    }

    pub(crate) fn update_coalesced(
        &self,
        update: impl FnOnce(&mut CoalescedCommands),
    ) -> Result<(), BrowserError> {
        self.check_live()?;
        update(&mut self.coalesced());
        self.wake_coalesced()
    }

    fn wake_coalesced(&self) -> Result<(), BrowserError> {
        self.admit_wake(|| {})
    }

    fn admit_wake(&self, after_failure: impl FnOnce()) -> Result<(), BrowserError> {
        // The engine's receive/refill cannot pass a failed producer before
        // its pending flag is reset. Otherwise it could drain the full queue
        // while observing true, then park just before the producer clears it.
        let _admission = self
            .wake_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.wake_pending.swap(true, Ordering::AcqRel)
            && let Err(error) = self.tx.try_send(Command::WakeCoalesced)
        {
            after_failure();
            self.wake_pending.store(false, Ordering::Release);
            return Err(map_send_error(error));
        }
        Ok(())
    }

    pub(crate) fn drain_coalesced(&self) -> CoalescedCommands {
        let admission = self
            .wake_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let drained = std::mem::take(&mut *self.coalesced());
        self.wake_pending.store(false, Ordering::Release);
        drop(admission);
        // A producer can race between the take and clearing `wake_pending`. Ensure
        // any values deposited during that window receive another wake packet.
        let has_more = !self.coalesced().is_empty();
        if has_more {
            let _ = self.wake_coalesced();
        }
        drained
    }

    pub(crate) fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        // The flag is authoritative. This packet only shortens the engine's
        // bounded select timeout when there is room in the command queue.
        let _ = self.tx.try_send(Command::Shutdown);
    }

    pub(crate) fn shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }

    /// Marks this card unserviceable, which the engine does after releasing it -
    /// including while unwinding from a panic. Every later call through the
    /// handle then reports [`BrowserError::ThreadStopped`] instead of silently
    /// queueing into a channel nobody will ever read again.
    pub(crate) fn mark_engine_gone(&self) {
        self.engine_gone.store(true, Ordering::Release);
    }

    fn check_live(&self) -> Result<(), BrowserError> {
        if self.engine_gone.load(Ordering::Acquire) {
            return Err(BrowserError::ThreadStopped);
        }
        Ok(())
    }

    /// Locks the coalescing slot, ignoring poisoning.
    ///
    /// The slot is a set of `Option`s that no panic can leave half-updated, and
    /// one shared engine thread now services every card: treating a poisoned
    /// lock as fatal would let one panicking producer escalate into a panic on
    /// the engine thread, which would take every other card down with it.
    fn coalesced(&self) -> std::sync::MutexGuard<'_, CoalescedCommands> {
        self.coalesced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn map_send_error<T>(error: TrySendError<T>) -> BrowserError {
    match error {
        TrySendError::Full(_) => BrowserError::CommandQueueFull,
        TrySendError::Disconnected(_) => BrowserError::ThreadStopped,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    Url,
    History,
    Title,
    Load,
    Focus,
    InputMethod,
    Error,
    ClipboardMutation,
    Ready,
    Crashed,
    Closed,
    Lifecycle,
}

fn supersedable_kind(event: &BrowserEvent) -> Option<EventKind> {
    match event {
        BrowserEvent::UrlChanged(_) => Some(EventKind::Url),
        BrowserEvent::HistoryChanged { .. } => Some(EventKind::History),
        BrowserEvent::TitleChanged(_) => Some(EventKind::Title),
        BrowserEvent::LoadStateChanged(_) => Some(EventKind::Load),
        BrowserEvent::FocusChanged(_) => Some(EventKind::Focus),
        BrowserEvent::InputMethod(_) => Some(EventKind::InputMethod),
        BrowserEvent::Error(_) => Some(EventKind::Error),
        BrowserEvent::Delegate(
            DelegateRequest::ClipboardClear | DelegateRequest::ClipboardWrite { .. },
        ) => Some(EventKind::ClipboardMutation),
        BrowserEvent::Ready { .. } => Some(EventKind::Ready),
        BrowserEvent::Crashed { .. } => Some(EventKind::Crashed),
        BrowserEvent::ClosedByContent => Some(EventKind::Closed),
        BrowserEvent::LifecycleChanged(_) => Some(EventKind::Lifecycle),
        _ => None,
    }
}

fn is_critical(event: &BrowserEvent) -> bool {
    matches!(
        event,
        BrowserEvent::Ready { .. }
            | BrowserEvent::Crashed { .. }
            | BrowserEvent::ClosedByContent
            | BrowserEvent::LifecycleChanged(_)
    )
}

#[derive(Debug)]
pub(crate) struct EventBus {
    id: u64,
    queue: Mutex<VecDeque<BrowserEvent>>,
    wake_tx: async_channel::Sender<()>,
    wake_rx: async_channel::Receiver<()>,
}

impl EventBus {
    pub(crate) fn new() -> Arc<Self> {
        let (wake_tx, wake_rx) = async_channel::bounded(1);
        Arc::new(Self {
            id: EVENT_SOURCE_ID.fetch_add(1, Ordering::Relaxed),
            queue: Mutex::new(VecDeque::with_capacity(EVENT_CAPACITY)),
            wake_tx,
            wake_rx,
        })
    }

    /// Publishes without blocking Servo. Replaceable state is coalesced by
    /// semantic kind. Lifecycle edges have dedicated reserved capacity, while
    /// callers of rejectable delegate requests can use the return value to
    /// complete the Servo request immediately when regular capacity is full.
    pub(crate) fn semantic(&self, event: BrowserEvent) -> bool {
        let supersedable = supersedable_kind(&event);
        let critical = is_critical(&event);
        let mut queue = self.queue();

        if let Some(kind) = supersedable
            && let Some(index) = queue
                .iter()
                .position(|queued| supersedable_kind(queued) == Some(kind))
        {
            queue[index] = event;
            drop(queue);
            let _ = self.wake_tx.try_send(());
            return true;
        }

        let regular_count = queue.iter().filter(|queued| !is_critical(queued)).count();
        let has_capacity = if critical {
            queue.len() < EVENT_CAPACITY
        } else {
            regular_count < EVENT_CAPACITY - CRITICAL_EVENT_RESERVE && queue.len() < EVENT_CAPACITY
        };
        if !has_capacity {
            if let Some(index) = queue
                .iter()
                .position(|queued| !is_critical(queued) && supersedable_kind(queued).is_some())
            {
                queue.remove(index);
            } else {
                return false;
            }
        }

        queue.push_back(event);
        drop(queue);
        // The one-place notifier is an edge, not an event store. Its bounded
        // fullness coalesces any number of queue publications into one wake.
        let _ = self.wake_tx.try_send(());
        true
    }

    pub(crate) fn try_recv(&self) -> Result<BrowserEvent, async_channel::TryRecvError> {
        self.queue()
            .pop_front()
            .ok_or(async_channel::TryRecvError::Empty)
    }

    async fn recv(&self) -> Option<BrowserEvent> {
        loop {
            let event = self.queue().pop_front();
            if event.is_some() {
                return event;
            }
            self.wake_rx.recv().await.ok()?;
        }
    }

    /// Locks the event queue, ignoring poisoning.
    ///
    /// This bus is the *only* channel through which a card reports a fatal
    /// error, and the engine publishes on it while unwinding from a panic. A
    /// poisoned lock therefore must not be fatal twice over: panicking here
    /// would replace the crash report with a second panic (and, inside a drop
    /// during unwind, an abort). A `VecDeque` of events cannot be left
    /// half-updated by a panic, so recovering the guard is sound.
    fn queue(&self) -> std::sync::MutexGuard<'_, VecDeque<BrowserEvent>> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    fn queued_len(&self) -> usize {
        self.queue().len()
    }
}

#[derive(Clone)]
struct SubscriptionSource(Arc<EventBus>);

impl PartialEq for SubscriptionSource {
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}

impl Eq for SubscriptionSource {}

impl std::hash::Hash for SubscriptionSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.0.id, state);
    }
}

fn event_stream(
    source: &SubscriptionSource,
) -> Pin<Box<dyn cosmic::iced::futures::Stream<Item = BrowserEvent> + Send + 'static>> {
    Box::pin(cosmic::iced::futures::stream::unfold(
        source.0.clone(),
        |events| async move { events.recv().await.map(|event| (event, events)) },
    ))
}

pub struct BrowserHandle {
    pub(crate) mailbox: Arc<CommandMailbox>,
    events: Arc<EventBus>,
    /// Teardown barrier. Nothing is ever sent on it; the engine thread holds
    /// the paired sender for as long as this card owns any Servo or Wayland
    /// resource, so a `recv` here returns exactly when they are all destroyed.
    teardown: Option<Receiver<()>>,
}

impl std::fmt::Debug for BrowserHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserHandle")
            .finish_non_exhaustive()
    }
}

impl BrowserHandle {
    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        Self {
            mailbox: CommandMailbox::new(),
            events: EventBus::new(),
            teardown: None,
        }
    }

    pub fn import_cookies(&self, request: super::ImportRequest) -> Result<(), BrowserError> {
        self.mailbox.send(Command::Import(request))
    }
    /// Registers a browser card on the shared Servo engine thread, starting
    /// that thread if it is not already running.
    pub fn spawn(
        restore: BrowserRestoreState,
        visible: bool,
        fallback: Option<Arc<dyn WaylandSubsurfaceFactory>>,
    ) -> Result<Self, BrowserError> {
        match fallback.as_ref().map(|factory| factory.availability()) {
            Some(super::CapabilityProbe::Available) => {}
            Some(super::CapabilityProbe::Unavailable(reason)) => {
                return Err(BrowserError::Capability(reason));
            }
            _ => {
                return Err(BrowserError::Capability(
                    crate::capability::BrowserUnavailable::ParentUnavailable,
                ));
            }
        }
        let mailbox = CommandMailbox::new();
        let events = EventBus::new();
        let (teardown, teardown_done) = crossbeam_channel::bounded(0);
        runtime::register(runtime::Registration {
            mailbox: mailbox.clone(),
            events: events.clone(),
            restore,
            visible,
            fallback_factory: fallback,
            teardown,
        })?;
        Ok(Self {
            mailbox,
            events,
            teardown: Some(teardown_done),
        })
    }

    pub fn navigate(&self, url: Url) -> Result<(), BrowserError> {
        self.mailbox.send(Command::Navigate(url))
    }

    pub fn back(&self) -> Result<(), BrowserError> {
        self.mailbox.send(Command::Back)
    }

    pub fn forward(&self) -> Result<(), BrowserError> {
        self.mailbox.send(Command::Forward)
    }

    pub fn reload(&self) -> Result<(), BrowserError> {
        self.mailbox.send(Command::Reload)
    }

    pub fn stop(&self) -> Result<(), BrowserError> {
        self.mailbox.send(Command::Stop)
    }

    /// Sets the full physical size Servo renders at, independently of how much
    /// of it is presented.
    pub fn resize(&self, viewport: PhysicalViewport) -> Result<(), BrowserError> {
        self.mailbox
            .update_coalesced(|state| state.viewport = Some(viewport))
    }

    /// Positions the native child surface and clips the region of `buffer` it
    /// presents. Servo keeps rendering the whole `buffer`, so panning a card
    /// past the edge of its clip crops the presented pixels without reflowing
    /// the page.
    pub fn place(
        &self,
        buffer: PhysicalViewport,
        placement: SurfacePlacement,
    ) -> Result<(), BrowserError> {
        self.mailbox.update_coalesced(|state| {
            state.viewport = Some(buffer);
            state.placement = Some(placement);
        })
    }

    pub fn set_scale_factor(&self, scale_factor: f32) -> Result<(), BrowserError> {
        self.mailbox
            .update_coalesced(|state| state.scale_factor = Some(scale_factor))
    }

    /// Application-layer residency: whether this card is allowed to hold live
    /// Servo and Wayland resources at all.
    ///
    /// This is one of two independent inputs to the worker's effective
    /// visibility, and it is owned exclusively by the application's
    /// hibernate/restore policy. The other is
    /// [`set_presented`](Self::set_presented), owned by the widget. The worker
    /// runs Servo only while both are true, so neither owner can clobber the
    /// other's decision by writing last in a frame.
    pub fn set_visible(&self, visible: bool) -> Result<(), BrowserError> {
        self.mailbox
            .update_coalesced(|state| state.residency = Some(visible))
    }

    /// Widget-layer presentation: whether any part of this card's rectangle
    /// survives iced's clip.
    ///
    /// Owned exclusively by [`BrowserWidget`](super::BrowserWidget), which
    /// derives it from real geometry. Partial clipping is expressed through
    /// [`place`](Self::place)'s source rectangle instead, so this only goes
    /// false when the card is entirely off screen or an application overlay
    /// must cover its native child surface.
    pub fn set_presented(&self, presented: bool) -> Result<(), BrowserError> {
        self.mailbox
            .update_coalesced(|state| state.presented = Some(presented))
    }

    pub fn set_focused(&self, focused: bool) -> Result<(), BrowserError> {
        if !focused {
            self.mailbox.release_pressed_keys();
        }
        self.mailbox
            .update_coalesced(|state| state.focused = Some(focused))
    }

    pub fn input(&self, input: BrowserInput) -> Result<(), BrowserError> {
        // Coalesce in one unit so switching between a mouse and touchpad
        // cannot discard the previous device's still-pending movement.
        let input = super::input::normalize_wheel(input);
        match input {
            BrowserInput::MouseMove { .. } => self
                .mailbox
                .update_coalesced(|state| state.pointer_motion = Some(input)),
            BrowserInput::Wheel { .. } => self.mailbox.update_coalesced(|state| {
                if let (
                    Some(BrowserInput::Wheel { x, y, unit, point }),
                    BrowserInput::Wheel {
                        x: nx,
                        y: ny,
                        unit: nunit,
                        point: npoint,
                    },
                ) = (&mut state.wheel, &input)
                    && *unit == *nunit
                {
                    *x += *nx;
                    *y += *ny;
                    *point = *npoint;
                } else {
                    state.wheel = Some(input);
                }
            }),
            BrowserInput::Touch {
                state: super::TouchState::Move,
                ..
            } => self
                .mailbox
                .update_coalesced(|commands| commands.touch_motion = Some(input)),
            _ => self.mailbox.send(Command::Input(input)),
        }
    }

    pub fn hibernate(&self) -> Result<(), BrowserError> {
        self.mailbox.send(Command::Hibernate)
    }

    pub fn restore(&self) -> Result<(), BrowserError> {
        self.mailbox.send(Command::Restore)
    }

    pub fn respond_clipboard(
        &self,
        id: DelegateRequestId,
        result: Result<String, String>,
    ) -> Result<(), BrowserError> {
        self.mailbox.send(Command::ClipboardText { id, result })
    }

    pub fn respond_file_picker(
        &self,
        id: DelegateRequestId,
        paths: Option<Vec<PathBuf>>,
    ) -> Result<(), BrowserError> {
        self.mailbox.send(Command::FilePickerResult { id, paths })
    }

    pub fn drain_events(&self) -> Vec<BrowserEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }

    /// Event-driven integration with iced. Prefer this over polling
    /// [`drain_events`](Self::drain_events); the bounded channel wakes iced only
    /// when Servo produces a semantic event.
    pub fn subscription(&self) -> cosmic::iced::Subscription<BrowserEvent> {
        cosmic::iced::Subscription::run_with(SubscriptionSource(self.events.clone()), event_stream)
    }

    /// Requests shutdown without waiting for card teardown.
    pub fn request_shutdown(&self) {
        self.mailbox.request_shutdown();
    }

    /// Requests card shutdown and waits until every Servo and Wayland resource
    /// owned by this card has been destroyed on the engine thread.
    ///
    /// Repeated calls are harmless. The engine drops this card's teardown
    /// sender only after its webview, rendering context and Wayland lease are
    /// gone, so the disconnect is the same barrier joining the card's own
    /// thread used to be. A defensive engine-thread check replaces the old
    /// refusal to join our own thread.
    ///
    /// The disconnect says nothing about *why* the card is gone - a panicking
    /// engine reports itself on the event bus as [`BrowserEvent::Crashed`],
    /// and every later command through this handle fails with
    /// [`BrowserError::ThreadStopped`].
    pub fn shutdown(&mut self) {
        self.request_shutdown();
        let Some(teardown) = self.teardown.take() else {
            return;
        };
        if runtime::on_engine_thread() {
            return;
        }
        // Disconnection is the ordinary end of this wait: the engine drops
        // the sender only after this card's browser resources are gone.
        let _ = teardown.recv();
    }
}

impl Drop for BrowserHandle {
    fn drop(&mut self) {
        // Dropping a live handle is a safety boundary: the worker may hold
        // copied raw Wayland handles, so detaching it would let those handles
        // outlive their parent window.
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_wake_reset_is_serialized_with_engine_receive_refill() {
        let mailbox = CommandMailbox::new();
        for _ in 0..COMMAND_CAPACITY {
            mailbox.send(Command::Reload).unwrap();
        }
        mailbox.coalesced().focused = Some(false);
        let (failed_tx, failed_rx) = crossbeam_channel::bounded(1);
        let (resume_tx, resume_rx) = crossbeam_channel::bounded(1);
        let (received_tx, received_rx) = crossbeam_channel::bounded(1);
        std::thread::scope(|scope| {
            let producer = scope.spawn(|| {
                mailbox.admit_wake(|| {
                    failed_tx.send(()).unwrap();
                    resume_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                })
            });
            failed_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert!(mailbox.wake_pending.load(Ordering::Acquire));
            assert!(
                matches!(
                    mailbox.wake_admission.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ),
                "failed admission must retain serialization until reset"
            );
            let engine = scope.spawn(|| {
                let mut observed = Vec::new();
                let mut first = true;
                while let Ok(command) = mailbox.receiver().try_recv() {
                    if first {
                        received_tx.send(()).unwrap();
                        first = false;
                    }
                    mailbox.refill_after_receive();
                    if matches!(command, Command::WakeCoalesced) {
                        observed.push(mailbox.drain_coalesced().focused);
                    }
                }
                observed
            });
            received_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert!(matches!(
                mailbox.wake_admission.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            resume_tx.send(()).unwrap();
            assert!(matches!(
                producer.join().unwrap(),
                Err(BrowserError::CommandQueueFull)
            ));
            assert_eq!(engine.join().unwrap(), vec![Some(false)]);
        });
        assert!(!mailbox.wake_pending.load(Ordering::Acquire));
    }

    #[test]
    fn engine_receive_retries_a_blur_wake_refused_by_a_full_queue() {
        let mailbox = CommandMailbox::new();
        for _ in 0..COMMAND_CAPACITY {
            mailbox.send(Command::Reload).unwrap();
        }
        assert!(matches!(
            mailbox.update_coalesced(|state| state.focused = Some(false)),
            Err(BrowserError::CommandQueueFull)
        ));
        let mut observed = Vec::new();
        while let Ok(command) = mailbox.receiver().try_recv() {
            mailbox.refill_after_receive();
            if matches!(command, Command::WakeCoalesced) {
                observed.push(mailbox.drain_coalesced().focused);
            }
        }
        assert_eq!(observed, vec![Some(false)]);
        assert!(!mailbox.wake_pending.load(Ordering::Acquire));
    }

    #[test]
    fn blur_releases_stay_after_admitted_presses_under_backpressure() {
        use super::super::{ButtonState, input::iced_key};
        use cosmic::iced::keyboard::{
            self,
            key::{Code, Physical},
        };
        let mailbox = CommandMailbox::new();
        let physical = Physical::Code(Code::KeyA);
        let key = keyboard::Key::Character("a".into());
        let input = |state, repeat| {
            iced_key(
                state,
                &key,
                physical,
                keyboard::Location::Standard,
                keyboard::Modifiers::empty(),
                repeat,
                false,
            )
        };
        mailbox
            .press_key(
                physical,
                input(ButtonState::Pressed, false),
                input(ButtonState::Released, false),
            )
            .unwrap();
        mailbox
            .press_key(
                physical,
                input(ButtonState::Pressed, true),
                input(ButtonState::Released, false),
            )
            .unwrap();
        for _ in 2..COMMAND_CAPACITY {
            mailbox.send(Command::Reload).unwrap();
        }
        mailbox.release_pressed_keys();
        let other = Physical::Code(Code::KeyB);
        assert!(matches!(
            mailbox.press_key(
                other,
                input(ButtonState::Pressed, false),
                input(ButtonState::Released, false)
            ),
            Err(BrowserError::CommandQueueFull)
        ));
        assert!(
            !mailbox
                .release_key(other, input(ButtonState::Released, false))
                .unwrap()
        );
        let mut observed = Vec::new();
        while let Ok(command) = mailbox.receiver().try_recv() {
            mailbox.refill_after_receive();
            if let Command::Input(input) = command {
                observed.push(input);
            }
        }
        assert_eq!(
            observed,
            vec![
                input(ButtonState::Pressed, false),
                input(ButtonState::Pressed, true),
                input(ButtonState::Released, false)
            ]
        );
        assert!(
            mailbox
                .release_key(physical, input(ButtonState::Released, false))
                .unwrap()
        );
        assert!(
            !mailbox
                .release_key(physical, input(ButtonState::Released, false))
                .unwrap()
        );
        assert!(
            mailbox.receiver().try_recv().is_err(),
            "physical release was already paired"
        );
    }

    #[test]
    fn blur_does_not_restore_a_modifier_released_after_keydown() {
        use super::super::{BrowserKeyLocation, BrowserModifiers, ButtonState};
        use cosmic::iced::keyboard::key::{Code, Physical};
        let mailbox = CommandMailbox::new();
        let input = |state, code: &str, control| BrowserInput::Key {
            state,
            key: if code == "KeyA" { "a" } else { "Control" }.into(),
            code: code.into(),
            location: BrowserKeyLocation::Standard,
            modifiers: BrowserModifiers { control, ..Default::default() },
            repeat: false,
            composing: false,
        };
        for (physical, code) in [(Physical::Code(Code::ControlLeft), "ControlLeft"),
            (Physical::Code(Code::KeyA), "KeyA")] {
            mailbox.press_key(physical, input(ButtonState::Pressed, code, true),
                input(ButtonState::Released, code, true)).unwrap();
        }
        assert!(mailbox.release_key(Physical::Code(Code::ControlLeft),
            input(ButtonState::Released, "ControlLeft", false)).unwrap());
        mailbox.release_pressed_keys();
        let observed: Vec<_> = mailbox.receiver().try_iter().filter_map(|command| {
            if let Command::Input(input) = command { Some(input) } else { None }
        }).collect();
        assert_eq!(observed.last(), Some(&input(ButtonState::Released, "KeyA", false)));
    }

    #[test]
    fn blur_uses_modifier_changes_without_an_owned_modifier_press() {
        use super::super::{BrowserKeyLocation, BrowserModifiers, ButtonState};
        use cosmic::iced::keyboard::key::{Code, Physical};
        for native_release in [false, true] {
            let mailbox = CommandMailbox::new();
            let mut input = BrowserInput::Key {
                state: ButtonState::Pressed, key: "a".into(), code: "KeyA".into(),
                location: BrowserKeyLocation::Standard,
                modifiers: BrowserModifiers { control: true, ..Default::default() },
                repeat: false, composing: false,
            };
            let mut release = input.clone();
            if let BrowserInput::Key { state, .. } = &mut release {
                *state = ButtonState::Released;
            }
            mailbox.press_key(Physical::Code(Code::KeyA), input.clone(), release).unwrap();
            if native_release {
                assert!(!mailbox.release_key(Physical::Code(Code::ControlLeft),
                    BrowserInput::Key {
                        state: ButtonState::Released, key: "Control".into(),
                        code: "ControlLeft".into(), location: BrowserKeyLocation::Left,
                        modifiers: BrowserModifiers::default(), repeat: false, composing: false,
                    }).unwrap());
            } else {
                mailbox.observe_modifiers(BrowserModifiers::default());
            }
            mailbox.release_pressed_keys();
            if let BrowserInput::Key { state, modifiers, .. } = &mut input {
                *state = ButtonState::Released;
                *modifiers = BrowserModifiers::default();
            }
            let observed: Vec<_> = mailbox.receiver().try_iter().filter_map(|command| {
                if let Command::Input(input) = command { Some(input) } else { None }
            }).collect();
            assert_eq!(observed.last(), Some(&input));
        }
    }

    #[test]
    fn blur_releases_dual_modifiers_with_current_aggregate_state() {
        use super::super::{BrowserKeyLocation, BrowserModifiers, ButtonState};
        use cosmic::iced::keyboard::key::{Code, Physical};
        for (left, right, name, kind) in [
            (Code::ControlLeft, Code::ControlRight, "Control", 0),
            (Code::ShiftLeft, Code::ShiftRight, "Shift", 1),
            (Code::AltLeft, Code::AltRight, "Alt", 2),
            (Code::SuperLeft, Code::SuperRight, "Super", 3),
        ] {
            for reverse in [false, true] {
                let mailbox = CommandMailbox::new();
                let expected_modifiers = BrowserModifiers {
                    control: kind == 0, shift: kind == 1,
                    alt: kind == 2, meta: kind == 3, ..Default::default()
                };
                let code_name = |code| match code {
                    Code::SuperLeft => "MetaLeft".into(),
                    Code::SuperRight => "MetaRight".into(),
                    _ => format!("{code:?}"),
                };
                let input = |state, code, location| BrowserInput::Key {
                    state, key: name.into(), code: code_name(code), location,
                    modifiers: expected_modifiers,
                    repeat: false, composing: false,
                };
                let mut keys = [(left, BrowserKeyLocation::Left),
                    (right, BrowserKeyLocation::Right)];
                if reverse { keys.reverse(); }
                for (code, location) in keys {
                    mailbox.press_key(Physical::Code(code),
                        input(ButtonState::Pressed, code, location),
                        input(ButtonState::Released, code, location)).unwrap();
                }
                mailbox.release_pressed_keys();
                let releases: Vec<_> = mailbox.receiver().try_iter().filter_map(|command| {
                    if let Command::Input(BrowserInput::Key {
                        state: ButtonState::Released, code, location, modifiers, ..
                    }) = command { Some((code, location, modifiers)) } else { None }
                }).collect();
                assert_eq!(releases.len(), 2);
                assert_eq!(releases[0].0, code_name(keys[1].0));
                assert_eq!(releases[0].1, keys[1].1);
                assert_eq!(releases[1].0, code_name(keys[0].0));
                assert_eq!(releases[1].1, keys[0].1);
                assert_eq!(releases[0].2, expected_modifiers);
                assert_eq!(releases[1].2, BrowserModifiers::default());
            }
        }
    }

    #[test]
    fn resize_and_pointer_storms_are_coalesced() {
        let mailbox = CommandMailbox::new();
        for n in 1..=1000 {
            mailbox
                .update_coalesced(|commands| {
                    commands.viewport = Some(PhysicalViewport::new(n, n + 1));
                    commands.pointer_motion = Some(BrowserInput::MouseMove {
                        point: super::super::DevicePoint {
                            x: f64::from(n),
                            y: f64::from(n + 1),
                        },
                    });
                })
                .unwrap();
        }
        assert!(matches!(
            mailbox.receiver().try_recv(),
            Ok(Command::WakeCoalesced)
        ));
        assert!(mailbox.receiver().try_recv().is_err());
        let commands = mailbox.drain_coalesced();
        assert_eq!(commands.viewport, Some(PhysicalViewport::new(1000, 1001)));
        assert!(matches!(
            commands.pointer_motion,
            Some(BrowserInput::MouseMove { point }) if point.x == 1000.0 && point.y == 1001.0
        ));
    }

    #[test]
    fn switching_wheel_units_preserves_all_pending_distance_and_latest_hit_point() {
        let handle = BrowserHandle::fixture();
        for (y, unit, x) in [
            (-1.0, super::super::WheelUnit::Lines, 4.0),
            (-12.5, super::super::WheelUnit::Pixels, 8.0),
            (0.5, super::super::WheelUnit::Lines, 10.0),
        ] {
            handle
                .input(BrowserInput::Wheel {
                    x: 0.0,
                    y,
                    unit,
                    point: super::super::DevicePoint { x, y: 20.0 },
                })
                .unwrap();
        }
        assert!(matches!(
            handle.mailbox.receiver().try_recv(),
            Ok(Command::WakeCoalesced)
        ));
        assert!(handle.mailbox.receiver().try_recv().is_err());
        assert_eq!(
            handle.mailbox.drain_coalesced().wheel,
            Some(BrowserInput::Wheel {
                x: 0.0,
                y: -50.5,
                unit: super::super::WheelUnit::Pixels,
                point: super::super::DevicePoint { x: 10.0, y: 20.0 },
            })
        );
    }

    #[test]
    fn wheel_deltas_accumulate_without_unbounding_queue() {
        let mailbox = CommandMailbox::new();
        for _ in 0..40 {
            let input = BrowserInput::Wheel {
                x: 1.0,
                y: -2.0,
                unit: super::super::WheelUnit::Pixels,
                point: super::super::DevicePoint { x: 4.0, y: 8.0 },
            };
            mailbox
                .update_coalesced(|commands| {
                    if let Some(BrowserInput::Wheel { x, y, .. }) = &mut commands.wheel {
                        *x += 1.0;
                        *y -= 2.0;
                    } else {
                        commands.wheel = Some(input);
                    }
                })
                .unwrap();
        }
        let commands = mailbox.drain_coalesced();
        assert!(matches!(
            commands.wheel,
            Some(BrowserInput::Wheel {
                x: 40.0,
                y: -80.0,
                ..
            })
        ));
    }

    #[test]
    fn shutdown_flag_does_not_depend_on_queue_capacity() {
        let mailbox = CommandMailbox::new();
        for _ in 0..COMMAND_CAPACITY {
            mailbox.send(Command::Reload).unwrap();
        }
        mailbox.request_shutdown();
        assert!(mailbox.shutdown_requested());
    }

    #[test]
    fn explicit_shutdown_waits_for_teardown_once_and_is_idempotent() {
        let mailbox = CommandMailbox::new();
        let engine_mailbox = mailbox.clone();
        let (teardown, teardown_done) = crossbeam_channel::bounded::<()>(0);
        let released = Arc::new(AtomicBool::new(false));
        let engine_released = released.clone();
        std::thread::spawn(move || {
            while !engine_mailbox.shutdown_requested() {
                std::thread::yield_now();
            }
            engine_released.store(true, Ordering::Release);
            // Dropping the sender is the card's teardown barrier: the engine
            // does it only after the card's resources are destroyed.
            drop(teardown);
        });
        let mut handle = BrowserHandle {
            mailbox,
            events: EventBus::new(),
            teardown: Some(teardown_done),
        };

        handle.shutdown();
        assert!(released.load(Ordering::Acquire));
        assert!(handle.teardown.is_none());

        handle.shutdown();
        assert!(handle.teardown.is_none());
    }

    #[test]
    fn state_telemetry_is_replaced_in_place() {
        let events = EventBus::new();
        for index in 0..EVENT_CAPACITY {
            assert!(events.semantic(BrowserEvent::TitleChanged(Some(index.to_string()))));
        }
        assert!(events.wake_rx.try_recv().is_ok());
        assert!(events.wake_rx.try_recv().is_err());

        assert_eq!(events.queued_len(), 1);
        assert!(matches!(
            events.try_recv(),
            Ok(BrowserEvent::TitleChanged(Some(title))) if title == (EVENT_CAPACITY - 1).to_string()
        ));
    }

    #[test]
    fn pressure_evicts_only_telemetry_and_preserves_control_edges() {
        let events = EventBus::new();
        for sequence in 0..EVENT_CAPACITY - CRITICAL_EVENT_RESERVE - 2 {
            assert!(events.semantic(BrowserEvent::InputHandled {
                sequence: sequence as u64,
                consumed: false,
            }));
        }
        assert!(events.semantic(BrowserEvent::LoadStateChanged("Loading".into())));
        assert!(events.semantic(BrowserEvent::Delegate(
            super::super::DelegateRequest::ClipboardRead {
                id: DelegateRequestId(7),
            },
        )));
        assert!(events.semantic(BrowserEvent::Ready {
            backend: super::super::BackendSelection {
                selected: super::super::BrowserBackend::WaylandSubsurface,
            },
        }));
        assert!(events.semantic(BrowserEvent::Crashed {
            reason: "fatal".into(),
            backtrace: None,
        }));
        assert!(events.semantic(BrowserEvent::ClosedByContent));
        for lifecycle in [
            super::super::BrowserLifecycle::Visible,
            super::super::BrowserLifecycle::OffscreenHot,
            super::super::BrowserLifecycle::Hibernating,
            super::super::BrowserLifecycle::Hibernated,
            super::super::BrowserLifecycle::Closed,
        ] {
            assert!(events.semantic(BrowserEvent::LifecycleChanged(lifecycle)));
        }

        assert_eq!(
            events.queued_len(),
            EVENT_CAPACITY - CRITICAL_EVENT_RESERVE + 4
        );
        let queued = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert!(queued.iter().any(|event| matches!(
            event,
            BrowserEvent::Delegate(super::super::DelegateRequest::ClipboardRead {
                id: DelegateRequestId(7),
            })
        )));
        assert!(
            queued
                .iter()
                .any(|event| matches!(event, BrowserEvent::Ready { .. }))
        );
        assert!(
            queued
                .iter()
                .any(|event| matches!(event, BrowserEvent::Crashed { .. }))
        );
        assert!(
            queued
                .iter()
                .any(|event| matches!(event, BrowserEvent::ClosedByContent))
        );
        assert!(queued.iter().any(|event| matches!(
            event,
            BrowserEvent::LifecycleChanged(super::super::BrowserLifecycle::Closed)
        )));
        assert_eq!(
            queued
                .iter()
                .filter(|event| matches!(event, BrowserEvent::LifecycleChanged(_)))
                .count(),
            1
        );
    }

    #[test]
    fn delegate_rejection_keeps_the_queue_bounded() {
        let events = EventBus::new();
        for sequence in 0..EVENT_CAPACITY - CRITICAL_EVENT_RESERVE {
            assert!(events.semantic(BrowserEvent::Delegate(
                super::super::DelegateRequest::ClipboardRead {
                    id: DelegateRequestId(sequence as u64),
                },
            )));
        }
        assert!(events.semantic(BrowserEvent::Ready {
            backend: super::super::BackendSelection {
                selected: super::super::BrowserBackend::WaylandSubsurface,
            },
        }));
        assert!(events.semantic(BrowserEvent::Crashed {
            reason: "fatal".into(),
            backtrace: None,
        }));
        assert!(events.semantic(BrowserEvent::ClosedByContent));
        assert!(events.semantic(BrowserEvent::LifecycleChanged(
            super::super::BrowserLifecycle::Closed,
        )));

        assert!(!events.semantic(BrowserEvent::Delegate(
            super::super::DelegateRequest::ClipboardRead {
                id: DelegateRequestId(EVENT_CAPACITY as u64),
            },
        )));
        assert_eq!(
            events.queued_len(),
            EVENT_CAPACITY - CRITICAL_EVENT_RESERVE + 4
        );
    }
}
