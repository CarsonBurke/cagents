//! Real registry wire fixture, not a compositor/Servo rendering substitute.
//! Only this owned test connection reads; production probing never reads.
use super::*;
use std::{
    os::fd::{AsFd, AsRawFd},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};
use wayland_protocols::wp::viewporter::server::wp_viewporter as server_viewporter;
use wayland_server::{
    Client, DataInit, Dispatch as ServerDispatch, Display, DisplayHandle as ServerHandle,
    GlobalDispatch, New, Resource,
    protocol::{
        wl_compositor as server_compositor, wl_region as server_region,
        wl_subcompositor as server_subcompositor, wl_surface as server_surface,
    },
};

struct Server;
macro_rules! global {
    ($ty:ty) => {
        impl GlobalDispatch<$ty, ()> for Server {
            fn bind(
                _: &mut Self,
                _: &ServerHandle,
                _: &Client,
                resource: New<$ty>,
                _: &(),
                init: &mut DataInit<'_, Self>,
            ) {
                init.init(resource, ());
            }
        }
    };
}
global!(server_compositor::WlCompositor);
global!(server_subcompositor::WlSubcompositor);
global!(server_viewporter::WpViewporter);
impl ServerDispatch<server_compositor::WlCompositor, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &server_compositor::WlCompositor,
        request: server_compositor::Request,
        _: &(),
        _: &ServerHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            server_compositor::Request::CreateSurface { id } => {
                init.init(id, ());
            }
            server_compositor::Request::CreateRegion { id } => {
                init.init(id, ());
            }
            _ => {}
        }
    }
}
macro_rules! no_requests {
    ($ty:ty) => {
        impl ServerDispatch<$ty, ()> for Server {
            fn request(
                _: &mut Self,
                _: &Client,
                _: &$ty,
                _: <$ty as Resource>::Request,
                _: &(),
                _: &ServerHandle,
                _: &mut DataInit<'_, Self>,
            ) {
            }
        }
    };
}
no_requests!(server_surface::WlSurface);
no_requests!(server_region::WlRegion);
no_requests!(server_subcompositor::WlSubcompositor);
no_requests!(server_viewporter::WpViewporter);

