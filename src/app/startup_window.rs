//! Owns the initial native window before saved-state preparation begins.
use super::{Cagents, Message, Startup, refresh_ui};
use crate::{device, instance, session};
use cosmic::{Application, Core, app::cosmic::Cosmic, iced};
use iced::{Subscription, Task, window};
use std::{
    io,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

type Shell = Cosmic<Cagents>;

/// Values already validated by the binary's existing argument parser.
pub struct Request {
    pub new_window: bool,
    pub slot: Option<String>,
    pub source: Option<String>,
    pub device_window_scoped: bool,
}

#[derive(Debug)]
enum Failure {
    Io(io::Error),
    Daemon(crate::terminal::HostedTerminalStartError),
    Other(String),
    RendererBeforeState(String),
    Canceled,
    WorkerPanicked,
}

/// Keeps typed I/O causes (including update contention) across the worker.
#[derive(Debug)]
pub struct StartupFailure(Failure);

impl StartupFailure {
    pub fn renderer_before_state(&self) -> Option<&str> {
        match &self.0 {
            Failure::RendererBeforeState(detail) => Some(detail),
            _ => None,
        }
    }

    pub fn is_canceled(&self) -> bool {
        matches!(self.0, Failure::Canceled)
    }
}

impl std::fmt::Display for StartupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Failure::Io(error) => error.fmt(f),
            Failure::Daemon(error) => error.fmt(f),
            Failure::Other(error) => f.write_str(error),
            Failure::RendererBeforeState(detail) => {
                write!(f, "could not initialize window graphics: {detail}")
            }
            Failure::Canceled => f.write_str("UI startup was canceled"),
            Failure::WorkerPanicked => f.write_str("UI startup preparation panicked"),
        }
    }
}
impl std::error::Error for StartupFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.0 {
            Failure::Io(error) => Some(error),
            Failure::Daemon(error) => Some(error),
            _ => None,
        }
    }
}
impl From<io::Error> for StartupFailure {
    fn from(error: io::Error) -> Self {
        Self(Failure::Io(error))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// Private to this one startup operation. The original thread handle is never
// owned by a cancellable iced Task. T is generic only for inert owning tests.
struct Worker<T: Send + 'static> {
    state_entered: AtomicBool,
    canceled: AtomicBool,
    handle: Mutex<Option<JoinHandle<()>>>,
    result: Mutex<Option<thread::Result<T>>>,
    finished: async_channel::Sender<()>,
    completion: async_channel::Receiver<()>,
}
impl<T: Send + 'static> Worker<T> {
    fn new() -> Arc<Self> {
        let (finished, completion) = async_channel::bounded(1);
        Arc::new(Self {
            state_entered: AtomicBool::new(false),
            canceled: AtomicBool::new(false),
            handle: Mutex::new(None),
            result: Mutex::new(None),
            finished,
            completion,
        })
    }

    fn start(
        self: &Arc<Self>,
        work: impl FnOnce(&AtomicBool) -> T + Send + 'static,
    ) -> io::Result<bool> {
        if self
            .state_entered
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(false);
        }
        // StateEntered is irreversible, even when thread creation fails. It
        // precedes the thread and every device/lease/store effect in work.
        let worker = self.clone();
        let original = thread::Builder::new()
            .name("window-startup".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    work(&worker.canceled)
                }));
                *lock(&worker.result) = Some(result);
                let _ = worker.finished.send_blocking(());
            })?;
        *lock(&self.handle) = Some(original);
        Ok(true)
    }

    fn take(&self) -> Option<thread::Result<T>> {
        lock(&self.result).take()
    }

    fn join(&self) -> thread::Result<()> {
        match lock(&self.handle).take() {
            Some(original) => original.join(),
            None => Ok(()),
        }
    }
}

struct WindowOwners {
    activation: Option<instance::UiActivationListener>,
    lease: Option<instance::WindowLease>,
    device: Option<device::Cleanup>,
}
impl Drop for WindowOwners {
    fn drop(&mut self) {
        drop(self.activation.take());
        drop(self.lease.take());
        drop(self.device.take());
    }
}

struct PreparedWindow {
    startup: Option<Startup>,
    activation: Option<async_channel::Receiver<instance::UiActivation>>,
    owners: Option<WindowOwners>,
}
impl Drop for PreparedWindow {
    fn drop(&mut self) {
        // SessionRuntime::Drop joins its writer before the slot can unlock.
        drop(self.startup.take());
        drop(self.activation.take());
        drop(self.owners.take());
    }
}

