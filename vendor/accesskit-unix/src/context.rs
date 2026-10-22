// Copyright 2023 The AccessKit Authors. All rights reserved.
// Licensed under the Apache License, Version 2.0 (found in
// the LICENSE-APACHE file) or the MIT license (found in
// the LICENSE-MIT file), at your option.

use accesskit::{ActivationHandler, DeactivationHandler};
use accesskit_atspi_common::{Adapter as AdapterImpl, AppContext, Event};
#[cfg(not(feature = "tokio"))]
use async_channel::{Receiver, Sender};
use atspi::proxy::bus::StatusProxy;
use futures_lite::{future, FutureExt, StreamExt};
use std::{
    fmt::Display,
    future::Future,
    pin::pin,
    sync::{Arc, Mutex, OnceLock, RwLock},
    thread,
    time::{Duration, Instant},
};
#[cfg(feature = "tokio")]
use tokio::sync::mpsc::{UnboundedReceiver as Receiver, UnboundedSender as Sender};
#[cfg(feature = "tokio")]
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, warn};
use zbus::{
    connection::Builder,
    fdo::{DBusProxy, NameOwnerChangedStream},
    names::{BusName, OwnedUniqueName, WellKnownName},
    proxy::{CacheProperties, PropertyChanged, PropertyStream},
    Connection,
};

use crate::{
    adapter::{AdapterState, Callback, Message},
    atspi::Bus,
    executor::{sleep_until, Executor, Task},
    util::block_on,
};

static APP_CONTEXT: OnceLock<Arc<RwLock<AppContext>>> = OnceLock::new();
static MESSAGES: OnceLock<Sender<Message>> = OnceLock::new();

/// The session-bus name of at-spi-bus-launcher, which publishes the
/// screen-reader status and the address of the AT-SPI bus.
const LAUNCHER: &str = "org.a11y.Bus";

fn app_name() -> Option<String> {
    std::env::current_exe().ok().and_then(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_string())
    })
}

pub(crate) fn get_or_init_app_context<'a>() -> &'a Arc<RwLock<AppContext>> {
    APP_CONTEXT.get_or_init(|| AppContext::new(app_name()))
}

pub(crate) fn get_or_init_messages() -> Sender<Message> {
    MESSAGES
        .get_or_init(|| {
            #[cfg(not(feature = "tokio"))]
            let (tx, rx) = async_channel::unbounded();
            #[cfg(feature = "tokio")]
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

            thread::spawn(|| {
                let executor = Executor::new();
                block_on(executor.run(run_context(&executor, rx, connect_session_bus)))
            });

            tx
        })
        .clone()
}

async fn connect_session_bus() -> zbus::Result<Connection> {
    Builder::session()?.internal_executor(false).build().await
}