struct Fixture {
    display: Display<Server>,
    state: Server,
    connection: Connection,
    owner_queue: EventQueue<WaylandDispatch>,
    owner_state: WaylandDispatch,
    surface: wl_surface::WlSurface,
    globals: [wayland_server::backend::GlobalId; 3],
}
impl Fixture {
    fn new() -> Self {
        let mut display = Display::<Server>::new().unwrap();
        let mut handle = display.handle();
        let globals = [
            handle.create_global::<Server, server_compositor::WlCompositor, _>(1, ()),
            handle.create_global::<Server, server_subcompositor::WlSubcompositor, _>(1, ()),
            handle.create_global::<Server, server_viewporter::WpViewporter, _>(1, ()),
        ];
        let (client, server) = UnixStream::pair().unwrap();
        handle.insert_client(server, Arc::new(())).unwrap();
        let connection = Connection::from_socket(client).unwrap();
        let mut owner_queue = connection.new_event_queue();
        let qh = owner_queue.handle();
        let registry = connection.display().get_registry(&qh, ());
        let mut owner_state = WaylandDispatch::default();
        connection.display().sync(&qh, ());
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut state = Server;
        while !owner_state.complete {
            assert!(Instant::now() < deadline, "owned registry setup deadline");
            pump(&mut display, &mut state, &connection);
            owner_queue.dispatch_pending(&mut owner_state).unwrap();
        }
        let global = owner_state
            .globals
            .iter()
            .find(|g| g.interface == "wl_compositor")
            .unwrap();
        let compositor: wl_compositor::WlCompositor = registry.bind(global.name, 1, &qh, ());
        let surface = compositor.create_surface(&qh, ());
        owner_state.complete = false;
        connection.display().sync(&qh, ());
        while !owner_state.complete {
            assert!(Instant::now() < deadline, "owned parent setup deadline");
            pump(&mut display, &mut state, &connection);
            owner_queue.dispatch_pending(&mut owner_state).unwrap();
        }
        Self {
            display,
            state,
            connection,
            owner_queue,
            owner_state,
            surface,
            globals,
        }
    }
    fn factory(&self) -> RawWaylandSubsurfaceFactory {
        // This guard owns the actual display and surface produced by the
        // fixture, including if a tested factory Arc survives Fixture itself.
        let parent = Arc::new(Parent {
            connection: self.connection.clone(),
            surface: self.surface.clone(),
        });
        RawWaylandSubsurfaceFactory::from_parent(parent).unwrap()
    }
    fn pump(&mut self) {
        pump(&mut self.display, &mut self.state, &self.connection);
        self.owner_queue
            .dispatch_pending(&mut self.owner_state)
            .unwrap();
    }
    fn until_settled(&mut self, factory: &RawWaylandSubsurfaceFactory) -> CapabilityProbe {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let result = factory.poll_capability();
            if result != CapabilityProbe::Pending {
                return result;
            }
            assert!(Instant::now() < deadline, "private queue fixture deadline");
            self.pump();
        }
    }
}
struct Parent {
    connection: Connection,
    surface: wl_surface::WlSurface,
}
impl HasDisplayHandle for Parent {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, raw_window_handle::HandleError> {
        let pointer = NonNull::new(self.connection.backend().display_ptr().cast()).unwrap();
        // SAFETY: this borrow is backed by our owned client connection.
        Ok(unsafe { DisplayHandle::borrow_raw(WaylandDisplayHandle::new(pointer).into()) })
    }
}
impl HasWindowHandle for Parent {
    fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
        let pointer = NonNull::new(self.surface.id().as_ptr().cast()).unwrap();
        // SAFETY: this borrow is backed by our owned actual surface proxy.
        Ok(unsafe { WindowHandle::borrow_raw(WaylandWindowHandle::new(pointer).into()) })
    }
}
fn pump(display: &mut Display<Server>, state: &mut Server, connection: &Connection) {
    connection.flush().unwrap();
    display.dispatch_clients(state).unwrap();
    display.flush_clients().unwrap();
    if let Some(guard) = connection.prepare_read() {
        let mut fd = libc::pollfd {
            fd: connection.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: a single initialized pollfd; this test owns its connection
        // and has no competing reader, so readiness bounds the read below.
        let ready = unsafe { libc::poll(&mut fd, 1, 10) };
        assert!(ready >= 0);
        if ready > 0 {
            guard.read().unwrap();
        }
    }
}

#[test]
fn capable_registry_requires_both_syncs_then_latches_ready() {
    let mut fixture = Fixture::new();
    let factory = fixture.factory();
    assert_eq!(factory.poll_capability(), CapabilityProbe::Pending);
    fixture.pump();
    // The first sync identifies the globals; binding requests need their own
    // acknowledgment before an engine may register.
    assert_eq!(factory.poll_capability(), CapabilityProbe::Pending);
    fixture.pump();
    assert_eq!(fixture.until_settled(&factory), CapabilityProbe::Available);
    assert_eq!(factory.poll_capability(), CapabilityProbe::Available);
    factory.release_parent();
}

#[derive(Clone)]
struct ProtocolLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for ProtocolLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn protocol_logs(body: impl FnOnce()) -> String {
    let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = bytes.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || ProtocolLog(writer.clone()))
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    String::from_utf8(bytes.lock().unwrap().clone()).unwrap()
}

#[test]
fn protocol_diagnostics_distinguish_both_syncs_and_retain_parent_span() {
    let logs = protocol_logs(|| {
        let mut fixture = Fixture::new();
        let span = tracing::info_span!(
            target: "cagents::browser_capability", "browser_protocol_probe", generation = "fixture"
        );
        let factory = {
            let _entered = span.enter();
            fixture.factory()
        };
        assert_eq!(factory.poll_capability(), CapabilityProbe::Pending);
        fixture.pump();
        assert_eq!(factory.poll_capability(), CapabilityProbe::Pending);
        fixture.pump();
        assert_eq!(fixture.until_settled(&factory), CapabilityProbe::Available);
        factory.release_parent();
    });
    for phase in [
        "first_poll",
        "registry_sync_complete",
        "bound_sync_queued",
        "bound_sync_complete",
    ] {
        let records = logs
            .lines()
            .filter(|line| line.contains(&format!("phase=\"{phase}\"")))
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1, "{phase}: {logs}");
        assert!(records[0].contains("generation=\"fixture\""), "{logs}");
    }
    assert!(logs.contains("sync_stage=\"registry_sync\""), "{logs}");
    assert!(logs.contains("sync_stage=\"bound_sync\""), "{logs}");
}

