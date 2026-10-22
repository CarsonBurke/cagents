//! Seat capability lifecycle regressions for the clipboard worker.
//!
//! An in-process compositor speaks the Wayland protocol to a real libwayland
//! client connection over a socket pair. Each server command is flushed as one
//! batch, so the worker reads and queues it together, as it would when a
//! compositor removes an input device while that device still has events in
//! flight.

use std::collections::VecDeque;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sctk::reexports::client::globals::{registry_queue_init, GlobalListContents};
use sctk::reexports::client::protocol::{
    wl_compositor as client_compositor, wl_registry, wl_surface as client_surface,
};
use sctk::reexports::client::{delegate_noop, Connection, EventQueue};
use smithay_clipboard::SimpleClipboard;
use wayland_server::backend::{ClientData, GlobalId};
use wayland_server::protocol::wl_keyboard::{self, KeyState};
use wayland_server::protocol::wl_pointer::{self, ButtonState};
use wayland_server::protocol::wl_seat::{self, Capability};
use wayland_server::protocol::{
    wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_source, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource,
};

const TIMEOUT: Duration = Duration::from_secs(5);
const BTN_LEFT: u32 = 0x110;
const KEY_C: u32 = 46;

/// The worker released its pointer, then dispatched a frame already queued for
/// it. The frame carries a press, so it reaches the clipboard's handler.
#[test]
fn pointer_frame_queued_after_pointer_release() {
    let mut session = Session::start();
    session.run(|compositor| compositor.keyboard_enter(1));
    session.run(|compositor| {
        compositor.pointer_enter(2);
        compositor.press(3);
        compositor.capabilities(Capability::Keyboard);
        compositor.frame();
    });
    session.observe(Observed::PointerReleased);

    session.assert_copy_after_key(4);

    // The pointer returns and its presses can authorize selections again.
    session.run(|compositor| compositor.capabilities(Capability::Pointer | Capability::Keyboard));
    session.observe(Observed::PointerCreated);
    session.run(|compositor| {
        compositor.pointer_enter(5);
        compositor.press(6);
        compositor.frame();
    });
    session.assert_copy(6);
}

/// A leave queued after the release is delivered without a frame: the rebuilt
/// proxy of the destroyed pointer reports version 0, below frame support.
#[test]
fn pointer_leave_queued_after_pointer_release() {
    let mut session = Session::start();
    session.run(|compositor| {
        compositor.keyboard_enter(1);
        compositor.pointer_enter(2);
        compositor.frame();
    });
    session.run(|compositor| {
        compositor.capabilities(Capability::Keyboard);
        compositor.pointer_leave(3);
        compositor.frame();
    });
    session.observe(Observed::PointerReleased);

    session.assert_copy_after_key(4);
}

/// The compositor removed the seat, then reported a capability change that was
/// already queued for it.
#[test]
fn capabilities_queued_after_seat_removal() {
    let mut session = Session::start();
    session.run(|compositor| {
        compositor.remove_seat();
        compositor.capabilities(Capability::Keyboard);
    });
    // Dropping the seat's state releases its devices.
    session.observe(Observed::PointerReleased);
    session.observe(Observed::KeyboardReleased);
    session.barrier();
}

/// As above, but the queued change adds a capability to the removed seat.
#[test]
fn capability_added_after_seat_removal() {
    let mut session = Session::start();
    session.run(|compositor| compositor.capabilities(Capability::Pointer));
    session.observe(Observed::KeyboardReleased);
    session.run(|compositor| {
        compositor.remove_seat();
        compositor.capabilities(Capability::Pointer | Capability::Keyboard);
    });
    session.observe(Observed::PointerReleased);
    session.barrier();
}

/// Copy from a pointer press after the keyboard, and with it the data device,
/// went away while the client had keyboard focus.
#[test]
fn copy_after_keyboard_release() {
    let mut session = Session::start();
    session.run(|compositor| compositor.keyboard_enter(1));
    session.run(|compositor| compositor.capabilities(Capability::Pointer));
    session.observe(Observed::KeyboardReleased);
    session.run(|compositor| {
        compositor.pointer_enter(2);
        compositor.press(3);
        compositor.frame();
    });
    session.barrier();
    session.assert_store_refused("without a data device");

    session.restore_keyboard();
    session.run(|compositor| compositor.keyboard_enter(4));
    session.assert_copy_after_key(5);
}

/// Releasing a focused keyboard ends its focus.
#[test]
fn keyboard_release_ends_focus() {
    let mut session = Session::start();
    session.run(|compositor| compositor.keyboard_enter(1));
    session.run(|compositor| compositor.capabilities(Capability::Pointer));
    session.assert_released_keyboard_cannot_authorize_copy();
}