/// Serves adapters until every message sender is dropped. Unreachable or lost
/// buses never end the context: adapters stay registered, are deactivated
/// while no AT-SPI bus is connected, and are activated again once one is.
async fn run_context<C, F>(executor: &Executor<'_>, rx: Receiver<Message>, connect_session: C)
where
    C: FnMut() -> F,
    F: Future<Output = zbus::Result<Connection>>,
{
    #[cfg(feature = "tokio")]
    let rx = UnboundedReceiverStream::new(rx);
    let mut messages = pin!(rx);
    let mut context = Context {
        executor,
        connect_session,
        session: None,
        session_reconnect: Reconnect::default(),
        atspi_bus: None,
        atspi_reconnect: Reconnect::default(),
        adapters: Vec::new(),
    };
    context.connect_session().await;
    loop {
        let message = async { ContextEvent::Message(messages.next().await) };
        match context.next_event().or(message).await {
            ContextEvent::Message(Some(message)) => context.process_message(message).await,
            ContextEvent::Message(None) => return,
            ContextEvent::SessionClosed => context.session_lost(),
            ContextEvent::LauncherOwnerChanged(owner) => {
                context.launcher_owner_changed(owner).await
            }
            ContextEvent::ScreenReaderChanged(change) => {
                context.screen_reader_changed(change).await
            }
            ContextEvent::AtspiBusClosed => context.atspi_bus_lost(&"connection closed"),
            ContextEvent::ReconnectDue => context.reconnect().await,
        }
    }
}

enum ContextEvent {
    Message(Option<Message>),
    SessionClosed,
    LauncherOwnerChanged(Option<OwnedUniqueName>),
    ScreenReaderChanged(PropertyChanged<'static, bool>),
    AtspiBusClosed,
    ReconnectDue,
}

struct Context<'a, 'e, C> {
    executor: &'a Executor<'e>,
    connect_session: C,
    session: Option<Session>,
    session_reconnect: Reconnect,
    atspi_bus: Option<Bus>,
    atspi_reconnect: Reconnect,
    adapters: Vec<AdapterEntry>,
}

impl<C, F> Context<'_, '_, C>
where
    C: FnMut() -> F,
    F: Future<Output = zbus::Result<Connection>>,
{
    /// Waits for a bus to change state or for a reconnection attempt to be due.
    async fn next_event(&mut self) -> ContextEvent {
        let Some(Session {
            connection,
            launcher_owner_changes,
            screen_reader_changes,
            ..
        }) = &mut self.session
        else {
            self.session_reconnect.due().await;
            return ContextEvent::ReconnectDue;
        };
        let atspi_bus = &self.atspi_bus;
        let atspi_reconnect = &self.atspi_reconnect;
        let session_closed = async {
            connection.closed().await;
            ContextEvent::SessionClosed
        };
        let launcher_owner_changed = async {
            let Some(change) = launcher_owner_changes.next().await else {
                return ContextEvent::SessionClosed;
            };
            let owner = change.args().ok().and_then(|args| {
                args.new_owner()
                    .as_ref()
                    .map(|owner| OwnedUniqueName::from(owner.to_owned()))
            });
            ContextEvent::LauncherOwnerChanged(owner)
        };
        let atspi_bus_closed = async {
            match atspi_bus {
                Some(bus) => bus.closed().await,
                None => future::pending().await,
            }
            ContextEvent::AtspiBusClosed
        };
        let reconnect_due = async {
            atspi_reconnect.due().await;
            ContextEvent::ReconnectDue
        };
        let screen_reader_changed = async {
            if let Some(changes) = screen_reader_changes {
                if let Some(change) = changes.next().await {
                    return ContextEvent::ScreenReaderChanged(change);
                }
            }
            future::pending().await
        };
        session_closed
            .or(launcher_owner_changed)
            .or(atspi_bus_closed)
            .or(screen_reader_changed)
            .or(reconnect_due)
            .await
    }

    async fn connect_session(&mut self) {
        let connection = (self.connect_session)();
        let session = async { Session::new(connection.await?, self.executor).await }.await;
        match session {
            Ok(session) => {
                self.session = Some(session);
                self.session_reconnect.connected();
            }
            Err(error) => self
                .session_reconnect
                .schedule("The D-Bus session bus", &error),
        }
    }

    fn session_lost(&mut self) {
        self.session = None;
        self.disconnect_atspi_bus();
        self.atspi_reconnect.reset();
        self.session_reconnect
            .schedule("The D-Bus session bus", &"connection closed");
    }

    /// A new launcher runs a new AT-SPI bus, and a departed one runs none.
    async fn launcher_owner_changed(&mut self, owner: Option<OwnedUniqueName>) {
        let Some(session) = &self.session else {
            return;
        };
        if session.launcher_owner == owner {
            return;
        }
        self.disconnect_atspi_bus();
        self.atspi_reconnect.reset();
        if let Some(session) = &mut self.session {
            match owner {
                Some(owner) => session.watch_launcher(Some(owner)).await,
                None => session.forget_launcher(),
            }
        }
    }

    async fn screen_reader_changed(&mut self, change: PropertyChanged<'static, bool>) {
        let enabled = change.get().await.unwrap_or_else(|error| {
            debug!(%error, "Unreadable screen-reader status; treating it as disabled");
            false
        });
        let Some(session) = &mut self.session else {
            return;
        };
        session.screen_reader_enabled = enabled;
        if !enabled {
            self.disconnect_atspi_bus();
            self.atspi_reconnect.reset();
        } else if self.atspi_bus.is_none() {
            self.connect_atspi_bus().await;
        }
    }

    async fn connect_atspi_bus(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        match Bus::new(&session.connection, self.executor).await {
            Ok(bus) => {
                self.atspi_bus = Some(bus);
                self.atspi_reconnect.connected();
                for entry in &mut self.adapters {
                    activate_adapter(entry);
                }
            }
            Err(error) => self.atspi_bus_lost(&error),
        }
    }

    fn atspi_bus_lost(&mut self, error: &dyn Display) {
        self.disconnect_atspi_bus();
        self.atspi_reconnect.schedule("The AT-SPI bus", error);
    }

    fn disconnect_atspi_bus(&mut self) {
        self.atspi_bus = None;
        for entry in &mut self.adapters {
            deactivate_adapter(entry);
        }
    }

    async fn process_message(&mut self, message: Message) {
        if let Err(error) =
            process_adapter_message(&self.atspi_bus, &mut self.adapters, message).await
        {
            self.atspi_bus_lost(&error);
        }
    }

    async fn reconnect(&mut self) {
        match &self.session {
            None => {
                self.session_reconnect.cancel();
                self.connect_session().await
            }
            Some(session) => {
                self.atspi_reconnect.cancel();
                if session.screen_reader_enabled && self.atspi_bus.is_none() {
                    self.connect_atspi_bus().await
                }
            }
        }
    }
}

/// A session-bus connection and the screen-reader status published by the
/// launcher's current owner, which decides whether an AT-SPI bus is wanted.
struct Session {
    connection: Connection,
    _task: Task<()>,
    launcher_owner_changes: NameOwnerChangedStream,
    launcher_owner: Option<OwnedUniqueName>,
    screen_reader_changes: Option<PropertyStream<'static, bool>>,
    screen_reader_enabled: bool,
}

impl Session {
    async fn new(connection: Connection, executor: &Executor<'_>) -> zbus::Result<Self> {
        let connection_copy = connection.clone();
        let task = executor.spawn(
            async move {
                loop {
                    connection_copy.executor().tick().await;
                }
            },
            "accesskit_session_bus_task",
        );
        let dbus = DBusProxy::builder(&connection)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        // Subscribe before looking up the owner so that no change is missed.
        let launcher_owner_changes = dbus
            .receive_name_owner_changed_with_args(&[(0, LAUNCHER)])
            .await?;
        let launcher = BusName::from(WellKnownName::from_static_str_unchecked(LAUNCHER));
        let launcher_owner = dbus.get_name_owner(launcher).await.ok();
        let mut session = Self {
            connection,
            _task: task,
            launcher_owner_changes,
            launcher_owner: None,
            screen_reader_changes: None,
            screen_reader_enabled: false,
        };
        session.watch_launcher(launcher_owner).await;
        Ok(session)
    }

    /// Watches the screen-reader status published by `owner`. Without an owner,
    /// this may start the launcher by D-Bus activation, as upstream does. An
    /// unavailable launcher is not an error: the owner watch reports when one
    /// appears.
    async fn watch_launcher(&mut self, owner: Option<OwnedUniqueName>) {
        self.forget_launcher();
        self.launcher_owner = owner;
        let status = StatusProxy::builder(&self.connection)
            .cache_properties(CacheProperties::Yes)
            .build()
            .await;
        match status {
            Ok(status) => {
                self.screen_reader_changes =
                    Some(status.receive_screen_reader_enabled_changed().await)
            }
            Err(error) => debug!(%error, "The AT-SPI status is unavailable"),
        }
    }

    fn forget_launcher(&mut self) {
        self.launcher_owner = None;
        self.screen_reader_changes = None;
        self.screen_reader_enabled = false;
    }
}

/// Paces reconnection to one bus while it is unreachable. The first failure of
/// an outage is a warning and later ones are debug messages; attempts back off
/// exponentially to one every `MAX_DELAY`. An outage ends once a connection
/// stays up for `MAX_DELAY`, so a bus that accepts connections and then drops
/// them keeps backing off instead of being retried every second.
#[derive(Debug)]
struct Reconnect {
    delay: Duration,
    due: Option<Instant>,
    warned: bool,
    connected_at: Option<Instant>,
}

impl Default for Reconnect {
    fn default() -> Self {
        Self {
            delay: Self::FIRST_DELAY,
            due: None,
            warned: false,
            connected_at: None,
        }
    }
}

impl Reconnect {
    const FIRST_DELAY: Duration = Duration::from_secs(1);
    const MAX_DELAY: Duration = Duration::from_secs(30);

    fn connected(&mut self) {
        self.due = None;
        self.connected_at = Some(Instant::now());
    }

    fn schedule(&mut self, bus: &str, error: &dyn Display) {
        let connected_at = self.connected_at.take();
        if connected_at.is_some_and(|at| at.elapsed() >= Self::MAX_DELAY) {
            *self = Self::default();
        }
        if self.warned {
            debug!(%error, "{bus} is still unreachable; retrying in {:?}", self.delay);
        } else {
            warn!(%error, "{bus} is unreachable; accessibility is unavailable until it is reachable again");
            self.warned = true;
        }
        self.due = Some(Instant::now() + self.delay);
        self.delay = (self.delay * 2).min(Self::MAX_DELAY);
    }

    fn cancel(&mut self) {
        self.due = None;
    }

    /// Ends the outage because the bus is no longer wanted.
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// Resolves when a scheduled attempt is due, and never otherwise.
    async fn due(&self) {
        match self.due {
            Some(deadline) => sleep_until(deadline).await,
            None => future::pending().await,
        }
    }
}

struct AdapterEntry {
    id: usize,
    activation_handler: Box<dyn ActivationHandler>,
    deactivation_handler: Box<dyn DeactivationHandler>,
    state: Arc<Mutex<AdapterState>>,
}

fn activate_adapter(entry: &mut AdapterEntry) {
    let mut state = entry.state.lock().unwrap();
    if let AdapterState::Inactive {
        is_window_focused,
        root_window_bounds,
        action_handler,
    } = &*state
    {
        *state = match entry.activation_handler.request_initial_tree() {
            Some(initial_state) => {
                let r#impl = AdapterImpl::with_wrapped_action_handler(
                    entry.id,
                    get_or_init_app_context(),
                    Callback::new(),
                    initial_state,
                    *is_window_focused,
                    *root_window_bounds,
                    Arc::clone(action_handler),
                );
                AdapterState::Active(r#impl)
            }
            None => AdapterState::Pending {
                is_window_focused: *is_window_focused,
                root_window_bounds: *root_window_bounds,
                action_handler: Arc::clone(action_handler),
            },
        };
    }
}

fn deactivate_adapter(entry: &mut AdapterEntry) {
    let mut state = entry.state.lock().unwrap();
    match &*state {
        AdapterState::Inactive { .. } => (),
        AdapterState::Pending {
            is_window_focused,
            root_window_bounds,
            action_handler,
        } => {
            *state = AdapterState::Inactive {
                is_window_focused: *is_window_focused,
                root_window_bounds: *root_window_bounds,
                action_handler: Arc::clone(action_handler),
            };
            drop(state);
            entry.deactivation_handler.deactivate_accessibility();
        }
        AdapterState::Active(r#impl) => {
            *state = AdapterState::Inactive {
                is_window_focused: r#impl.is_window_focused(),
                root_window_bounds: r#impl.root_window_bounds(),
                action_handler: r#impl.wrapped_action_handler(),
            };
            drop(state);
            entry.deactivation_handler.deactivate_accessibility();
        }
    }
}

async fn process_adapter_message(
    atspi_bus: &Option<Bus>,
    adapters: &mut Vec<AdapterEntry>,
    message: Message,
) -> zbus::Result<()> {
    match message {
        Message::AddAdapter {
            id,
            activation_handler,
            deactivation_handler,
            state,
        } => {
            adapters.push(AdapterEntry {
                id,
                activation_handler,
                deactivation_handler,
                state,
            });
            if atspi_bus.is_some() {
                let entry = adapters.last_mut().unwrap();
                activate_adapter(entry);
            }
        }
        Message::RemoveAdapter { id } => {
            if let Ok(index) = adapters.binary_search_by(|entry| entry.id.cmp(&id)) {
                adapters.remove(index);
            }
        }
        Message::RegisterInterfaces { node, interfaces } => {
            if let Some(bus) = atspi_bus {
                bus.register_interfaces(node, interfaces).await?
            }
        }
        Message::UnregisterInterfaces {
            adapter_id,
            node_id,
            interfaces,
        } => {
            if let Some(bus) = atspi_bus {
                bus.unregister_interfaces(adapter_id, node_id, interfaces)
                    .await?
            }
        }
        Message::EmitEvent {
            adapter_id,
            event: Event::Object { target, event },
        } => {
            if let Some(bus) = atspi_bus {
                bus.emit_object_event(adapter_id, target, event).await?
            }
        }
        Message::EmitEvent {
            adapter_id,
            event:
                Event::Window {
                    target,
                    name,
                    event,
                },
        } => {
            if let Some(bus) = atspi_bus {
                bus.emit_window_event(adapter_id, target, name, event)
                    .await?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use accesskit::{ActionHandler, ActionRequest, TreeUpdate};
    use accesskit_atspi_common::ActionHandlerWrapper;
    use std::{
        fs::File,
        io::{BufRead, BufReader, Read},
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
    };
    use zbus::zvariant::OwnedObjectPath;

    /// Set in the re-executed copy of this test binary that owns an isolated environment.
    const ISOLATED: &str = "ACCESSKIT_UNIX_ISOLATED_TEST";
    const PATIENCE: Duration = Duration::from_secs(15);

    #[test]
    fn reconnection_backs_off_exponentially_and_warns_once_per_outage() {
        let mut reconnect = Reconnect::default();
        let mut delays = Vec::new();
        for _ in 0..7 {
            delays.push(reconnect.delay.as_secs());
            let before = Instant::now();
            reconnect.schedule("The test bus", &"unreachable");
            assert!(
                reconnect.due.unwrap() >= before + Duration::from_secs(delays[delays.len() - 1])
            );
            assert!(reconnect.warned);
        }
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);

        // A connection that drops at once continues the outage.
        reconnect.connected();
        assert_eq!(reconnect.due, None);
        reconnect.schedule("The test bus", &"closed");
        assert_eq!(reconnect.delay, Reconnect::MAX_DELAY);

        // One that stayed up starts a new outage, which warns again.
        reconnect.connected();
        reconnect.connected_at = Instant::now().checked_sub(Reconnect::MAX_DELAY);
        reconnect.schedule("The test bus", &"closed");
        assert_eq!(reconnect.delay, 2 * Reconnect::FIRST_DELAY);
        assert!(reconnect.warned);

        reconnect.reset();
        assert_eq!(reconnect.delay, Reconnect::FIRST_DELAY);
        assert_eq!(reconnect.due, None);
        assert!(!reconnect.warned);
    }

    #[test]
    fn unreachable_atspi_bus_is_retried_and_reconnected() {
        if !in_isolated_process(
            "unreachable_atspi_bus_is_retried_and_reconnected",
            SessionBus::Running,
        ) {
            return;
        }
        let dir = TempDir::new();
        let atspi_socket = dir.path().join("atspi-bus");
        let atspi_bus = start_atspi_bus(&atspi_socket);
        let (observed_tx, observed) = mpsc::channel();
        let _launcher = serve_launcher(&dir, 1, observed_tx.clone());
        let (tx, context, finished) = start_context(&observed_tx);

        // The first address reported by the launcher has no bus behind it.
        for expected in [
            Observed::SessionConnect,
            Observed::AddressRequested,
            Observed::AddressRequested,
            Observed::Activated,
        ] {
            assert_eq!(observed.recv_timeout(PATIENCE), Ok(expected));
        }

        // Losing the bus deactivates the adapter until a bus is reachable again.
        drop(atspi_bus);
        expect(&observed, &context, Observed::Deactivated);
        expect(&observed, &context, Observed::AddressRequested);
        let _atspi_bus = start_atspi_bus(&atspi_socket);
        expect(&observed, &context, Observed::Activated);

        expect_shutdown(tx, context, finished);
    }

    #[test]
    fn unreachable_session_bus_is_retried_and_reconnected() {
        if !in_isolated_process(
            "unreachable_session_bus_is_retried_and_reconnected",
            SessionBus::Missing,
        ) {
            return;
        }
        let dir = TempDir::new();
        let (observed_tx, observed) = mpsc::channel();
        let (tx, context, finished) = start_context(&observed_tx);

        // Nothing listens on the session bus address yet.
        for expected in [Observed::SessionConnect, Observed::SessionConnect] {
            assert_eq!(observed.recv_timeout(PATIENCE), Ok(expected));
        }
        let session_bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap();
        let _session_bus =
            PrivateBus::start(Path::new(session_bus.strip_prefix("unix:path=").unwrap()));
        let _atspi_bus = start_atspi_bus(&dir.path().join("atspi-bus"));
        let _launcher = serve_launcher(&dir, 0, observed_tx);
        expect(&observed, &context, Observed::Activated);

        expect_shutdown(tx, context, finished);
    }

    #[test]
    fn launcher_departure_and_return_are_followed() {
        if !in_isolated_process(
            "launcher_departure_and_return_are_followed",
            SessionBus::Running,
        ) {
            return;
        }
        let dir = TempDir::new();
        let _atspi_bus = start_atspi_bus(&dir.path().join("atspi-bus"));
        let (observed_tx, observed) = mpsc::channel();
        let launcher = serve_launcher(&dir, 0, observed_tx.clone());
        let (tx, context, finished) = start_context(&observed_tx);
        expect(&observed, &context, Observed::Activated);

        // The AT-SPI bus outlives the launcher here, so only the owner watch
        // can notice that it left and that a new one appeared.
        drop(launcher);
        expect(&observed, &context, Observed::Deactivated);
        let _launcher = serve_launcher(&dir, 0, observed_tx);
        expect(&observed, &context, Observed::Activated);

        expect_shutdown(tx, context, finished);
    }

    enum SessionBus {
        /// A private bus is running at the session bus address.
        Running,
        /// Nothing listens at the session bus address until the test starts a bus there.
        Missing,
    }

    /// Environment variables are process-global, so each scenario re-executes
    /// this test binary with its own session bus address and without
    /// `AT_SPI_BUS_ADDRESS`. Every bus involved is a private `dbus-daemon`; the
    /// user's session and accessibility buses are never contacted. Returns true
    /// inside that process.
    fn in_isolated_process(test: &str, session_bus: SessionBus) -> bool {
        if std::env::var_os(ISOLATED).is_some() {
            return true;
        }
        let dir = TempDir::new();
        let socket = dir.path().join("session-bus");
        let running =
            matches!(session_bus, SessionBus::Running).then(|| PrivateBus::start(&socket));
        let address = match &running {
            Some(bus) => bus.address.clone(),
            None => format!("unix:path={}", socket.display()),
        };
        let test = format!("context::tests::{test}");
        let log = dir.path().join("output");
        let status = Command::new(std::env::current_exe().unwrap())
            .args([test.as_str(), "--exact", "--nocapture", "--test-threads=1"])
            .env(ISOLATED, "1")
            .env("DBUS_SESSION_BUS_ADDRESS", address)
            .env_remove("AT_SPI_BUS_ADDRESS")
            .stdout(File::create(&log).unwrap())
            .status()
            .unwrap();
        drop(running);
        let mut output = String::new();
        File::open(&log)
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        assert!(
            status.success() && output.contains("test result: ok. 1 passed"),
            "isolated `{test}` failed\n{output}"
        );
        false
    }

    struct TempDir(PathBuf);

    impl TempDir {
        /// A new directory, short enough to hold Unix socket paths.
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let index = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("accesskit-{}-{index}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A private `dbus-daemon` listening on `socket`; killed when dropped.
    struct PrivateBus {
        daemon: Child,
        address: String,
    }

    impl PrivateBus {
        fn start(socket: &Path) -> Self {
            let config = socket.with_extension("conf");
            std::fs::write(
                &config,
                format!(
                    r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                    socket.display()
                ),
            )
            .unwrap();
            let mut daemon = Command::new("dbus-daemon")
                .arg(format!("--config-file={}", config.display()))
                .args(["--nofork", "--nopidfile", "--nosyslog", "--print-address"])
                .stdout(Stdio::piped())
                .spawn()
                .expect("these tests run private buses with dbus-daemon");
            let mut address = String::new();
            BufReader::new(daemon.stdout.take().unwrap())
                .read_line(&mut address)
                .unwrap();
            assert!(!address.trim().is_empty(), "dbus-daemon did not start");
            Self {
                daemon,
                address: address.trim().to_owned(),
            }
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.daemon.kill();
            let _ = self.daemon.wait();
        }
    }

    fn channel<T>() -> (Sender<T>, Receiver<T>) {
        #[cfg(not(feature = "tokio"))]
        return async_channel::unbounded();
        #[cfg(feature = "tokio")]
        return tokio::sync::mpsc::unbounded_channel();
    }

    /// Another process's connection, owning a name until dropped.
    struct Service {
        _stop: Sender<()>,
    }

    /// Owns `name` on the bus at `address` from its own thread and executor
    /// until the returned service is dropped or that bus goes away.
    fn serve(
        address: &str,
        name: &'static str,
        interfaces: impl FnOnce(Builder<'static>) -> zbus::Result<Builder<'static>> + Send + 'static,
    ) -> Service {
        let address = address.to_owned();
        let (ready_tx, ready) = mpsc::channel();
        let (stop, stopped) = channel::<()>();
        thread::spawn(move || {
            let executor = Executor::new();
            block_on(executor.run(async {
                let connection = async {
                    interfaces(Builder::address(address.as_str())?.name(name)?)?
                        .internal_executor(false)
                        .build()
                        .await
                };
                let connection = match connection.await {
                    Ok(connection) => connection,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                let connection_copy = connection.clone();
                let _task = executor.spawn(
                    async move {
                        loop {
                            connection_copy.executor().tick().await;
                        }
                    },
                    "test_service_task",
                );
                let _ = ready_tx.send(Ok(()));
                #[cfg(feature = "tokio")]
                let mut stopped = stopped;
                let stop = async {
                    let _ = stopped.recv().await;
                };
                stop.or(connection.closed()).await;
            }));
        });
        ready
            .recv_timeout(PATIENCE)
            .expect("test service did not start")
            .unwrap();
        Service { _stop: stop }
    }

    #[derive(Debug, PartialEq)]
    enum Observed {
        SessionConnect,
        AddressRequested,
        Activated,
        Deactivated,
    }

    /// The session-bus side of at-spi-bus-launcher while a screen reader runs.
    struct FakeLauncher {
        /// Requests still to be answered with an address that has no bus.
        missing_requests: AtomicUsize,
        missing_bus: String,
        atspi_bus: String,
        observed: mpsc::Sender<Observed>,
    }

    #[zbus::interface(name = "org.a11y.Bus")]
    impl FakeLauncher {
        fn get_address(&self) -> String {
            let _ = self.observed.send(Observed::AddressRequested);
            let missing = self
                .missing_requests
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .is_ok();
            if missing {
                self.missing_bus.clone()
            } else {
                self.atspi_bus.clone()
            }
        }
    }

    struct FakeStatus;

    #[zbus::interface(name = "org.a11y.Status")]
    impl FakeStatus {
        #[zbus(property)]
        fn is_enabled(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn screen_reader_enabled(&self) -> bool {
            true
        }
    }

    /// Serves a launcher on the session bus for the AT-SPI bus in `dir`.
    fn serve_launcher(
        dir: &TempDir,
        missing_requests: usize,
        observed: mpsc::Sender<Observed>,
    ) -> Service {
        let launcher = FakeLauncher {
            missing_requests: AtomicUsize::new(missing_requests),
            missing_bus: format!("unix:path={}", dir.path().join("missing-bus").display()),
            atspi_bus: format!("unix:path={}", dir.path().join("atspi-bus").display()),
            observed,
        };
        let session_bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap();
        serve(&session_bus, LAUNCHER, |builder| {
            builder
                .serve_at("/org/a11y/bus", launcher)?
                .serve_at("/org/a11y/bus", FakeStatus)
        })
    }

    /// The AT-SPI registry, which embeds each application's root.
    struct FakeRegistry;

    #[zbus::interface(name = "org.a11y.atspi.Socket")]
    impl FakeRegistry {
        fn embed(&self, plug: (String, OwnedObjectPath)) -> (String, OwnedObjectPath) {
            plug
        }
    }

    /// An AT-SPI bus with a registry on it; both stop when dropped.
    struct AtspiBus {
        _registry: Service,
        _bus: PrivateBus,
    }

    fn start_atspi_bus(socket: &Path) -> AtspiBus {
        let bus = PrivateBus::start(socket);
        let registry = serve(&bus.address, "org.a11y.atspi.Registry", |builder| {
            builder.serve_at("/org/a11y/atspi/accessible/root", FakeRegistry)
        });
        AtspiBus {
            _registry: registry,
            _bus: bus,
        }
    }

    struct ObservedActivation(mpsc::Sender<Observed>);

    impl ActivationHandler for ObservedActivation {
        fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
            let _ = self.0.send(Observed::Activated);
            None
        }
    }

    impl DeactivationHandler for ObservedActivation {
        fn deactivate_accessibility(&mut self) {
            let _ = self.0.send(Observed::Deactivated);
        }
    }

    struct NoActions;

    impl ActionHandler for NoActions {
        fn do_action(&mut self, _request: ActionRequest) {}
    }

    /// Runs the context with one adapter on its own thread and executor, using
    /// the production session connector. The receiver reports when the context
    /// returns; the handle reports whether it panicked.
    fn start_context(
        observed: &mpsc::Sender<Observed>,
    ) -> (Sender<Message>, thread::JoinHandle<()>, mpsc::Receiver<()>) {
        let (tx, rx) = channel();
        let (finished_tx, finished) = mpsc::channel();
        let connects = observed.clone();
        let context = thread::spawn(move || {
            let executor = Executor::new();
            block_on(executor.run(run_context(&executor, rx, move || {
                let _ = connects.send(Observed::SessionConnect);
                connect_session_bus()
            })));
            let _ = finished_tx.send(());
        });
        let adapter = Message::AddAdapter {
            id: 0,
            activation_handler: Box::new(ObservedActivation(observed.clone())),
            deactivation_handler: Box::new(ObservedActivation(observed.clone())),
            state: Arc::new(Mutex::new(AdapterState::Inactive {
                is_window_focused: false,
                root_window_bounds: Default::default(),
                action_handler: Arc::new(ActionHandlerWrapper::new(NoActions)),
            })),
        };
        #[cfg(not(feature = "tokio"))]
        tx.try_send(adapter).ok().unwrap();
        #[cfg(feature = "tokio")]
        tx.send(adapter).ok().unwrap();
        (tx, context, finished)
    }

    /// Waits for `expected`, skipping repeated connection attempts.
    fn expect(
        observed: &mpsc::Receiver<Observed>,
        context: &thread::JoinHandle<()>,
        expected: Observed,
    ) {
        loop {
            match observed.recv_timeout(PATIENCE) {
                Ok(event) if event == expected => return,
                Ok(Observed::SessionConnect | Observed::AddressRequested) => (),
                Ok(event) => panic!("expected {expected:?}, observed {event:?}"),
                Err(_) => panic!(
                    "{expected:?} not observed; context still running: {}",
                    !context.is_finished()
                ),
            }
        }
    }

    fn expect_shutdown(
        tx: Sender<Message>,
        context: thread::JoinHandle<()>,
        finished: mpsc::Receiver<()>,
    ) {
        drop(tx);
        finished
            .recv_timeout(PATIENCE)
            .expect("the context must stop once every message sender is dropped");
        context.join().expect("the context thread must not panic");
    }
}