type LoadResult = Result<PreparedWindow, StartupFailure>;

struct RunOwner {
    worker: Arc<Worker<LoadResult>>,
    live_owners: Mutex<Option<WindowOwners>>,
    failure: Mutex<Option<StartupFailure>>,
    finalized: AtomicBool,
    ready: AtomicBool,
}
impl RunOwner {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            worker: Worker::new(),
            live_owners: Mutex::new(None),
            failure: Mutex::new(None),
            finalized: AtomicBool::new(false),
            ready: AtomicBool::new(false),
        })
    }
    fn fail(&self, error: StartupFailure) {
        let mut first = lock(&self.failure);
        if first.is_none() {
            *first = Some(error);
        }
    }
    fn finalize(&self) {
        if self.finalized.swap(true, Ordering::AcqRel) {
            return;
        }
        self.worker.canceled.store(true, Ordering::Release);
        if self.worker.join().is_err() {
            self.fail(StartupFailure(Failure::WorkerPanicked));
        }
        // This is outside the live event loop, including late canceled loads.
        if let Some(result) = self.worker.take() {
            match result {
                Ok(Ok(prepared)) => drop(prepared),
                Ok(Err(error)) => self.fail(error),
                Err(_) => self.fail(StartupFailure(Failure::WorkerPanicked)),
            }
        }
        drop(lock(&self.live_owners).take());
    }
}

// The guard stays on the run stack outside both the iced model and Task.
struct Finalize(Arc<RunOwner>);
impl Drop for Finalize {
    fn drop(&mut self) {
        self.0.finalize();
    }
}

fn checkpoint(canceled: &AtomicBool) -> Result<(), StartupFailure> {
    if canceled.load(Ordering::Acquire) {
        Err(StartupFailure(Failure::Canceled))
    } else {
        Ok(())
    }
}