/// An enter queued for a keyboard after its release does not grant focus.
#[test]
fn keyboard_enter_queued_after_keyboard_release() {
    let mut session = Session::start();
    session.run(|compositor| {
        compositor.capabilities(Capability::Pointer);
        compositor.keyboard_enter(1);
    });
    session.assert_released_keyboard_cannot_authorize_copy();
}

struct Session {
    clipboard: Option<SimpleClipboard>,
    server: Server,
    // The window toolkit owns the focused surface; the clipboard shares its display.
    _surface: client_surface::WlSurface,
    _queue: EventQueue<SurfaceOwner>,
    _connection: Connection,
}

impl Session {
    fn start() -> Self {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        let mut server = Server::start(server_socket);
        let connection =
            Connection::from_socket(client_socket).expect("libwayland-client is required");

        let (globals, mut queue) = registry_queue_init::<SurfaceOwner>(&connection).unwrap();
        let compositor: client_compositor::WlCompositor =
            globals.bind(&queue.handle(), 1..=6, ()).unwrap();
        let surface = compositor.create_surface(&queue.handle(), ());
        queue.roundtrip(&mut SurfaceOwner).unwrap();
        server.observe(Observed::SurfaceCreated);

        // SAFETY: the connection outlives the clipboard; `Drop` releases it first.
        let clipboard = unsafe { SimpleClipboard::new(connection.backend().display_ptr().cast()) };
        server.observe(Observed::PointerCreated);
        server.observe(Observed::KeyboardCreated);
        server.observe(Observed::DataDeviceCreated);

        Self {
            clipboard: Some(clipboard),
            server,
            _surface: surface,
            _queue: queue,
            _connection: connection,
        }
    }

    fn clipboard(&self) -> &SimpleClipboard {
        self.clipboard.as_ref().unwrap()
    }

    fn run(&self, command: impl FnOnce(&mut Compositor) + Send + 'static) {
        self.server.run(command);
    }

    fn observe(&mut self, wanted: Observed) {
        self.server.observe(wanted);
    }

    fn barrier(&mut self) {
        self.server.barrier();
    }

    fn restore_keyboard(&mut self) {
        self.run(|compositor| compositor.capabilities(Capability::Pointer | Capability::Keyboard));
        self.observe(Observed::KeyboardCreated);
        self.observe(Observed::DataDeviceCreated);
    }

    /// Ctrl+C on the focused keyboard replaces the clipboard with its serial.
    fn assert_copy_after_key(&mut self, serial: u32) {
        self.run(move |compositor| compositor.key(serial));
        self.assert_copy(serial);
    }

    /// The next selection the client sets is this copy, authorized by `serial`.
    fn assert_copy(&mut self, serial: u32) {
        self.barrier();
        let text = format!("copied with serial {serial}");
        self.clipboard().store_text(text);
        let (selection_serial, mime_types) = self.server.next_selection();
        assert_eq!(selection_serial, serial, "selection authorized by another input event");
        assert!(mime_types.iter().any(|mime| mime == "text/plain;charset=utf-8"), "{mime_types:?}");
    }

    /// Copy while the seat cannot authorize a selection.
    ///
    /// The worker handles requests in order, so the load's answer proves the
    /// store was handled before any later Wayland event. A live worker refuses
    /// the load for lack of keyboard focus.
    fn assert_store_refused(&self, text: &str) {
        self.clipboard().store_text(text);
        let refusal = self.clipboard().load_text().expect_err("loaded without keyboard focus");
        assert_eq!(refusal.to_string(), "client doesn't have focus");
    }

    /// The keyboard was released; a new keyboard arrives without focus, so a
    /// pointer press must not set a selection until that keyboard enters.
    fn assert_released_keyboard_cannot_authorize_copy(&mut self) {
        self.observe(Observed::KeyboardReleased);
        self.restore_keyboard();
        self.run(|compositor| {
            compositor.pointer_enter(2);
            compositor.press(3);
            compositor.frame();
        });
        self.barrier();
        self.assert_store_refused("without keyboard focus");

        self.run(|compositor| compositor.keyboard_enter(4));
        self.assert_copy_after_key(5);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Join the worker before its display connection closes.
        drop(self.clipboard.take());
    }
}

struct SurfaceOwner;