#[test]
fn unread_owner_diagnostics_are_bounded_and_retirement_reports_pending_stage() {
    let logs = protocol_logs(|| {
        let fixture = Fixture::new();
        let factory = fixture.factory();
        for _ in 0..16 {
            assert_eq!(factory.poll_capability(), CapabilityProbe::Pending);
        }
        factory.release_parent();
        factory.release_parent();
    });
    let records = logs
        .lines()
        .filter(|line| line.contains("browser protocol capability progress"))
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 2, "{logs}");
    let retired = records
        .iter()
        .find(|line| line.contains("phase=\"retired\""))
        .unwrap();
    for field in [
        "sync_stage=\"registry_sync\"",
        "globals=0",
        "bound=false",
        "sync_complete=false",
        "polls=16",
        "dispatched=0",
        "flush_would_block=0",
    ] {
        assert!(retired.contains(field), "{field}: {logs}");
    }
}

#[test]
fn each_missing_global_is_exact_and_engine_registration_is_refused() {
    for index in 0..3 {
        let mut fixture = Fixture::new();
        fixture
            .display
            .handle()
            .disable_global::<Server>(fixture.globals[index].clone());
        let factory = Arc::new(fixture.factory());
        let result = fixture.until_settled(&factory);
        let CapabilityProbe::Unavailable(BrowserUnavailable::MissingProtocols(missing)) = result
        else {
            panic!("unexpected capability {result:?}");
        };
        assert_eq!(
            missing.protocols().collect::<Vec<_>>(),
            vec![crate::capability::RequiredWaylandProtocol::ALL[index]]
        );
        assert_eq!(factory.poll_capability(), result);
        let url = url::Url::parse("https://example.test/").unwrap();
        let restore = crate::browser::BrowserRestoreState {
            url: url.clone(),
            history: vec![url],
            history_index: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            scale_factor: 1.0,
            viewport: PhysicalViewport::new(200, 100),
        };
        assert!(
            matches!(crate::browser::BrowserHandle::spawn(restore, true, Some(factory.clone())),
            Err(BrowserError::Capability(BrowserUnavailable::MissingProtocols(actual))) if actual == missing)
        );
        factory.release_parent();
    }
}

#[test]
fn global_removed_after_announcement_is_not_bound() {
    let mut fixture = Fixture::new();
    let factory = fixture.factory();
    assert_eq!(factory.poll_capability(), CapabilityProbe::Pending);
    fixture.connection.flush().unwrap();
    fixture
        .display
        .dispatch_clients(&mut fixture.state)
        .unwrap();
    // These announcements and removal are all read before private dispatch.
    fixture
        .display
        .handle()
        .disable_global::<Server>(fixture.globals[2].clone());
    fixture.display.flush_clients().unwrap();
    fixture.pump();
    let CapabilityProbe::Unavailable(BrowserUnavailable::MissingProtocols(missing)) =
        factory.poll_capability()
    else {
        panic!()
    };
    assert_eq!(missing.to_string(), "wp_viewporter");
    factory.release_parent();
}

#[test]
fn unfinished_queue_is_nonreading_and_explicit_retirement_survives_arc_clone() {
    let fixture = Fixture::new();
    let factory = Arc::new(fixture.factory());
    let late_callback = factory.clone();
    // Do not service the server. Every poll must return, never roundtrip/read.
    for _ in 0..10 {
        assert_eq!(factory.poll_capability(), CapabilityProbe::Pending);
    }
    factory.release_parent();
    drop(factory);
    assert_eq!(
        late_callback.poll_capability(),
        CapabilityProbe::Unavailable(BrowserUnavailable::ParentUnavailable)
    );
    drop(fixture);
    // A late callback cannot access the now-destroyed protocol queue/parent.
    assert_eq!(
        late_callback.poll_capability(),
        CapabilityProbe::Unavailable(BrowserUnavailable::ParentUnavailable)
    );
}

impl RawWaylandSubsurfaceFactory {
    /// Test-only prepared protocol owner. Its safe constructor retains the
    /// actual client connection/surface; no application engine is registered.
    pub(crate) fn prepared_registry_fixture() -> Arc<Self> {
        let mut fixture = Fixture::new();
        let factory = Arc::new(fixture.factory());
        assert_eq!(fixture.until_settled(&factory), CapabilityProbe::Available);
        factory
    }
}