fn load_prepared_window(request: Request, canceled: &AtomicBool) -> LoadResult {
    checkpoint(canceled)?;
    let cleanup = device::Cleanup;
    if !device::is_remote() && crate::control::staged_ui_executable()?.is_some() {
        return Err(StartupFailure(Failure::Other(
            "staged UI ownership changed after graphics initialization; launch again through the matched owner".into(),
        )));
    }
    checkpoint(canceled)?;
    device::prepare()?;
    checkpoint(canceled)?;
    let unique_slot = uuid::Uuid::new_v4().to_string();
    let remote_new_window = request.new_window && device::is_remote();
    let new_window = request.new_window && !remote_new_window;
    let slot = if remote_new_window {
        Some(unique_slot.as_str())
    } else {
        request.slot.as_deref()
    };
    let scoped_slot = if request.device_window_scoped && !remote_new_window {
        slot.map(str::to_owned)
    } else {
        device::window_slot(slot)?
    };
    let lease = instance::WindowLease::acquire(new_window, scoped_slot.as_deref())?;
    checkpoint(canceled)?;
    let device_source = if request.device_window_scoped && request.source.is_some() {
        request.source
    } else {
        device::window_slot(request.source.as_deref())?
    };
    session::SessionRuntime::initialize_window(
        device_source.as_deref().unwrap_or(instance::PRIMARY_WINDOW),
    )
    .map_err(|error| StartupFailure(Failure::Other(error.to_string())))?;
    checkpoint(canceled)?;
    let fixture =
        cfg!(feature = "latency-harness") && std::env::var_os("CAGENTS_LATENCY_OUTPUT").is_some();
    if !fixture {
        crate::terminal::ensure_daemon_ready()
            .map_err(|error| StartupFailure(Failure::Daemon(error)))?;
    }
    checkpoint(canceled)?;
    let mut startup =
        Cagents::load_startup().map_err(|error| StartupFailure(Failure::Other(error)))?;
    startup.prepare_for_window();
    checkpoint(canceled)?;
    // Do not accept activation/update callbacks while no real reducer exists.
    let (sender, receiver) = async_channel::unbounded();
    let activation = instance::UiActivationListener::bind(sender)?;
    Ok(PreparedWindow {
        startup: Some(startup),
        activation: Some(receiver),
        owners: Some(WindowOwners {
            activation: Some(activation),
            lease: Some(lease),
            device: Some(cleanup),
        }),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    AwaitSurface,
    Loading,
    Closing,
    Ready,
}

#[derive(Debug, Default)]
struct Geometry {
    opened: bool,
    size: Option<iced::Size>,
    scale: Option<f32>,
    focused: Option<bool>,
}
impl Geometry {
    fn observe(&mut self, event: &window::Event) {
        match event {
            window::Event::Opened { size, .. } => {
                self.opened = true;
                self.size = Some(*size);
            }
            window::Event::Resized(size) => self.size = Some(*size),
            window::Event::Rescaled(scale) => self.scale = Some(*scale),
            window::Event::Focused => self.focused = Some(true),
            window::Event::Unfocused => self.focused = Some(false),
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
enum BootMessage {
    Surface(Result<window::Id, window::OpenError>),
    Loaded,
    Native(window::Id, window::Event),
    PendingFocus(window::Id, cosmic::keyboard_nav::Action),
    Close,
    App(cosmic::Action<Message>),
}

struct DeferredWindow {
    window: window::Id,
    pending_controls: iced::advanced::widget::Id,
    phase: Phase,
    geometry: Geometry,
    request: Option<Request>,
    owner: Arc<RunOwner>,
    shell: Option<Box<Shell>>,
    load_finished: bool,
}
impl DeferredWindow {
    fn boot(request: Request, owner: Arc<RunOwner>) -> (Self, Task<BootMessage>) {
        let mut settings = window::Settings {
            size: iced::Size::new(1440.0, 900.0),
            min_size: Some(iced::Size::new(900.0, 600.0)),
            decorations: false,
            transparent: true,
            resize_border: crate::design::WINDOW_RESIZE_BORDER as u32,
            exit_on_close_request: false,
            icon: Some(super::application_window_icon()),
            ..window::Settings::default()
        };
        settings.platform_specific.application_id = instance::APPLICATION_ID.to_owned();
        let (window, open) = window::open_configured(settings);
        (
            Self {
                window,
                pending_controls: iced::advanced::widget::Id::unique(),
                phase: Phase::AwaitSurface,
                geometry: Geometry::default(),
                request: Some(request),
                owner,
                shell: None,
                load_finished: false,
            },
            open.map(BootMessage::Surface),
        )
    }

    fn close_pending(&mut self) -> Task<BootMessage> {
        self.phase = Phase::Closing;
        self.owner.worker.canceled.store(true, Ordering::Release);
        Task::batch([window::close(self.window), iced::exit()])
    }

    fn update(&mut self, message: BootMessage) -> Task<BootMessage> {
        if self.phase == Phase::Ready {
            return match message {
                BootMessage::App(message) => self
                    .shell
                    .as_mut()
                    .unwrap()
                    .update(message)
                    .map(BootMessage::App),
                BootMessage::Close => self
                    .shell
                    .as_mut()
                    .unwrap()
                    .update(cosmic::Action::App(Message::WindowCloseRequested(
                        self.window,
                    )))
                    .map(BootMessage::App),
                // Lifecycle observations queued by the pending subscription
                // still belong to this window after promotion. Route its real
                // close through the normal dirty-close policy, not cancellation.
                BootMessage::Native(id, event) if id == self.window => {
                    let shell = self.shell.as_mut().unwrap();
                    let actions = match event {
                        window::Event::CloseRequested => {
                            vec![cosmic::Action::App(Message::WindowCloseRequested(id))]
                        }
                        window::Event::Resized(size) => vec![
                            cosmic::Action::Cosmic(cosmic::app::Action::WindowResize(
                                id,
                                size.width,
                                size.height,
                            )),
                            cosmic::Action::App(Message::WindowResized(size.width, size.height)),
                        ],
                        window::Event::Rescaled(scale) => vec![cosmic::Action::App(
                            Message::WindowScaleFactorChanged(scale),
                        )],
                        window::Event::Focused | window::Event::Unfocused => {
                            let focused = matches!(event, window::Event::Focused);
                            vec![
                                cosmic::Action::Cosmic(if focused {
                                    cosmic::app::Action::Focus(id)
                                } else {
                                    cosmic::app::Action::Unfocus(id)
                                }),
                                cosmic::Action::App(Message::RefreshUi(
                                    refresh_ui::Action::Focused(id, focused),
                                )),
                            ]
                        }
                        _ => Vec::new(),
                    };
                    Task::batch(actions.into_iter().map(|action| shell.update(action)))
                        .map(BootMessage::App)
                }
                _ => Task::none(),
            };
        }
        match message {
            BootMessage::Close => self.close_pending(),
            BootMessage::PendingFocus(id, action)
                if id == self.window
                    && matches!(self.phase, Phase::AwaitSurface | Phase::Loading) =>
            {
                self.focus_pending(action)
            }
            BootMessage::Native(id, event) if id == self.window => {
                if matches!(event, window::Event::CloseRequested | window::Event::Closed) {
                    return self.close_pending();
                }
                if self.phase == Phase::Closing {
                    return Task::none();
                }
                self.geometry.observe(&event);
                self.promote()
            }
            BootMessage::Surface(result) => self.surface_result(result, load_prepared_window),
            BootMessage::Loaded if self.phase == Phase::Loading => {
                self.load_finished = true;
                self.promote()
            }
            _ => Task::none(),
        }
    }

    fn surface_result(
        &mut self,
        result: Result<window::Id, window::OpenError>,
        load: impl FnOnce(Request, &AtomicBool) -> LoadResult + Send + 'static,
    ) -> Task<BootMessage> {
        if self.phase != Phase::AwaitSurface {
            return Task::none();
        }
        match result {
            Ok(id) if id == self.window => {
                self.phase = Phase::Loading;
                let request = self.request.take().expect("one startup request");
                match self
                    .owner
                    .worker
                    .start(move |canceled| load(request, canceled))
                {
                    Ok(true) => {
                        let completion = self.owner.worker.completion.clone();
                        Task::perform(
                            async move {
                                let _ = completion.recv().await;
                            },
                            |_| BootMessage::Loaded,
                        )
                    }
                    Ok(false) => {
                        self.owner.fail(StartupFailure(Failure::Other(
                            "UI startup was already entered".into(),
                        )));
                        self.close_pending()
                    }
                    Err(error) => {
                        self.owner.fail(error.into());
                        self.close_pending()
                    }
                }
            }
            Err(window::OpenError::GraphicsInitialization(detail))
                if !self.owner.worker.state_entered.load(Ordering::Acquire) =>
            {
                self.owner
                    .fail(StartupFailure(Failure::RendererBeforeState(detail)));
                self.close_pending()
            }
            Err(error) => {
                self.owner
                    .fail(StartupFailure(Failure::Other(error.to_string())));
                self.close_pending()
            }
            Ok(_) => {
                self.owner.fail(StartupFailure(Failure::Other(
                    "UI configured a different window".into(),
                )));
                self.close_pending()
            }
        }
    }

    fn promote(&mut self) -> Task<BootMessage> {
        if self.phase != Phase::Loading || !self.load_finished || !self.geometry.opened {
            return Task::none();
        }
        let prepared = match self.owner.worker.take() {
            Some(Ok(Ok(prepared))) => prepared,
            Some(Ok(Err(error))) => {
                self.owner.fail(error);
                return self.close_pending();
            }
            Some(Err(_)) => {
                self.owner.fail(StartupFailure(Failure::WorkerPanicked));
                return self.close_pending();
            }
            None => {
                self.owner.fail(StartupFailure(Failure::Other(
                    "UI startup completed without its original result".into(),
                )));
                return self.close_pending();
            }
        };
        let mut prepared = prepared;
        let startup = prepared.startup.take().expect("single original startup");
        let activation = prepared.activation.take();
        *lock(&self.owner.live_owners) = prepared.owners.take();
        // All original owners are installed before any normal initialization Task.
        cosmic::icon_theme::set_default(cosmic::config::icon_theme());
        let mut core = Core::default();
        core.set_main_window_id(Some(self.window));
        let (mut shell, initialize) = Shell::init((core, (activation, startup)));
        let mut tasks = vec![
            initialize,
            shell.update(cosmic::Action::Cosmic(cosmic::app::Action::Opened(
                self.window,
            ))),
        ];
        if let Some(size) = self.geometry.size {
            tasks.push(
                shell.update(cosmic::Action::Cosmic(cosmic::app::Action::WindowResize(
                    self.window,
                    size.width,
                    size.height,
                ))),
            );
            tasks.push(shell.update(cosmic::Action::App(Message::WindowResized(
                size.width,
                size.height,
            ))));
        }
        if let Some(scale) = self.geometry.scale {
            tasks.push(
                shell.update(cosmic::Action::App(Message::WindowScaleFactorChanged(
                    scale,
                ))),
            );
        }
        if let Some(focused) = self.geometry.focused {
            let action = if focused {
                cosmic::app::Action::Focus(self.window)
            } else {
                cosmic::app::Action::Unfocus(self.window)
            };
            tasks.push(shell.update(cosmic::Action::Cosmic(action)));
            tasks.push(shell.update(cosmic::Action::App(Message::RefreshUi(
                refresh_ui::Action::Focused(self.window, focused),
            ))));
        }
        tasks.push(
            shell.update(cosmic::Action::App(Message::StartupWindowOpened(
                self.window,
            ))),
        );
        self.shell = Some(Box::new(shell));
        self.phase = Phase::Ready;
        self.owner.ready.store(true, Ordering::Release);
        Task::batch(tasks).map(BootMessage::App)
    }

    fn subscription(&self) -> Subscription<BootMessage> {
        if let Some(shell) = &self.shell {
            return shell.subscription().map(BootMessage::App);
        }
        iced::event::listen_with(Self::pending_event)
    }

    fn pending_event(
        event: iced::Event,
        status: iced::event::Status,
        id: window::Id,
    ) -> Option<BootMessage> {
        match event {
            iced::Event::Window(
                event @ (window::Event::Opened { .. }
                | window::Event::Resized(_)
                | window::Event::Rescaled(_)
                | window::Event::Focused
                | window::Event::Unfocused
                | window::Event::CloseRequested
                | window::Event::Closed),
            ) => Some(BootMessage::Native(id, event)),
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Tab),
                modifiers,
                ..
            }) if status == iced::event::Status::Ignored && !modifiers.control() => {
                Some(BootMessage::PendingFocus(
                    id,
                    if modifiers.shift() {
                        cosmic::keyboard_nav::Action::FocusPrevious
                    } else {
                        cosmic::keyboard_nav::Action::FocusNext
                    },
                ))
            }
            _ => None,
        }
    }

    fn focus_pending(&self, action: cosmic::keyboard_nav::Action) -> Task<BootMessage> {
        use iced::advanced::widget::{Operation, operation};
        let traversal: Box<dyn Operation> = match action {
            cosmic::keyboard_nav::Action::FocusNext => {
                Box::new(operation::focusable::focus_next::<()>())
            }
            cosmic::keyboard_nav::Action::FocusPrevious => {
                Box::new(operation::focusable::focus_previous::<()>())
            }
            _ => return Task::none(),
        };
        // A queued traversal must not focus restored app controls after the
        // pending container has been replaced, even if its Task was issued.
        iced::runtime::task::effect(iced::runtime::Action::widget(operation::scoped(
            self.pending_controls.clone(),
            traversal,
        )))
    }
    fn title(&self, id: window::Id) -> String {
        self.shell
            .as_ref()
            .map_or_else(|| "cagents".into(), |shell| shell.title(id))
    }
    fn theme(&self, id: window::Id) -> cosmic::Theme {
        self.shell
            .as_ref()
            .map_or_else(cosmic::Theme::dark, |shell| shell.theme(id))
    }
    fn style(&self, theme: &cosmic::Theme) -> iced::theme::Style {
        self.shell.as_ref().map_or_else(
            || iced::theme::Style {
                background_color: iced::Color::from_rgb(0.08, 0.08, 0.09),
                text_color: iced::Color::WHITE,
                icon_color: iced::Color::WHITE,
            },
            |shell| shell.style(theme),
        )
    }
    fn scale_factor(&self, id: window::Id) -> f64 {
        self.shell
            .as_ref()
            .map_or(1.0, |shell| shell.scale_factor(id))
    }
    fn view(&self, id: window::Id) -> cosmic::Element<'_, BootMessage> {
        if let Some(shell) = &self.shell {
            return shell.view(id).map(BootMessage::App);
        }
        let content = cosmic::widget::column::with_children([
            cosmic::widget::text("Opening your workspace…").into(),
            cosmic::widget::button::text("Close")
                .on_press(BootMessage::Close)
                .into(),
        ])
        .spacing(16);
        cosmic::widget::container(content)
            .id(self.pending_controls.clone())
            .padding(24)
            .into()
    }
}

/// Runs one native window. Its owned worker is finalized after iced returns,
/// including cancellation, renderer failure and unwinding.
pub fn run(request: Request) -> Result<(), StartupFailure> {
    let owner = RunOwner::new();
    let finalizer = Finalize(owner.clone());
    let request = Mutex::new(Some(request));
    let boot_owner = owner.clone();
    let settings = iced::Settings {
        id: Some(instance::APPLICATION_ID.to_owned()),
        default_font: cosmic::font::default(),
        default_text_size: iced::Pixels(14.0),
        antialiasing: true,
        is_daemon: false,
        ..iced::Settings::default()
    };
    let result = iced::daemon(
        move || {
            DeferredWindow::boot(
                lock(&request).take().expect("one window boot"),
                boot_owner.clone(),
            )
        },
        DeferredWindow::update,
        DeferredWindow::view,
    )
    .subscription(DeferredWindow::subscription)
    .title(DeferredWindow::title)
    .style(DeferredWindow::style)
    .theme(DeferredWindow::theme)
    .scale_factor(DeferredWindow::scale_factor)
    .settings(settings)
    .run();
    if let Err(error) = result {
        owner.fail(StartupFailure(Failure::Other(error.to_string())));
    }
    finalizer.0.finalize();
    if let Some(error) = lock(&owner.failure).take() {
        return Err(error);
    }
    if !owner.ready.load(Ordering::Acquire) {
        return Err(StartupFailure(Failure::Canceled));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicUsize, mpsc};

    fn pending() -> (DeferredWindow, Arc<RunOwner>) {
        let owner = RunOwner::new();
        let (window, task) = DeferredWindow::boot(
            Request {
                new_window: false,
                slot: None,
                source: None,
                device_window_scoped: false,
            },
            owner.clone(),
        );
        drop(task); // Source-only window action, no native event loop.
        (window, owner)
    }

    #[test]
    fn checked_first_surface_gates_every_startup_effect() {
        for error in [
            window::OpenError::ZeroSize,
            window::OpenError::Unsupported,
            window::OpenError::NativeCreation("fixture".into()),
            window::OpenError::Canceled,
            window::OpenError::GraphicsInitialization("fixture".into()),
        ] {
            let (mut pending, owner) = pending();
            let _finalize = Finalize(owner.clone());
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            drop(pending.surface_result(Err(error.clone()), move |_, _| {
                observed.fetch_add(1, Ordering::AcqRel);
                Err(StartupFailure(Failure::Other("must not load".into())))
            }));
            assert_eq!(calls.load(Ordering::Acquire), 0);
            assert!(!owner.worker.state_entered.load(Ordering::Acquire));
            let failure = lock(&owner.failure);
            assert_eq!(
                failure.as_ref().unwrap().renderer_before_state().is_some(),
                matches!(error, window::OpenError::GraphicsInitialization(_))
            );
            drop(failure);
            owner.finalize();
        }
    }

    #[test]
    fn close_before_configuration_never_loads_state() {
        let (mut pending, owner) = pending();
        let _finalize = Finalize(owner.clone());
        drop(pending.close_pending());
        drop(pending.surface_result(Ok(pending.window), |_, _| {
            panic!("closed window must not load")
        }));
        assert_eq!(pending.phase, Phase::Closing);
        assert!(!owner.worker.state_entered.load(Ordering::Acquire));
        owner.finalize();
    }

    #[test]
    fn original_configured_window_starts_once_and_never_resets_state_entry() {
        let (mut pending, owner) = pending();
        let _finalize = Finalize(owner.clone());
        let (started, received) = mpsc::channel();
        drop(pending.surface_result(Ok(pending.window), move |_, _| {
            started.send(()).unwrap();
            Err(StartupFailure(Failure::Other(
                "fixture load failure".into(),
            )))
        }));
        received.recv().unwrap();
        assert!(owner.worker.state_entered.load(Ordering::Acquire));
        drop(pending.surface_result(Ok(pending.window), |_, _| panic!("must not load twice")));
        owner.finalize();
        assert!(owner.worker.state_entered.load(Ordering::Acquire));
        assert!(
            lock(&owner.failure)
                .as_ref()
                .unwrap()
                .renderer_before_state()
                .is_none()
        );
    }

    #[test]
    fn wrong_window_cannot_start_preparation() {
        let (mut pending, owner) = pending();
        let _finalize = Finalize(owner.clone());
        drop(pending.surface_result(Ok(window::Id::unique()), |_, _| panic!("wrong window")));
        assert!(!owner.worker.state_entered.load(Ordering::Acquire));
        assert!(
            lock(&owner.failure)
                .as_ref()
                .unwrap()
                .renderer_before_state()
                .is_none()
        );
        owner.finalize();
    }

    #[test]
    fn pending_geometry_coalesces_only_real_window_observations() {
        let mut geometry = Geometry::default();
        geometry.observe(&window::Event::Opened {
            position: None,
            size: iced::Size::new(1440.0, 900.0),
        });
        geometry.observe(&window::Event::Resized(iced::Size::new(980.0, 700.0)));
        geometry.observe(&window::Event::Rescaled(1.5));
        geometry.observe(&window::Event::Focused);
        geometry.observe(&window::Event::Unfocused);
        assert!(geometry.opened);
        assert_eq!(geometry.size, Some(iced::Size::new(980.0, 700.0)));
        assert_eq!(geometry.scale, Some(1.5));
        assert_eq!(geometry.focused, Some(false));
    }

    struct ReleaseAndJoin<T: Send + 'static> {
        release: Option<mpsc::Sender<()>>,
        worker: Arc<Worker<T>>,
    }
    impl<T: Send + 'static> Drop for ReleaseAndJoin<T> {
        fn drop(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
            let _ = self.worker.join();
            drop(self.worker.take());
        }
    }

    #[test]
    fn canceled_worker_is_joined_and_late_payload_disposed_before_outer_return() {
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::AcqRel);
            }
        }
        let worker = Worker::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let (release, held) = mpsc::channel();
        let (entered, started) = mpsc::channel();
        let mut guard = ReleaseAndJoin {
            release: Some(release),
            worker: worker.clone(),
        };
        let observed = drops.clone();
        worker
            .start(move |canceled| {
                entered.send(()).unwrap();
                held.recv().unwrap();
                assert!(canceled.load(Ordering::Acquire));
                Payload(observed)
            })
            .unwrap();
        started.recv().unwrap();
        worker.canceled.store(true, Ordering::Release);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        guard.release.take().unwrap().send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(drops.load(Ordering::Acquire), 0);
        drop(worker.take().unwrap().unwrap());
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(worker.state_entered.load(Ordering::Acquire));
    }

    #[test]
    fn worker_preserves_typed_update_contention_and_already_running_errors() {
        let update = StartupFailure::from(io::Error::new(
            io::ErrorKind::WouldBlock,
            instance::UpdateInProgress,
        ));
        assert!(instance::is_update_in_progress(&update));
        let already = StartupFailure::from(io::Error::other(
            instance::ApplicationLeaseError::AlreadyRunning,
        ));
        assert_eq!(
            already.to_string(),
            instance::ApplicationLeaseError::AlreadyRunning.to_string()
        );
        assert!(already.renderer_before_state().is_none());
        assert!(!instance::is_update_in_progress(&already));
    }

    #[test]
    fn prepared_payload_is_send_without_moving_the_native_shell() {
        fn is_send<T: Send>() {}
        is_send::<Startup>();
        is_send::<PreparedWindow>();
        is_send::<LoadResult>();
    }

    fn keyboard_event(
        named: iced::keyboard::key::Named,
        modifiers: iced::keyboard::Modifiers,
    ) -> iced::Event {
        let key = iced::keyboard::Key::Named(named);
        iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: iced::keyboard::key::Physical::Unidentified(
                iced::keyboard::key::NativeCode::Unidentified,
            ),
            location: iced::keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        })
    }

    fn headless_renderer() -> cosmic::Renderer {
        use iced::advanced::renderer::Headless;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(cosmic::Renderer::new(
                iced::Font::DEFAULT,
                iced::Pixels(16.0),
                Some("tiny-skia"),
            ))
            .expect("create the existing headless software renderer")
    }

    fn apply_widget_task(
        task: Task<BootMessage>,
        element: &mut cosmic::Element<'_, BootMessage>,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
    ) {
        use iced::{advanced::widget::operation::Outcome, futures::StreamExt};
        let mut stream = iced::runtime::task::into_stream(task).expect("widget focus task");
        let action = iced::futures::executor::block_on(stream.next()).expect("focus operation");
        let iced::runtime::Action::Widget(mut operation) = action else {
            panic!("pending navigation must use a real widget operation")
        };
        let node = element.as_widget_mut().layout(
            tree,
            renderer,
            &iced::advanced::layout::Limits::new(iced::Size::ZERO, iced::Size::new(900.0, 600.0)),
        );
        loop {
            element.as_widget_mut().operate(
                tree,
                iced::advanced::Layout::new(&node),
                renderer,
                operation.as_mut(),
            );
            match operation.finish() {
                Outcome::None => break,
                Outcome::Chain(next) => operation = next,
                Outcome::Some(()) => panic!("focus traversal must not inject a reducer message"),
            }
        }
        assert!(iced::futures::executor::block_on(stream.next()).is_none());
    }

    fn enter_widget(
        element: &mut cosmic::Element<'_, BootMessage>,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
    ) -> Vec<BootMessage> {
        use iced::advanced::{Layout, Shell, clipboard, layout, mouse};
        let node = element.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(900.0, 600.0)),
        );
        let layout = Layout::new(&node);
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            tree,
            &keyboard_event(
                iced::keyboard::key::Named::Enter,
                iced::keyboard::Modifiers::empty(),
            ),
            layout,
            mouse::Cursor::Unavailable,
            renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &layout.bounds(),
        );
        messages
    }

    #[test]
    fn pending_tab_and_shift_tab_focus_the_real_close_button_for_enter() {
        let renderer = headless_renderer();
        for (modifiers, expected) in [
            (
                iced::keyboard::Modifiers::empty(),
                cosmic::keyboard_nav::Action::FocusNext,
            ),
            (
                iced::keyboard::Modifiers::SHIFT,
                cosmic::keyboard_nav::Action::FocusPrevious,
            ),
        ] {
            let (mut pending, owner) = pending();
            let _finalize = Finalize(owner.clone());
            let mut tree = {
                let mut element = pending.view(pending.window);
                let mut tree = iced::advanced::widget::Tree::new(element.as_widget());
                assert!(enter_widget(&mut element, &mut tree, &renderer).is_empty());
                tree
            };
            let event = keyboard_event(iced::keyboard::key::Named::Tab, modifiers);
            let message =
                DeferredWindow::pending_event(event, iced::event::Status::Ignored, pending.window)
                    .expect("actual ignored Tab enters pending navigation");
            assert!(matches!(&message, BootMessage::PendingFocus(id, action)
                if *id == pending.window && *action == expected));
            let task = pending.update(message);
            let mut element = pending.view(pending.window);
            tree.diff(element.as_widget_mut());
            apply_widget_task(task, &mut element, &mut tree, &renderer);
            let messages = enter_widget(&mut element, &mut tree, &renderer);
            assert!(matches!(messages.as_slice(), [BootMessage::Close]));
            assert!(!owner.worker.state_entered.load(Ordering::Acquire));
        }
    }

    #[test]
    fn pending_keyboard_navigation_cannot_replay_into_ready_controls() {
        let renderer = headless_renderer();
        let (mut pending, owner) = pending();
        let _finalize = Finalize(owner.clone());
        let tab = keyboard_event(
            iced::keyboard::key::Named::Tab,
            iced::keyboard::Modifiers::empty(),
        );
        let message = DeferredWindow::pending_event(
            tab.clone(),
            iced::event::Status::Ignored,
            pending.window,
        )
        .unwrap();
        let issued = pending.update(message.clone());
        // A different widget scope stands for the replacement view; this is a
        // widget-operation contract, not a fabricated Ready app/native proof.
        let mut replacement: cosmic::Element<'_, BootMessage> = cosmic::widget::container(
            cosmic::widget::button::text("Restored action").on_press(BootMessage::Close),
        )
        .id(iced::advanced::widget::Id::unique())
        .into();
        let mut tree = iced::advanced::widget::Tree::new(replacement.as_widget());
        assert!(enter_widget(&mut replacement, &mut tree, &renderer).is_empty());
        apply_widget_task(issued, &mut replacement, &mut tree, &renderer);
        assert!(enter_widget(&mut replacement, &mut tree, &renderer).is_empty());
        // Exercise only the actual phase guard: no saved-state app is created.
        pending.phase = Phase::Ready;
        assert!(iced::runtime::task::into_stream(pending.update(message)).is_none());
        assert!(
            DeferredWindow::pending_event(tab, iced::event::Status::Captured, pending.window)
                .is_none()
        );
        assert!(
            DeferredWindow::pending_event(
                keyboard_event(
                    iced::keyboard::key::Named::Enter,
                    iced::keyboard::Modifiers::empty()
                ),
                iced::event::Status::Ignored,
                pending.window,
            )
            .is_none()
        );
        assert!(
            DeferredWindow::pending_event(
                keyboard_event(
                    iced::keyboard::key::Named::Tab,
                    iced::keyboard::Modifiers::CTRL
                ),
                iced::event::Status::Ignored,
                pending.window,
            )
            .is_none()
        );
    }
}