impl sctk::reexports::client::Dispatch<wl_registry::WlRegistry, GlobalListContents>
    for SurfaceOwner
{
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &sctk::reexports::client::QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(SurfaceOwner: client_compositor::WlCompositor);
delegate_noop!(SurfaceOwner: ignore client_surface::WlSurface);

#[derive(Debug, PartialEq)]
enum Observed {
    SurfaceCreated,
    PointerCreated,
    PointerReleased,
    KeyboardCreated,
    KeyboardReleased,
    DataDeviceCreated,
    BarrierBound(u32),
    SelectionSet { serial: u32, mime_types: Vec<String> },
}

type Command = Box<dyn FnOnce(&mut Compositor) + Send>;

struct Server {
    commands: Option<Sender<Command>>,
    observed: Receiver<Observed>,
    unclaimed: VecDeque<Observed>,
    barriers: u32,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start(socket: UnixStream) -> Self {
        let (commands, command_rx) = mpsc::channel::<Command>();
        let (observed_tx, observed) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut display = Display::<Compositor>::new().unwrap();
            let mut handle = display.handle();
            handle.create_global::<Compositor, wl_compositor::WlCompositor, ()>(6, ());
            handle.create_global::<Compositor, wl_shm::WlShm, ()>(1, ());
            handle.create_global::<Compositor, wl_data_device_manager::WlDataDeviceManager, ()>(
                3,
                (),
            );
            let seat_global = handle
                .create_global::<Compositor, wl_seat::WlSeat, SeatGlobal>(7, SeatGlobal::Primary);
            handle.insert_client(socket, Arc::new(TestClient)).unwrap();

            let mut compositor = Compositor {
                display: handle,
                observed: observed_tx,
                seat_global,
                seat: None,
                pointer: None,
                keyboard: None,
                surface: None,
            };
            loop {
                loop {
                    match command_rx.try_recv() {
                        Ok(command) => command(&mut compositor),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
                display.dispatch_clients(&mut compositor).unwrap();
                // Everything one command sent leaves in this single flush.
                display.flush_clients().unwrap();
                let mut poll = libc::pollfd {
                    fd: display.backend().poll_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                unsafe { libc::poll(&mut poll, 1, 5) };
            }
        });
        Self {
            commands: Some(commands),
            observed,
            unclaimed: VecDeque::new(),
            barriers: 0,
            thread: Some(thread),
        }
    }

    fn run(&self, command: impl FnOnce(&mut Compositor) + Send + 'static) {
        let command: Command = Box::new(command);
        self.commands.as_ref().unwrap().send(command).expect("compositor thread stopped");
    }

    fn observe(&mut self, wanted: Observed) {
        let description = format!("{wanted:?}");
        self.claim(&description, |observed| *observed == wanted);
    }

    /// Wait until the worker has dispatched every event sent so far.
    ///
    /// The worker binds each newly advertised seat while dispatching its event
    /// queue in order, so the bind follows all earlier events.
    fn barrier(&mut self) {
        self.barriers += 1;
        let barrier = self.barriers;
        self.run(move |compositor| {
            compositor.display.create_global::<Compositor, wl_seat::WlSeat, SeatGlobal>(
                7,
                SeatGlobal::Barrier(barrier),
            );
        });
        self.claim("the clipboard worker to keep dispatching events", |observed| {
            *observed == Observed::BarrierBound(barrier)
        });
    }

    fn next_selection(&mut self) -> (u32, Vec<String>) {
        let selection = |observed: &Observed| matches!(observed, Observed::SelectionSet { .. });
        match self.claim("a selection", selection) {
            Observed::SelectionSet { serial, mime_types } => (serial, mime_types),
            _ => unreachable!(),
        }
    }

    /// Take the earliest matching observation, keeping the others in order.
    fn claim(&mut self, description: &str, wanted: impl Fn(&Observed) -> bool) -> Observed {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(index) = self.unclaimed.iter().position(&wanted) {
                return self.unclaimed.remove(index).unwrap();
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.observed.recv_timeout(remaining) {
                Ok(observed) => self.unclaimed.push_back(observed),
                Err(_) => {
                    panic!("timed out waiting for {description}; unclaimed: {:?}", self.unclaimed)
                },
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        drop(self.commands.take());
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() && !thread::panicking() {
                panic!("compositor thread panicked");
            }
        }
    }
}

struct TestClient;

impl ClientData for TestClient {}

enum SeatGlobal {
    Primary,
    Barrier(u32),
}

struct Compositor {
    display: DisplayHandle,
    observed: Sender<Observed>,
    seat_global: GlobalId,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    surface: Option<wl_surface::WlSurface>,
}

impl Compositor {
    fn observe(&self, observed: Observed) {
        let _ = self.observed.send(observed);
    }

    fn surface(&self) -> &wl_surface::WlSurface {
        self.surface.as_ref().expect("no surface")
    }

    fn pointer(&self) -> &wl_pointer::WlPointer {
        self.pointer.as_ref().expect("the worker has no pointer")
    }

    fn keyboard(&self) -> &wl_keyboard::WlKeyboard {
        self.keyboard.as_ref().expect("the worker has no keyboard")
    }

    fn remove_seat(&self) {
        self.display.remove_global::<Self>(self.seat_global.clone());
    }

    fn capabilities(&self, capabilities: Capability) {
        self.seat.as_ref().expect("the worker has no seat").capabilities(capabilities);
    }

    fn pointer_enter(&self, serial: u32) {
        self.pointer().enter(serial, self.surface(), 1.0, 1.0);
    }

    fn pointer_leave(&self, serial: u32) {
        self.pointer().leave(serial, self.surface());
    }

    fn press(&self, serial: u32) {
        self.pointer().button(serial, 0, BTN_LEFT, ButtonState::Pressed);
    }

    fn frame(&self) {
        self.pointer().frame();
    }

    fn keyboard_enter(&self, serial: u32) {
        self.keyboard().enter(serial, self.surface(), Vec::new());
    }

    fn key(&self, serial: u32) {
        self.keyboard().key(serial, 0, KEY_C, KeyState::Pressed);
    }
}

impl GlobalDispatch<wl_seat::WlSeat, SeatGlobal> for Compositor {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_seat::WlSeat>,
        global: &SeatGlobal,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let seat = data_init.init(resource, ());
        match global {
            SeatGlobal::Primary => {
                seat.capabilities(Capability::Pointer | Capability::Keyboard);
                state.seat = Some(seat);
            },
            SeatGlobal::Barrier(barrier) => state.observe(Observed::BarrierBound(*barrier)),
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_seat::WlSeat,
        request: wl_seat::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_seat::Request::GetPointer { id } => {
                state.pointer = Some(data_init.init(id, ()));
                state.observe(Observed::PointerCreated);
            },
            wl_seat::Request::GetKeyboard { id } => {
                state.keyboard = Some(data_init.init(id, ()));
                state.observe(Observed::KeyboardCreated);
            },
            wl_seat::Request::GetTouch { .. } => panic!("touch was never advertised"),
            _ => (),
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        pointer: &wl_pointer::WlPointer,
        request: wl_pointer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_pointer::Request::Release = request {
            if state.pointer.as_ref() == Some(pointer) {
                state.pointer = None;
            }
            state.observe(Observed::PointerReleased);
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        keyboard: &wl_keyboard::WlKeyboard,
        request: wl_keyboard::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_keyboard::Request::Release = request {
            if state.keyboard.as_ref() == Some(keyboard) {
                state.keyboard = None;
            }
            state.observe(Observed::KeyboardReleased);
        }
    }
}

impl GlobalDispatch<wl_compositor::WlCompositor, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_compositor::WlCompositor>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                let surface = data_init.init(id, ());
                if state.surface.is_none() {
                    state.surface = Some(surface);
                    state.observe(Observed::SurfaceCreated);
                }
            },
            _ => panic!("unexpected compositor request"),
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_surface::WlSurface,
        _: wl_surface::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<wl_shm::WlShm, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_shm::WlShm>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wl_shm::WlShm, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm::WlShm,
        request: wl_shm::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, .. } = request {
            data_init.init(id, ());
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm_pool::Request::CreateBuffer { id, .. } = request {
            data_init.init(id, ());
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<wl_data_device_manager::WlDataDeviceManager, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_data_device_manager::WlDataDeviceManager>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_data_device_manager::WlDataDeviceManager,
        request: wl_data_device_manager::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_device_manager::Request::CreateDataSource { id } => {
                data_init.init(id, Mutex::new(Vec::new()));
            },
            wl_data_device_manager::Request::GetDataDevice { id, .. } => {
                data_init.init(id, ());
                state.observe(Observed::DataDeviceCreated);
            },
            _ => panic!("unexpected data device manager request"),
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, Mutex<Vec<String>>> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_data_source::WlDataSource,
        request: wl_data_source::Request,
        mime_types: &Mutex<Vec<String>>,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_data_source::Request::Offer { mime_type } = request {
            mime_types.lock().unwrap().push(mime_type);
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_data_device::WlDataDevice,
        request: wl_data_device::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_data_device::Request::SetSelection { source, serial } = request {
            let mime_types = source
                .and_then(|source| {
                    source.data::<Mutex<Vec<String>>>().map(|types| types.lock().unwrap().clone())
                })
                .unwrap_or_default();
            state.observe(Observed::SelectionSet { serial, mime_types });
        }
    }
}
