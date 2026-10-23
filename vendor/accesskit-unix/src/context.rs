// Copyright 2023 The AccessKit Authors. All rights reserved.
// Licensed under the Apache License, Version 2.0 (found in
// the LICENSE-APACHE file) or the MIT license (found in
// the LICENSE-MIT file), at your option.

use accesskit::{ActivationHandler, DeactivationHandler};
use accesskit_atspi_common::{Adapter as AdapterImpl, AppContext, Event};
#[cfg(not(feature = "tokio"))]
use async_channel::{Receiver, Sender};
use atspi::proxy::bus::StatusProxy;
use futures_lite::{FutureExt, StreamExt, future};
use std::{
    fmt::Display,
    future::Future,
    io,
    sync::{Arc, Mutex, OnceLock, RwLock},
    thread,
    time::{Duration, Instant},
};
#[cfg(feature = "tokio")]
use tokio::sync::mpsc::{UnboundedReceiver as Receiver, UnboundedSender as Sender};
use tracing::{debug, warn};
use zbus::{
    Connection,
    connection::Builder,
    fdo::{DBusProxy, NameOwnerChangedStream},
    names::{BusName, OwnedUniqueName, WellKnownName},
    proxy::{CacheProperties, PropertyChanged, PropertyStream},
};

use crate::{
    adapter::{AdapterState, Callback, Message},
    atspi::Bus,
    executor::{Executor, Task, sleep_until},
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

/// Bounds each D-Bus request, and each attempt to establish a connection, so
/// that an unresponsive bus, launcher or registry cannot hold up the context.
/// Tests use a shorter bound to exercise it quickly.
pub(crate) const REQUEST_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(3)
} else {
    Duration::from_secs(10)
};

async fn connect_session_bus() -> zbus::Result<Connection> {
    Builder::session()?
        .method_timeout(REQUEST_TIMEOUT)
        .internal_executor(false)
        .build()
        .await
}

/// Fails with a timeout unless `future` completes within `REQUEST_TIMEOUT`.
async fn bounded<T>(future: impl Future<Output = zbus::Result<T>>) -> zbus::Result<T> {
    let timed_out = async {
        sleep_until(Instant::now() + REQUEST_TIMEOUT).await;
        Err(zbus::Error::InputOutput(Arc::new(
            io::ErrorKind::TimedOut.into(),
        )))
    };
    future.or(timed_out).await
}

/// Whether `error` means that the connection itself failed, rather than one
/// request on it.
fn ends_connection(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::InputOutput(_))
}

async fn receive(messages: &mut Receiver<Message>) -> Option<Message> {
    #[cfg(not(feature = "tokio"))]
    return messages.recv().await.ok();
    #[cfg(feature = "tokio")]
    return messages.recv().await;
}

/// Serves adapters until every message sender is dropped. Unreachable or lost
/// buses never end the context: adapters stay registered, are deactivated
/// while no AT-SPI bus is connected, and are activated again once one is.
async fn run_context<C, F>(executor: &Executor<'_>, messages: Receiver<Message>, connect_session: C)
where
    C: FnMut() -> F,
    F: Future<Output = zbus::Result<Connection>>,
{
    let mut context = Context {
        executor,
        messages,
        connect_session,
        session: None,
        session_reconnect: Reconnect::default(),
        atspi_bus: None,
        atspi_reconnect: Reconnect::default(),
        adapters: Vec::new(),
    };
    context.connect_session().await;
    loop {
        match context.next_event().await {
            ContextEvent::Message(Some(message)) => context.process_message(message).await,
            ContextEvent::Message(None) => return,
            ContextEvent::SessionClosed => context.session_lost().await,
            ContextEvent::LauncherOwnerChanged(owner) => {
                context.launcher_owner_changed(owner).await
            }
            ContextEvent::ScreenReaderChanged(change) => {
                context.screen_reader_changed(change).await
            }
            ContextEvent::AtspiBusClosed => context.atspi_bus_lost(&"connection closed").await,
            ContextEvent::ReconnectDue => context.reconnect().await,
        }
    }
}

/// What a due AT-SPI retry attempts.
enum AtspiRetry {
    WatchLauncher,
    ConnectBus,
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
    messages: Receiver<Message>,
    connect_session: C,
    session: Option<Session>,
    session_reconnect: Reconnect,
    atspi_bus: Option<Bus>,
    /// Paces retries of both the AT-SPI bus and the launcher that reports it.
    /// Its deadline outlasts the screen reader being disabled and the session
    /// bus being lost, and is only waited for while there is something to retry.
    atspi_reconnect: Reconnect,
    adapters: Vec<AdapterEntry>,
}

impl<C, F> Context<'_, '_, C>
where
    C: FnMut() -> F,
    F: Future<Output = zbus::Result<Connection>>,
{
    /// Waits for a message, for a bus to change state or for a reconnection
    /// attempt to be due.
    async fn next_event(&mut self) -> ContextEvent {
        let atspi_retry = self.atspi_retry().is_some();
        let messages = &mut self.messages;
        let message = async { ContextEvent::Message(receive(messages).await) };
        let Some(Session {
            connection,
            launcher_owner_changes,
            screen_reader_changes,
            ..
        }) = &mut self.session
        else {
            let session_reconnect = &self.session_reconnect;
            let reconnect_due = async {
                session_reconnect.due().await;
                ContextEvent::ReconnectDue
            };
            return reconnect_due.or(message).await;
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
            if atspi_retry {
                atspi_reconnect.due().await
            } else {
                future::pending().await
            }
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
            .or(message)
            .await
    }

    async fn connect_session(&mut self) {
        let connection = (self.connect_session)();
        let executor = self.executor;
        match bounded(async { Session::new(connection.await?, executor).await }).await {
            Ok(session) => {
                self.session = Some(session);
                self.session_reconnect.connected();
                self.watch_launcher().await;
            }
            Err(error) => self
                .session_reconnect
                .schedule("The D-Bus session bus", &error),
        }
    }

    async fn session_lost(&mut self) {
        self.session = None;
        self.session_reconnect.disconnected();
        self.disconnect_atspi_bus().await;
        self.session_reconnect
            .schedule("The D-Bus session bus", &"connection closed");
    }

    /// A new launcher runs a new AT-SPI bus, and a departed one runs none.
    async fn launcher_owner_changed(&mut self, owner: Option<OwnedUniqueName>) {
        let Some(session) = &mut self.session else {
            return;
        };
        if session.launcher_owner == owner {
            return;
        }
        session.launcher_owner = owner;
        session.forget_status();
        self.disconnect_atspi_bus().await;
        self.watch_launcher().await;
    }

    /// Watches the screen-reader status published by the launcher. Without a
    /// launcher, this may start one by D-Bus activation, as upstream does;
    /// failing that, the owner watch reports when one appears. A launcher that
    /// is running but does not answer is retried like the AT-SPI bus.
    async fn watch_launcher(&mut self) {
        let Some(session) = &mut self.session else {
            return;
        };
        if let Err(error) = session.watch_status().await {
            if session.launcher_owner.is_some() {
                self.atspi_reconnect
                    .schedule("The AT-SPI bus launcher", &error);
            } else {
                debug!(%error, "The AT-SPI bus launcher is not running");
            }
        }
    }

    async fn screen_reader_changed(&mut self, change: PropertyChanged<'static, bool>) {
        let enabled = match bounded(change.get()).await {
            Ok(enabled) => enabled,
            Err(error) => {
                // Invalidated properties need another D-Bus read. A failed
                // refresh is not an explicit disabled status: keep retrying
                // even if this launcher never emits another property change.
                self.disconnect_atspi_bus().await;
                if let Some(session) = &mut self.session {
                    session.forget_status();
                    if session.launcher_owner.is_some() {
                        self.atspi_reconnect
                            .schedule("The AT-SPI bus launcher", &error);
                    }
                }
                return;
            }
        };
        let Some(session) = &mut self.session else {
            return;
        };
        session.screen_reader_enabled = enabled;
        if !enabled {
            self.disconnect_atspi_bus().await;
        } else if self.atspi_bus.is_none() && !self.atspi_reconnect.is_scheduled() {
            // A pending retry stays pending, so neither a launcher that keeps
            // restarting nor a screen reader toggled off and on can bypass the
            // backoff.
            self.connect_atspi_bus().await;
        }
    }

    async fn connect_atspi_bus(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        match bounded(Bus::new(&session.connection, self.executor)).await {
            Ok(bus) => {
                self.atspi_bus = Some(bus);
                self.atspi_reconnect.connected();
                for entry in &mut self.adapters {
                    activate_adapter(entry);
                }
            }
            Err(error) => self.atspi_bus_lost(&error).await,
        }
    }

    async fn atspi_bus_lost(&mut self, error: &dyn Display) {
        self.disconnect_atspi_bus().await;
        self.atspi_reconnect.schedule("The AT-SPI bus", error);
    }

    /// Deactivates every adapter until an AT-SPI bus is connected again.
    async fn disconnect_atspi_bus(&mut self) {
        if self.atspi_bus.take().is_some() {
            self.atspi_reconnect.disconnected();
        }
        for entry in &mut self.adapters {
            deactivate_adapter(entry);
        }
        // Adapters that were active may have queued updates for the lost bus.
        // Activation registers each tree afresh, and an update applied first
        // would claim its nodes on the next bus for the discarded tree. So
        // process the queue as though no bus were connected, which keeps only
        // adapter additions and removals. Deactivated adapters queue nothing
        // more, so this ends.
        while let Ok(message) = self.messages.try_recv() {
            // Without a bus, processing cannot fail.
            let _ = process_adapter_message(&None, &mut self.adapters, message).await;
        }
    }

    async fn process_message(&mut self, message: Message) {
        let result = bounded(process_adapter_message(
            &self.atspi_bus,
            &mut self.adapters,
            message,
        ))
        .await;
        let Err(error) = result else {
            return;
        };
        if ends_connection(&error) || self.atspi_bus.as_ref().is_some_and(Bus::is_closed) {
            self.atspi_bus_lost(&error).await;
        } else {
            debug!(%error, "An AT-SPI request failed");
        }
    }

    /// What an AT-SPI retry would attempt now, if anything.
    fn atspi_retry(&self) -> Option<AtspiRetry> {
        let session = self.session.as_ref()?;
        if session.launcher_owner.is_some() && session.screen_reader_changes.is_none() {
            Some(AtspiRetry::WatchLauncher)
        } else if session.screen_reader_enabled && self.atspi_bus.is_none() {
            Some(AtspiRetry::ConnectBus)
        } else {
            None
        }
    }

    async fn reconnect(&mut self) {
        if self.session.is_none() {
            self.session_reconnect.cancel();
            return self.connect_session().await;
        }
        self.atspi_reconnect.cancel();
        match self.atspi_retry() {
            Some(AtspiRetry::WatchLauncher) => self.watch_launcher().await,
            Some(AtspiRetry::ConnectBus) => self.connect_atspi_bus().await,
            None => (),
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
        Ok(Self {
            connection,
            _task: task,
            launcher_owner_changes,
            launcher_owner,
            screen_reader_changes: None,
            screen_reader_enabled: false,
        })
    }

    async fn watch_status(&mut self) -> zbus::Result<()> {
        self.forget_status();
        let changes = bounded(async {
            let status = StatusProxy::builder(&self.connection)
                .cache_properties(CacheProperties::Yes)
                .build()
                .await?;
            Ok(status.receive_screen_reader_enabled_changed().await)
        })
        .await?;
        self.screen_reader_changes = Some(changes);
        Ok(())
    }

    fn forget_status(&mut self) {
        self.screen_reader_changes = None;
        self.screen_reader_enabled = false;
    }
}

/// Paces reconnection to one bus while it is unreachable. The first failure of
/// an outage is a warning and later ones are debug messages; attempts back off
/// exponentially to one every `MAX_DELAY`. An outage ends only once a
/// connection stays up for `MAX_DELAY`. Until then, disconnections, launcher
/// restarts and screen-reader toggles keep its backoff and warning, so a bus
/// that accepts connections and then drops them, or a launcher that keeps
/// restarting, is neither retried faster nor warned about again.
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

    /// Ends the outage if the connection that just ended stayed up long enough.
    fn disconnected(&mut self) {
        let connected_at = self.connected_at.take();
        if connected_at.is_some_and(|at| at.elapsed() >= Self::MAX_DELAY) {
            *self = Self::default();
        }
    }

    fn schedule(&mut self, bus: &str, error: &dyn Display) {
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

    fn is_scheduled(&self) -> bool {
        self.due.is_some()
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
    use accesskit::{ActionHandler, ActionRequest, Node, NodeId, Role, Tree, TreeId, TreeUpdate};
    use accesskit_atspi_common::ActionHandlerWrapper;
    use std::{
        fs::File,
        io::{BufRead, BufReader, Read},
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
    };
    use zbus::zvariant::{OwnedObjectPath, OwnedValue};

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

        // Cancelling a retry once it is due continues the outage.
        reconnect.cancel();
        assert!(!reconnect.is_scheduled());
        assert_eq!(reconnect.delay, Reconnect::MAX_DELAY);
        assert!(reconnect.warned);

        // So does a connection that drops at once.
        reconnect.connected();
        assert!(!reconnect.is_scheduled());
        reconnect.disconnected();
        reconnect.schedule("The test bus", &"closed");
        assert_eq!(reconnect.delay, Reconnect::MAX_DELAY);

        // One that stayed up ends the outage, so the next one warns again.
        reconnect.connected();
        reconnect.connected_at = Instant::now().checked_sub(Reconnect::MAX_DELAY);
        reconnect.disconnected();
        assert_eq!(reconnect.delay, Reconnect::FIRST_DELAY);
        assert!(!reconnect.is_scheduled());
        assert!(!reconnect.warned);
        reconnect.schedule("The test bus", &"closed");
        assert_eq!(reconnect.delay, 2 * Reconnect::FIRST_DELAY);
        assert!(reconnect.warned);
    }

    #[test]
    fn only_connection_failures_end_the_connection() {
        let timed_out = || zbus::Error::InputOutput(Arc::new(io::ErrorKind::TimedOut.into()));
        assert!(ends_connection(&timed_out()));
        assert!(ends_connection(&zbus::Error::InputOutput(Arc::new(
            io::ErrorKind::ConnectionReset.into()
        ))));
        assert!(!ends_connection(&zbus::Error::Failure(
            "one request".into()
        )));
        assert!(!ends_connection(
            &zbus::fdo::Error::UnknownObject("/".into()).into()
        ));
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
        let _session_bus = PrivateBus::start(&session_socket());
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

    #[test]
    fn unresponsive_registry_does_not_hold_up_the_context() {
        if !in_isolated_process(
            "unresponsive_registry_does_not_hold_up_the_context",
            SessionBus::Running,
        ) {
            return;
        }
        let dir = TempDir::new();
        let atspi_bus = PrivateBus::start(&dir.path().join("atspi-bus"));
        let registry = FakeRegistry {
            unanswered: AtomicUsize::new(1),
            ..Default::default()
        };
        let _registry = serve_registry(&atspi_bus.address, registry);
        let (observed_tx, observed) = mpsc::channel();
        let _launcher = serve_launcher(&dir, 0, observed_tx.clone());
        let (tx, context, finished) = start_context(&observed_tx);

        // The registry never answers the first request to embed the application.
        for expected in [
            Observed::SessionConnect,
            Observed::AddressRequested,
            Observed::AddressRequested,
            Observed::Activated,
        ] {
            assert_eq!(observed.recv_timeout(PATIENCE), Ok(expected));
        }

        expect_shutdown(tx, context, finished);
    }

    #[test]
    fn launcher_restarts_do_not_bypass_the_backoff() {
        if !in_isolated_process(
            "launcher_restarts_do_not_bypass_the_backoff",
            SessionBus::Running,
        ) {
            return;
        }
        let dir = TempDir::new();
        let (observed_tx, observed) = mpsc::channel();
        // No AT-SPI bus runs at the address that any launcher reports.
        let mut launcher = serve_launcher(&dir, usize::MAX, observed_tx.clone());
        let (tx, context, finished) = start_context(&observed_tx);
        for expected in [Observed::SessionConnect, Observed::AddressRequested] {
            assert_eq!(observed.recv_timeout(PATIENCE), Ok(expected));
        }

        let mut failed_at = Instant::now();
        for backoff in [Reconnect::FIRST_DELAY, 2 * Reconnect::FIRST_DELAY] {
            // The replacement launcher is noticed at once, but its bus is
            // tried only when the retry is due.
            let _replaced = std::mem::replace(
                &mut launcher,
                serve_launcher(&dir, usize::MAX, observed_tx.clone()),
            );
            assert_eq!(
                observed.recv_timeout(PATIENCE),
                Ok(Observed::AddressRequested)
            );
            let retried_after = failed_at.elapsed();
            assert!(
                retried_after >= backoff * 4 / 5,
                "retried after {retried_after:?}, within the {backoff:?} backoff"
            );
            failed_at = Instant::now();
        }

        expect_shutdown(tx, context, finished);
    }

    #[test]
    fn screen_reader_toggles_do_not_bypass_the_backoff() {
        if !in_isolated_process(
            "screen_reader_toggles_do_not_bypass_the_backoff",
            SessionBus::Running,
        ) {
            return;
        }
        let dir = TempDir::new();
        let (observed_tx, observed) = mpsc::channel();
        // No AT-SPI bus runs at the address that the launcher reports.
        let _launcher = serve_launcher(&dir, usize::MAX, observed_tx.clone());
        let (tx, context, finished) = start_context(&observed_tx);
        for expected in [Observed::SessionConnect, Observed::AddressRequested] {
            assert_eq!(observed.recv_timeout(PATIENCE), Ok(expected));
        }

        let mut failed_at = Instant::now();
        for backoff in [Reconnect::FIRST_DELAY, 2 * Reconnect::FIRST_DELAY] {
            // Re-enabling the screen reader retries the bus only when the
            // retry is due.
            set_screen_reader_enabled(false);
            thread::sleep(backoff / 4);
            set_screen_reader_enabled(true);
            assert_eq!(
                observed.recv_timeout(PATIENCE),
                Ok(Observed::AddressRequested)
            );
            let retried_after = failed_at.elapsed();
            assert!(
                retried_after >= backoff * 4 / 5,
                "retried after {retried_after:?}, within the {backoff:?} backoff"
            );
            failed_at = Instant::now();
        }

        expect_shutdown(tx, context, finished);
    }

    #[test]
    fn unresponsive_launcher_status_is_retried() {
        if !in_isolated_process(
            "unresponsive_launcher_status_is_retried",
            SessionBus::Running,
        ) {
            return;
        }
        let dir = TempDir::new();
        let _atspi_bus = start_atspi_bus(&dir.path().join("atspi-bus"));
        let (observed_tx, observed) = mpsc::channel();
        // The launcher never answers the first read of the screen-reader status.
        let _launcher = serve_launcher_with(&dir, 0, 1, observed_tx.clone());
        let (tx, context, finished) = start_context(&observed_tx);
        for expected in [
            Observed::SessionConnect,
            Observed::AddressRequested,
            Observed::Activated,
        ] {
            assert_eq!(observed.recv_timeout(PATIENCE), Ok(expected));
        }

        expect_shutdown(tx, context, finished);
    }

    #[test]
    fn unreadable_invalidated_status_is_retried() {
        if !in_isolated_process(
            "unreadable_invalidated_status_is_retried",
            SessionBus::Running,
        ) {
            return;
        }
        let dir = TempDir::new();
        let _atspi_bus = start_atspi_bus(&dir.path().join("atspi-bus"));
        let (observed_tx, observed) = mpsc::channel();
        let _launcher = serve_launcher(&dir, 0, observed_tx.clone());
        let (tx, context, finished) = start_context(&observed_tx);
        expect(&observed, &context, Observed::Activated);

        let session_bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap();
        with_client(&session_bus, |connection| async move {
            connection
                .call_method(
                    Some(LAUNCHER),
                    "/org/a11y/bus",
                    Some("org.a11y.Status"),
                    "InvalidateScreenReaderStatus",
                    &(),
                )
                .await
                .unwrap();
        });
        expect(&observed, &context, Observed::Deactivated);
        // The launcher stays alive and emits no further status changes. The
        // one failed refresh must recover on its own, without restarting UI.
        expect(&observed, &context, Observed::Activated);
        expect_shutdown(tx, context, finished);
    }

    #[test]
    fn session_bus_loss_is_followed() {
        if !in_isolated_process("session_bus_loss_is_followed", SessionBus::Missing) {
            return;
        }
        let dir = TempDir::new();
        let session_bus = PrivateBus::start(&session_socket());
        let _atspi_bus = start_atspi_bus(&dir.path().join("atspi-bus"));
        let (observed_tx, observed) = mpsc::channel();
        let launcher = serve_launcher(&dir, 0, observed_tx.clone());
        let (tx, context, finished) = start_context(&observed_tx);
        expect(&observed, &context, Observed::Activated);

        // The AT-SPI bus outlives the session bus, so only the closed session
        // connection can tell the context to deactivate.
        drop(session_bus);
        expect(&observed, &context, Observed::Deactivated);
        drop(launcher);
        let _ = std::fs::remove_file(session_socket());
        let _session_bus = PrivateBus::start(&session_socket());
        let _launcher = serve_launcher(&dir, 0, observed_tx);
        expect(&observed, &context, Observed::Activated);

        expect_shutdown(tx, context, finished);
    }

    #[test]
    fn launcher_flapping_does_not_leak() {
        if !in_isolated_process("launcher_flapping_does_not_leak", SessionBus::Running) {
            return;
        }
        let dir = TempDir::new();
        let _atspi_bus = start_atspi_bus(&dir.path().join("atspi-bus"));
        let (observed_tx, observed) = mpsc::channel();
        let mut launcher = Some(serve_launcher(&dir, 0, observed_tx.clone()));
        let (tx, context, finished) = start_context(&observed_tx);
        expect(&observed, &context, Observed::Activated);

        let count = |dir: &str| std::fs::read_dir(dir).unwrap().count();
        let mut flap = |cycles: usize| {
            for _ in 0..cycles {
                drop(launcher.take());
                expect(&observed, &context, Observed::Deactivated);
                launcher = Some(serve_launcher(&dir, 0, observed_tx.clone()));
                expect(&observed, &context, Observed::Activated);
            }
            // Let the departed launchers' threads finish.
            thread::sleep(Duration::from_millis(500));
            (count("/proc/self/fd"), count("/proc/self/task"))
        };
        let (warm_fds, warm_threads) = flap(3);
        let (fds, threads) = flap(40);
        assert!(
            fds <= warm_fds + 8 && threads <= warm_threads + 4,
            "40 launcher restarts grew descriptors {warm_fds} -> {fds} and threads {warm_threads} -> {threads}"
        );

        drop(launcher);
        expect_shutdown(tx, context, finished);
    }

    /// A replaced launcher is noticed at once, while an adapter may still be
    /// updating the tree it registered on the old AT-SPI bus. Those updates
    /// must not reach the new bus ahead of the tree registered there.
    #[test]
    fn updates_for_a_lost_bus_do_not_reach_the_next_one() {
        if !in_isolated_process(
            "updates_for_a_lost_bus_do_not_reach_the_next_one",
            SessionBus::Running,
        ) {
            return;
        }
        let dir = TempDir::new();
        let atspi_bus = PrivateBus::start(&dir.path().join("atspi-bus"));
        let (embedded_tx, embedded) = mpsc::channel();
        let _registry = serve_registry(&atspi_bus.address, FakeRegistry::reporting(embedded_tx));
        let (unobserved, _) = mpsc::channel();
        let _launcher = serve_launcher(&dir, 0, unobserved.clone());

        let (lifecycle_tx, lifecycle) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let extended = Arc::new(AtomicBool::new(false));
        let _a = window_adapter("A", &Arc::default(), &lifecycle_tx, Some(gate));
        let mut b = window_adapter("B", &extended, &lifecycle_tx, None);
        for expected in [Lifecycle::Activated("A"), Lifecycle::Activated("B")] {
            assert_eq!(lifecycle.recv_timeout(PATIENCE), Ok(expected));
        }

        // The context deactivates A first and is held there, with B still active.
        let _replacement = serve_launcher(&dir, 0, unobserved);
        assert_eq!(
            lifecycle.recv_timeout(PATIENCE),
            Ok(Lifecycle::Deactivated("A"))
        );
        extended.store(true, Ordering::SeqCst);
        b.update_if_active(|| window("B", true));
        release.send(()).unwrap();
        for expected in [
            Lifecycle::Deactivated("B"),
            Lifecycle::Activated("A"),
            Lifecycle::Activated("B"),
        ] {
            assert_eq!(lifecycle.recv_timeout(PATIENCE), Ok(expected));
        }

        // Each tree was registered afresh on the new connection, and every
        // node registered there answers for the current tree.
        embedded.recv_timeout(PATIENCE).unwrap();
        let app = embedded.recv_timeout(PATIENCE).unwrap();
        let expected = [vec!["A", "two"], vec!["B", "three", "two"]];
        let deadline = Instant::now() + PATIENCE;
        loop {
            let names = accessible_names(&atspi_bus.address, &app);
            if names == expected {
                break;
            }
            assert!(Instant::now() < deadline, "registered nodes: {names:?}");
            thread::sleep(Duration::from_millis(100));
        }
    }

    #[derive(Debug, PartialEq)]
    enum Lifecycle {
        Activated(&'static str),
        Deactivated(&'static str),
    }

    /// A window labelled `label` with a button "two" and, if `extended`, "three".
    fn window(label: &str, extended: bool) -> TreeUpdate {
        let button = |label: &str| {
            let mut node = Node::new(Role::Button);
            node.set_label(label);
            node
        };
        let mut nodes = vec![(NodeId(2), button("two"))];
        if extended {
            nodes.push((NodeId(3), button("three")));
        }
        let mut root = Node::new(Role::Window);
        root.set_label(label);
        root.set_children(nodes.iter().map(|(id, _)| *id).collect::<Vec<_>>());
        nodes.insert(0, (NodeId(1), root));
        TreeUpdate {
            nodes,
            tree: Some(Tree::new(NodeId(1))),
            tree_id: TreeId::ROOT,
            focus: NodeId(1),
        }
    }

    /// The handlers of a window that reports its lifecycle. A gated window's
    /// first deactivation waits until the gate is released.
    struct Window {
        label: &'static str,
        extended: Arc<AtomicBool>,
        lifecycle: mpsc::Sender<Lifecycle>,
        gate: Option<mpsc::Receiver<()>>,
    }

    impl ActivationHandler for Window {
        fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
            let _ = self.lifecycle.send(Lifecycle::Activated(self.label));
            Some(window(self.label, self.extended.load(Ordering::SeqCst)))
        }
    }

    impl DeactivationHandler for Window {
        fn deactivate_accessibility(&mut self) {
            let _ = self.lifecycle.send(Lifecycle::Deactivated(self.label));
            if let Some(gate) = self.gate.take() {
                gate.recv_timeout(PATIENCE).unwrap();
            }
        }
    }

    /// A production adapter, served by the process-wide context.
    fn window_adapter(
        label: &'static str,
        extended: &Arc<AtomicBool>,
        lifecycle: &mpsc::Sender<Lifecycle>,
        gate: Option<mpsc::Receiver<()>>,
    ) -> crate::Adapter {
        let window = |gate| Window {
            label,
            extended: Arc::clone(extended),
            lifecycle: lifecycle.clone(),
            gate,
        };
        crate::Adapter::new(window(None), NoActions, window(gate))
    }

    /// The names of the nodes `app` registered on the bus at `address`, by
    /// adapter in ascending order and sorted within each.
    fn accessible_names(address: &str, app: &str) -> Vec<Vec<String>> {
        const ACCESSIBLE: &str = "/org/a11y/atspi/accessible";
        with_client(address, |connection| async move {
            let children = |path: String| {
                let connection = &connection;
                async move {
                    let reply = connection
                        .call_method(
                            Some(app),
                            path.as_str(),
                            Some("org.freedesktop.DBus.Introspectable"),
                            "Introspect",
                            &(),
                        )
                        .await
                        .unwrap();
                    let xml: String = reply.body().deserialize().unwrap();
                    xml.split("<node name=\"")
                        .skip(1)
                        .filter_map(|node| Some(node.split_once('"')?.0.to_owned()))
                        .collect::<Vec<_>>()
                }
            };
            let mut adapters: Vec<usize> = children(ACCESSIBLE.into())
                .await
                .iter()
                .filter_map(|child| child.parse().ok())
                .collect();
            adapters.sort();
            let mut names = Vec::new();
            for adapter in adapters {
                let mut adapter_names = Vec::new();
                for node in children(format!("{ACCESSIBLE}/{adapter}")).await {
                    let path = format!("{ACCESSIBLE}/{adapter}/{node}");
                    let reply = connection
                        .call_method(
                            Some(app),
                            path.as_str(),
                            Some("org.freedesktop.DBus.Properties"),
                            "Get",
                            &("org.a11y.atspi.Accessible", "Name"),
                        )
                        .await;
                    adapter_names.push(match reply {
                        Ok(reply) => match reply.body().deserialize::<OwnedValue>() {
                            Ok(name) => String::try_from(name).unwrap_or_default(),
                            Err(error) => format!("{path}: {error}"),
                        },
                        Err(error) => format!("{path}: {error}"),
                    });
                }
                adapter_names.sort();
                names.push(adapter_names);
            }
            names
        })
    }

    /// Runs `client` with a new connection to the bus at `address`, from this thread.
    fn with_client<T, F: Future<Output = T>>(
        address: &str,
        client: impl FnOnce(Connection) -> F,
    ) -> T {
        let executor = Executor::new();
        block_on(executor.run(async {
            let connection = Builder::address(address)
                .unwrap()
                .internal_executor(false)
                .build()
                .await
                .unwrap();
            let connection_copy = connection.clone();
            let _task = executor.spawn(
                async move {
                    loop {
                        connection_copy.executor().tick().await;
                    }
                },
                "test_client_task",
            );
            client(connection).await
        }))
    }

    /// Sets the screen-reader status that the launcher publishes, as a
    /// desktop's accessibility setting does.
    fn set_screen_reader_enabled(enabled: bool) {
        let session_bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap();
        with_client(&session_bus, |connection| async move {
            connection
                .call_method(
                    Some(LAUNCHER),
                    "/org/a11y/bus",
                    Some("org.freedesktop.DBus.Properties"),
                    "Set",
                    &(
                        "org.a11y.Status",
                        "ScreenReaderEnabled",
                        zbus::zvariant::Value::from(enabled),
                    ),
                )
                .await
                .unwrap();
        })
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
    /// inside that process. Without `dbus-daemon`, the scenario is skipped.
    fn in_isolated_process(test: &str, session_bus: SessionBus) -> bool {
        if std::env::var_os(ISOLATED).is_some() {
            return true;
        }
        let dbus_daemon = Command::new("dbus-daemon")
            .arg("--version")
            .stdout(Stdio::null())
            .status();
        if !dbus_daemon.is_ok_and(|status| status.success()) {
            eprintln!("skipping `{test}`: it needs dbus-daemon");
            return false;
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
        let status = killed_with_spawner(&mut Command::new(std::env::current_exe().unwrap()))
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

    /// The few C library calls that the harness needs; declared here because
    /// the crate does not depend on `libc`.
    mod sys {
        use std::ffi::{c_int, c_ulong};

        pub const PR_SET_PDEATHSIG: c_int = 1;
        pub const SIGKILL: c_ulong = 9;
        pub const LOCK_EX: c_int = 2;
        pub const LOCK_NB: c_int = 4;

        extern "C" {
            pub fn prctl(option: c_int, ...) -> c_int;
            pub fn getppid() -> c_int;
            pub fn _exit(status: c_int) -> !;
            pub fn flock(fd: c_int, operation: c_int) -> c_int;
        }
    }

    /// Makes `command`'s process receive `SIGKILL` when the thread that
    /// spawned it ends, so that a test process that is killed leaves no bus
    /// or isolated test process behind. Each is spawned and reaped by the same
    /// test thread, which waits in `spawn` until the program has started, so
    /// only the death of the whole spawning process can come before the
    /// signal is armed; the child then exits at once.
    fn killed_with_spawner(command: &mut Command) -> &mut Command {
        use std::os::unix::process::CommandExt;
        let spawner = std::ffi::c_int::try_from(std::process::id()).unwrap();
        // SAFETY: prctl, getppid and _exit are async-signal-safe, and only
        // the child is affected.
        unsafe {
            command.pre_exec(move || {
                if sys::prctl(sys::PR_SET_PDEATHSIG, sys::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if sys::getppid() != spawner {
                    sys::_exit(1);
                }
                Ok(())
            })
        }
    }

    /// Takes an exclusive lock on `file` if no one holds one.
    fn try_lock(file: &File) -> bool {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor stays open for the duration of the call.
        unsafe { sys::flock(file.as_raw_fd(), sys::LOCK_EX | sys::LOCK_NB) == 0 }
    }

    /// A directory short enough to hold Unix socket paths. It keeps a lock on
    /// the `lock` file inside for as long as it exists, which tells test
    /// processes in any PID namespace that it is in use.
    struct TempDir {
        path: PathBuf,
        _lock: File,
    }

    impl TempDir {
        const PREFIX: &'static str = "accesskit-test-";
        const LOCK: &'static str = "lock";

        /// The first directory in a process removes those whose test process
        /// died. Each is locked before it gets a name that the removal
        /// considers, so it is never seen unlocked while in use.
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            static SWEEP: std::sync::Once = std::sync::Once::new();
            SWEEP.call_once(Self::remove_abandoned);
            loop {
                let index = NEXT.fetch_add(1, Ordering::Relaxed);
                let name = format!("{}{}-{index}", Self::PREFIX, std::process::id());
                let staging = std::env::temp_dir().join(format!(".{name}"));
                match std::fs::create_dir(&staging) {
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    result => result.unwrap(),
                }
                let lock = File::create(staging.join(Self::LOCK)).unwrap();
                assert!(try_lock(&lock));
                let path = std::env::temp_dir().join(name);
                // Fails if a run in another PID namespace has the name.
                match std::fs::rename(&staging, &path) {
                    Ok(()) => return Self { path, _lock: lock },
                    Err(_) => {
                        let _ = std::fs::remove_dir_all(&staging);
                    }
                }
            }
        }

        fn remove_abandoned() {
            let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
                return;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                if !name
                    .to_str()
                    .is_some_and(|name| name.starts_with(Self::PREFIX))
                {
                    continue;
                }
                let Ok(lock) = File::open(entry.path().join(Self::LOCK)) else {
                    continue;
                };
                if try_lock(&lock) {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // The lock is released only afterwards, when the file is closed.
            let _ = std::fs::remove_dir_all(&self.path);
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
            let mut daemon = killed_with_spawner(&mut Command::new("dbus-daemon"))
                .arg(format!("--config-file={}", config.display()))
                .args(["--nofork", "--nopidfile", "--nosyslog", "--print-address"])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
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

    /// Counts down `remaining`, returning whether it was positive.
    fn take_one(remaining: &AtomicUsize) -> bool {
        remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
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
            if take_one(&self.missing_requests) {
                self.missing_bus.clone()
            } else {
                self.atspi_bus.clone()
            }
        }
    }

    struct FakeStatus {
        screen_reader_enabled: bool,
        /// Reads of the screen-reader status still to be left unanswered.
        unanswered: AtomicUsize,
        unreadable: AtomicUsize,
    }

    #[zbus::interface(name = "org.a11y.Status")]
    impl FakeStatus {
        #[zbus(property)]
        fn is_enabled(&self) -> bool {
            true
        }

        #[zbus(property)]
        async fn screen_reader_enabled(&self) -> zbus::fdo::Result<bool> {
            if take_one(&self.unanswered) {
                future::pending::<()>().await;
            }
            if take_one(&self.unreadable) {
                return Err(zbus::fdo::Error::Failed("temporary status failure".into()));
            }
            Ok(self.screen_reader_enabled)
        }

        async fn invalidate_screen_reader_status(
            &self,
            #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        ) -> zbus::fdo::Result<()> {
            self.unreadable.store(1, Ordering::Relaxed);
            emitter
                .emit(
                    "org.freedesktop.DBus.Properties",
                    "PropertiesChanged",
                    &(
                        "org.a11y.Status",
                        std::collections::HashMap::<&str, zbus::zvariant::Value<'_>>::new(),
                        vec!["ScreenReaderEnabled"],
                    ),
                )
                .await
                .map_err(zbus::fdo::Error::from)
        }

        #[zbus(property)]
        fn set_screen_reader_enabled(&mut self, enabled: bool) {
            self.screen_reader_enabled = enabled;
        }
    }

    /// The socket of the session bus that this isolated process was given.
    fn session_socket() -> PathBuf {
        let address = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap();
        PathBuf::from(address.strip_prefix("unix:path=").unwrap())
    }

    /// Serves a launcher on the session bus for the AT-SPI bus in `dir`. Like a
    /// restarted at-spi-bus-launcher, it takes the name from any running one.
    fn serve_launcher(
        dir: &TempDir,
        missing_requests: usize,
        observed: mpsc::Sender<Observed>,
    ) -> Service {
        serve_launcher_with(dir, missing_requests, 0, observed)
    }

    fn serve_launcher_with(
        dir: &TempDir,
        missing_requests: usize,
        unanswered_status_reads: usize,
        observed: mpsc::Sender<Observed>,
    ) -> Service {
        let status = FakeStatus {
            screen_reader_enabled: true,
            unanswered: AtomicUsize::new(unanswered_status_reads),
            unreadable: AtomicUsize::new(0),
        };
        let launcher = FakeLauncher {
            missing_requests: AtomicUsize::new(missing_requests),
            missing_bus: format!("unix:path={}", dir.path().join("missing-bus").display()),
            atspi_bus: format!("unix:path={}", dir.path().join("atspi-bus").display()),
            observed,
        };
        let session_bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap();
        serve(&session_bus, LAUNCHER, |builder| {
            builder
                .allow_name_replacements(true)
                .replace_existing_names(true)
                .serve_at("/org/a11y/bus", launcher)?
                .serve_at("/org/a11y/bus", status)
        })
    }

    /// The AT-SPI registry, which embeds each application's root.
    #[derive(Default)]
    struct FakeRegistry {
        /// Receives the bus name of each embedded application.
        embedded: Option<mpsc::Sender<String>>,
        /// Requests still to be left unanswered.
        unanswered: AtomicUsize,
    }

    impl FakeRegistry {
        fn reporting(embedded: mpsc::Sender<String>) -> Self {
            Self {
                embedded: Some(embedded),
                ..Default::default()
            }
        }
    }

    #[zbus::interface(name = "org.a11y.atspi.Socket")]
    impl FakeRegistry {
        async fn embed(&self, plug: (String, OwnedObjectPath)) -> (String, OwnedObjectPath) {
            if take_one(&self.unanswered) {
                future::pending::<()>().await;
            }
            if let Some(embedded) = &self.embedded {
                let _ = embedded.send(plug.0.clone());
            }
            plug
        }
    }

    fn serve_registry(address: &str, registry: FakeRegistry) -> Service {
        serve(address, "org.a11y.atspi.Registry", |builder| {
            builder.serve_at("/org/a11y/atspi/accessible/root", registry)
        })
    }

    /// An AT-SPI bus with a registry on it; both stop when dropped.
    struct AtspiBus {
        _registry: Service,
        _bus: PrivateBus,
    }

    fn start_atspi_bus(socket: &Path) -> AtspiBus {
        let bus = PrivateBus::start(socket);
        let registry = serve_registry(&bus.address, FakeRegistry::default());
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
