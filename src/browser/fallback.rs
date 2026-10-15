use std::{
    any::Any,
    cell::Cell,
    ffi::c_void,
    ptr::NonNull,
    rc::Rc,
    sync::{Arc, Mutex},
};

use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WaylandDisplayHandle, WaylandWindowHandle, WindowHandle,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    backend::{Backend, ObjectId},
    delegate_noop,
    globals::Global,
    protocol::{
        wl_callback, wl_compositor, wl_region, wl_registry, wl_subcompositor, wl_subsurface,
        wl_surface,
    },
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};

use super::{BrowserError, PhysicalViewport, SurfacePlacement};
use crate::capability::{BrowserUnavailable, MissingWaylandProtocols};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityProbe {
    Pending,
    Available,
    Unavailable(BrowserUnavailable),
}

enum FactoryHost {
    Uninitialized,
    Checking(WaylandPreparation),
    Ready(Arc<WaylandHost>),
    Unavailable(BrowserUnavailable),
    Retired,
}

/// Owner of a Wayland child `wl_surface`/`wl_subsurface` created in an
/// externally-rendered mode. The implementation must suppress iced swapchain
/// creation for this child, drive the child's own commits without ever
/// committing the parent, and destroy the native child only after Servo's
/// rendering context is dropped.
pub trait WaylandSubsurfaceLease: HasDisplayHandle + HasWindowHandle {
    /// Positions the child and clips the region of Servo's buffer it presents.
    fn set_geometry(&self, placement: SurfacePlacement) -> Result<(), BrowserError>;

    fn set_visible(&self, visible: bool) -> Result<(), BrowserError>;

    /// Latches viewport state that must be applied atomically with the buffer
    /// the rendering context is about to attach. `buffer` is that buffer's
    /// physical size: a `wp_viewport` source rectangle may never leave the
    /// attached buffer, so a grown source rectangle can only be committed
    /// together with the grown buffer.
    fn before_present(&self, buffer: PhysicalViewport) -> Result<(), BrowserError>;

    /// Drains queued events and flushes this child's protocol traffic.
    fn flush(&self) -> Result<(), BrowserError>;

    /// Dispatch events already read from the shared Wayland connection into
    /// this lease's private queue. Implementations without a queue may ignore
    /// this operation.
    fn dispatch_pending(&self) -> Result<(), BrowserError> {
        Ok(())
    }
}

/// Factory implemented by the libcosmic Wayland host. Creation happens on the
/// isolated browser thread so the resulting native window and Servo GL context
/// share one owner thread for their complete lifetime.
pub trait WaylandSubsurfaceFactory: Send + Sync + 'static {
    /// Engines may register only after this factory has a prepared host.
    fn availability(&self) -> CapabilityProbe;
    fn create(
        &self,
        viewport: PhysicalViewport,
        scale_factor: f32,
    ) -> Result<Box<dyn WaylandSubsurfaceLease>, BrowserError>;
}

/// Production factory for embedding Servo into a child of an existing Wayland
/// `wl_surface` supplied through raw-window-handle 0.6.
///
/// A retained lifetime guard travels with every lease. Protocol globals are prepared in bounded parent UI callbacks before any
/// browser engine starts. The prepared host is then reused across hibernation
/// and restoration cycles. Parent teardown explicitly clears its event queue.
pub struct RawWaylandSubsurfaceFactory {
    display: usize,
    parent_surface: usize,
    lifetime_guard: Arc<dyn Any + Send + Sync>,
    host: Mutex<FactoryHost>,
}

impl std::fmt::Debug for RawWaylandSubsurfaceFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RawWaylandSubsurfaceFactory")
            .field("display", &format_args!("{:#x}", self.display))
            .field(
                "parent_surface",
                &format_args!("{:#x}", self.parent_surface),
            )
            .finish_non_exhaustive()
    }
}

impl RawWaylandSubsurfaceFactory {
    /// Builds a factory from a shared parent that exposes raw-window-handle
    /// 0.6 handles. Keeping the `Arc` in the factory and every live lease makes
    /// this the preferred safe constructor for libcosmic/winit window owners.
    pub fn from_parent<P>(parent: Arc<P>) -> Result<Self, BrowserError>
    where
        P: HasDisplayHandle + HasWindowHandle + Any + Send + Sync + 'static,
    {
        let display = parent
            .display_handle()
            .map_err(|error| BrowserError::Wayland(format!("parent display handle: {error}")))?
            .as_raw();
        let window = parent
            .window_handle()
            .map_err(|error| BrowserError::Wayland(format!("parent window handle: {error}")))?
            .as_raw();
        let lifetime_guard: Arc<dyn Any + Send + Sync> = parent;

        // SAFETY: the retained Arc is the object which produced both borrowed
        // handles, so it keeps their display and surface alive.
        unsafe { Self::from_raw_handles(display, window, lifetime_guard) }
    }

    /// Builds a factory from copied raw-window-handle 0.6 handles and a guard
    /// retained by the factory and every lease.
    ///
    /// # Safety
    ///
    /// The display and surface must belong to the same Wayland client
    /// connection and remain valid until every lease/browser thread is gone
    /// and `release_parent` explicitly clears the factory protocol state (or
    /// the factory is dropped). Retained factory references after release can
    /// compare identities but cannot recreate or access protocol objects. `lifetime_guard` must either own the
    /// parent directly or be a parent-identified, stateful token paired with an
    /// external owner whose Drop synchronously joins all browser threads before
    /// destroying the parent. An unrelated inert token such as `Arc<()>` does
    /// not satisfy this contract.
    pub unsafe fn from_raw_handles(
        display: RawDisplayHandle,
        window: RawWindowHandle,
        lifetime_guard: Arc<dyn Any + Send + Sync>,
    ) -> Result<Self, BrowserError> {
        let RawDisplayHandle::Wayland(display) = display else {
            return Err(BrowserError::Capability(BrowserUnavailable::NotWayland));
        };
        let RawWindowHandle::Wayland(window) = window else {
            return Err(BrowserError::Capability(BrowserUnavailable::NotWayland));
        };

        Ok(Self {
            display: display.display.as_ptr() as usize,
            parent_surface: window.surface.as_ptr() as usize,
            lifetime_guard,
            host: Mutex::new(FactoryHost::Uninitialized),
        })
    }

    pub fn matches_parent(&self, display: RawDisplayHandle, window: RawWindowHandle) -> bool {
        matches!((display, window), (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(window))
            if display.display.as_ptr() as usize == self.display
                && window.surface.as_ptr() as usize == self.parent_surface)
    }

    /// Run only in the parent's serialized UI callbacks. This never reads the
    /// display: Winit owns reading, and this private queue only dispatches
    /// events it has already received. No worker owns an unfinished probe.
    pub fn poll_capability(&self) -> CapabilityProbe {
        let Ok(mut slot) = self.host.lock() else {
            return CapabilityProbe::Unavailable(BrowserUnavailable::InitializationFailed);
        };
        if matches!(*slot, FactoryHost::Uninitialized) {
            match WaylandPreparation::new(self.display, self.parent_surface) {
                Ok(preparation) => *slot = FactoryHost::Checking(preparation),
                Err(reason) => *slot = FactoryHost::Unavailable(reason),
            }
        }
        let ready = match &mut *slot {
            FactoryHost::Ready(_) => return CapabilityProbe::Available,
            FactoryHost::Unavailable(reason) => return CapabilityProbe::Unavailable(*reason),
            FactoryHost::Retired => {
                return CapabilityProbe::Unavailable(BrowserUnavailable::ParentUnavailable);
            }
            FactoryHost::Checking(preparation) => preparation.poll(),
            FactoryHost::Uninitialized => unreachable!(),
        };
        match ready {
            Ok(false) => CapabilityProbe::Pending,
            Ok(true) => {
                let FactoryHost::Checking(preparation) =
                    std::mem::replace(&mut *slot, FactoryHost::Retired)
                else {
                    unreachable!()
                };
                match preparation.finish(self.display, self.lifetime_guard.clone()) {
                    Ok(host) => {
                        *slot = FactoryHost::Ready(Arc::new(host));
                        CapabilityProbe::Available
                    }
                    Err(reason) => {
                        *slot = FactoryHost::Unavailable(reason);
                        CapabilityProbe::Unavailable(reason)
                    }
                }
            }
            Err(reason) => {
                *slot = FactoryHost::Unavailable(reason);
                CapabilityProbe::Unavailable(reason)
            }
        }
    }

    /// Explicitly release every foreign protocol object before the parent
    /// dies, even when a stale UI closure retains an Arc to this factory.
    /// Browser leases must have been joined/dropped first.
    pub fn release_parent(&self) {
        if let Ok(mut slot) = self.host.lock() {
            *slot = FactoryHost::Retired;
        }
    }

    fn host(&self) -> Result<Arc<WaylandHost>, BrowserError> {
        let slot = self
            .host
            .lock()
            .map_err(|_| BrowserError::Capability(BrowserUnavailable::InitializationFailed))?;
        match &*slot {
            FactoryHost::Ready(host) => Ok(host.clone()),
            FactoryHost::Unavailable(reason) => Err(BrowserError::Capability(*reason)),
            _ => Err(BrowserError::Capability(
                BrowserUnavailable::ParentUnavailable,
            )),
        }
    }
}

impl WaylandSubsurfaceFactory for RawWaylandSubsurfaceFactory {
    fn availability(&self) -> CapabilityProbe {
        let Ok(slot) = self.host.lock() else {
            return CapabilityProbe::Unavailable(BrowserUnavailable::InitializationFailed);
        };
        match &*slot {
            FactoryHost::Ready(_) => CapabilityProbe::Available,
            FactoryHost::Unavailable(reason) => CapabilityProbe::Unavailable(*reason),
            FactoryHost::Retired => {
                CapabilityProbe::Unavailable(BrowserUnavailable::ParentUnavailable)
            }
            _ => CapabilityProbe::Pending,
        }
    }
    fn create(
        &self,
        viewport: PhysicalViewport,
        scale_factor: f32,
    ) -> Result<Box<dyn WaylandSubsurfaceLease>, BrowserError> {
        Ok(Box::new(WaylandLease::create(
            self.host()?,
            viewport,
            scale_factor,
        )?))
    }
}

#[derive(Default)]
struct WaylandDispatch {
    globals: Vec<Global>,
    complete: bool,
}
impl Dispatch<wl_registry::WlRegistry, ()> for WaylandDispatch {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => state.globals.push(Global {
                name,
                interface,
                version,
            }),
            wl_registry::Event::GlobalRemove { name } => {
                state.globals.retain(|global| global.name != name)
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for WaylandDispatch {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            state.complete = true;
        }
    }
}

struct BoundGlobals {
    compositor: wl_compositor::WlCompositor,
    subcompositor: wl_subcompositor::WlSubcompositor,
    viewporter: wp_viewporter::WpViewporter,
}
struct WaylandPreparation {
    connection: Connection,
    queue: EventQueue<WaylandDispatch>,
    state: WaylandDispatch,
    registry: wl_registry::WlRegistry,
    _sync: wl_callback::WlCallback,
    parent: wl_surface::WlSurface,
    bound: Option<BoundGlobals>,
}
impl WaylandPreparation {
    fn new(display: usize, parent_surface: usize) -> Result<Self, BrowserUnavailable> {
        // SAFETY: the caller runs inside a live parent's UI callback; the app
        // retains and explicitly releases this foreign queue before the owner.
        let backend = unsafe { Backend::from_foreign_display((display as *mut c_void).cast()) };
        let connection = Connection::from_backend(backend);
        let parent_id = unsafe {
            ObjectId::from_ptr(
                wl_surface::WlSurface::interface(),
                (parent_surface as *mut c_void).cast(),
            )
        }
        .map_err(|_| BrowserUnavailable::ParentUnavailable)?;
        let parent = wl_surface::WlSurface::from_id(&connection, parent_id)
            .map_err(|_| BrowserUnavailable::ParentUnavailable)?;
        let queue = connection.new_event_queue();
        let handle = queue.handle();
        let registry = connection.display().get_registry(&handle, ());
        let sync = connection.display().sync(&handle, ());
        Ok(Self {
            connection,
            queue,
            state: WaylandDispatch::default(),
            registry,
            _sync: sync,
            parent,
            bound: None,
        })
    }
    fn poll(&mut self) -> Result<bool, BrowserUnavailable> {
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|_| BrowserUnavailable::InitializationFailed)?;
        if self.state.complete && self.bound.is_none() {
            self.bind_required()?;
        }
        match self.connection.flush() {
            Ok(()) => {}
            Err(wayland_client::backend::WaylandError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return Err(BrowserUnavailable::InitializationFailed),
        }
        Ok(self.state.complete)
    }
    fn bind_required(&mut self) -> Result<(), BrowserUnavailable> {
        let missing = MissingWaylandProtocols::from_advertised(
            self.state
                .globals
                .iter()
                .map(|global| (global.interface.as_str(), global.version)),
        );
        if !missing.is_empty() {
            return Err(BrowserUnavailable::MissingProtocols(missing));
        }
        let handle = self.queue.handle();
        let bind = |name: &str, maximum: u32| {
            self.state
                .globals
                .iter()
                .find(|global| global.interface == name && global.version >= 1)
                .map(|global| (global.name, global.version.min(maximum)))
                .ok_or(BrowserUnavailable::InitializationFailed)
        };
        let (name, version) = bind("wl_compositor", 6)?;
        let compositor: wl_compositor::WlCompositor =
            self.registry.bind(name, version, &handle, ());
        let (name, version) = bind("wl_subcompositor", 1)?;
        let subcompositor: wl_subcompositor::WlSubcompositor =
            self.registry.bind(name, version, &handle, ());
        let (name, version) = bind("wp_viewporter", 1)?;
        let viewporter: wp_viewporter::WpViewporter =
            self.registry.bind(name, version, &handle, ());
        if [compositor.id(), subcompositor.id(), viewporter.id()]
            .iter()
            .any(ObjectId::is_null)
        {
            return Err(BrowserUnavailable::InitializationFailed);
        }
        self.bound = Some(BoundGlobals {
            compositor,
            subcompositor,
            viewporter,
        });
        self.state.complete = false;
        self._sync = self.connection.display().sync(&handle, ());
        Ok(())
    }
    fn finish(
        self,
        display: usize,
        lifetime_guard: Arc<dyn Any + Send + Sync>,
    ) -> Result<WaylandHost, BrowserUnavailable> {
        let bound = self.bound.ok_or(BrowserUnavailable::InitializationFailed)?;
        Ok(WaylandHost {
            display,
            connection: self.connection,
            dispatch: Mutex::new((self.queue, self.state)),
            compositor: bound.compositor,
            subcompositor: bound.subcompositor,
            viewporter: bound.viewporter,
            parent: self.parent,
            _registry: self.registry,
            _lifetime_guard: lifetime_guard,
        })
    }
}

delegate_noop!(WaylandDispatch: ignore wl_compositor::WlCompositor);
delegate_noop!(WaylandDispatch: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(WaylandDispatch: ignore wl_surface::WlSurface);
delegate_noop!(WaylandDispatch: ignore wl_region::WlRegion);
delegate_noop!(WaylandDispatch: ignore wl_subsurface::WlSubsurface);
delegate_noop!(WaylandDispatch: ignore wp_viewporter::WpViewporter);
delegate_noop!(WaylandDispatch: ignore wp_viewport::WpViewport);

struct WaylandHost {
    display: usize,
    connection: Connection,
    dispatch: Mutex<(EventQueue<WaylandDispatch>, WaylandDispatch)>,
    compositor: wl_compositor::WlCompositor,
    subcompositor: wl_subcompositor::WlSubcompositor,
    viewporter: wp_viewporter::WpViewporter,
    parent: wl_surface::WlSurface,
    _registry: wl_registry::WlRegistry,
    _lifetime_guard: Arc<dyn Any + Send + Sync>,
}

impl WaylandHost {
    fn dispatch_pending(&self) -> Result<(), BrowserError> {
        let mut dispatch = self
            .dispatch
            .lock()
            .map_err(|_| BrowserError::Wayland("Wayland event queue lock was poisoned".into()))?;
        let (event_queue, state) = &mut *dispatch;
        event_queue
            .dispatch_pending(state)
            .map_err(|error| BrowserError::Wayland(format!("dispatch child events: {error}")))?;
        Ok(())
    }

    fn flush(&self) -> Result<(), BrowserError> {
        self.connection
            .flush()
            .map_err(|error| BrowserError::Wayland(format!("flush child surface: {error}")))
    }
}

struct WaylandLease {
    host: Arc<WaylandHost>,
    surface: wl_surface::WlSurface,
    subsurface: wl_subsurface::WlSubsurface,
    viewport: wp_viewport::WpViewport,
    visible: Cell<bool>,
    /// Last requested placement. Re-latched whenever the attached buffer grows
    /// enough for its source rectangle to fit.
    placement: Cell<SurfacePlacement>,
    /// Physical size of the buffer the compositor currently holds for this
    /// child. `wp_viewport` raises `out_of_buffer` when a committed source
    /// rectangle leaves the attached buffer, and the buffer only reaches a new
    /// size once Servo has actually presented at it.
    attached: Cell<(u32, u32)>,
    /// Protocol state already sent, so a pan that only moves the card does not
    /// re-send an unchanged source rectangle or destination every frame.
    latched_position: Cell<Option<(i32, i32)>>,
    latched_destination: Cell<Option<(i32, i32)>>,
    latched_source: Cell<Option<(u32, u32, u32, u32)>>,
}

impl WaylandLease {
    fn create(
        host: Arc<WaylandHost>,
        viewport_size: PhysicalViewport,
        scale_factor: f32,
    ) -> Result<Self, BrowserError> {
        let queue = host
            .dispatch
            .lock()
            .map_err(|_| BrowserError::Wayland("Wayland event queue lock was poisoned".into()))?
            .0
            .handle();
        let surface = host.compositor.create_surface(&queue, ());
        if surface.id().is_null() {
            return Err(BrowserError::Wayland(
                "wl_compositor could not create the Servo child surface".into(),
            ));
        }
        let subsurface = host
            .subcompositor
            .get_subsurface(&surface, &host.parent, &queue, ());
        if subsurface.id().is_null() {
            surface.destroy();
            return Err(BrowserError::Wayland(
                "wl_subcompositor could not create the Servo child subsurface".into(),
            ));
        }
        let viewport = host.viewporter.get_viewport(&surface, &queue, ());
        if viewport.id().is_null() {
            subsurface.destroy();
            surface.destroy();
            return Err(BrowserError::Wayland(
                "wp_viewporter could not create the Servo child viewport".into(),
            ));
        }

        // A wl_surface's default input region is infinite, so the compositor
        // would deliver pointer and touch enter to this child. Its connection
        // wrapper binds no wl_seat and delegate_noop!s every interface, so
        // those events would be dropped instead of reaching the iced widget.
        // An empty region keeps all input on libcosmic's parent surface, where
        // iced routes it through BrowserWidget::update.
        let region = host.compositor.create_region(&queue, ());
        if region.id().is_null() {
            viewport.destroy();
            subsurface.destroy();
            surface.destroy();
            return Err(BrowserError::Wayland(
                "wl_compositor could not create the Servo child input region".into(),
            ));
        }
        surface.set_input_region(Some(&region));
        // set_input_region has copy semantics, so the region object is dead
        // weight from here on.
        region.destroy();

        // Desynchronize the child. A synchronized subsurface only presents when
        // the parent commits, which would force browser threads to commit
        // libcosmic's own toplevel and thereby present whatever iced/winit has
        // staged for its next frame. The browser is not required to be
        // pixel-synchronised with iced: it has its own paint cadence, so its
        // buffer and viewport state apply on its own commit.
        //
        // Tradeoff: wl_subsurface.set_position is parent state in either mode,
        // so a card that moves lags the parent by a frame during a window
        // resize. That is acceptable because a card only moves as a result of
        // an iced relayout, which is itself followed by an iced parent commit.
        subsurface.set_desync();

        // A unit buffer scale plus a viewport destination preserves the exact
        // physical Servo buffer while supporting fractional logical scaling.
        if surface.version() >= 3 {
            surface.set_buffer_scale(1);
        }
        let lease = Self {
            host,
            surface,
            subsurface,
            viewport,
            visible: Cell::new(true),
            placement: Cell::new(SurfacePlacement::unclipped(viewport_size, scale_factor)),
            attached: Cell::new((viewport_size.width, viewport_size.height)),
            latched_position: Cell::new(None),
            latched_destination: Cell::new(None),
            latched_source: Cell::new(None),
        };
        lease.apply_placement(lease.attached.get());
        lease.surface.commit();
        lease.host.flush()?;
        Ok(lease)
    }

    /// Sends the protocol state implied by the current placement against a
    /// buffer of size `buffer`, skipping requests whose value has not changed.
    /// Returns whether any double-buffered child state changed and therefore
    /// needs a commit on this surface; `set_position` is parent state and never
    /// counts.
    fn apply_placement(&self, buffer: (u32, u32)) -> bool {
        let delta = placement_delta(
            self.placement.get(),
            buffer,
            self.latched_position.get(),
            self.latched_destination.get(),
            self.latched_source.get(),
        );
        if let Some((x, y)) = delta.position {
            self.subsurface.set_position(x, y);
            self.latched_position.set(Some((x, y)));
        }
        if let Some((width, height)) = delta.destination {
            self.viewport.set_destination(width, height);
            self.latched_destination.set(Some((width, height)));
        }
        if let Some(source) = delta.source {
            self.viewport.set_source(
                f64::from(source.0),
                f64::from(source.1),
                f64::from(source.2),
                f64::from(source.3),
            );
            self.latched_source.set(Some(source));
        }
        delta.destination.is_some() || delta.source.is_some()
    }
}

/// The protocol requests a placement still needs, given what was already sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PlacementDelta {
    position: Option<(i32, i32)>,
    destination: Option<(i32, i32)>,
    source: Option<(u32, u32, u32, u32)>,
}

/// Diffs a placement against the latched protocol state.
///
/// A workspace pan moves every card every frame while their sizes and clips
/// stay put, so re-sending an unchanged destination or source rectangle is pure
/// protocol traffic. Keeping this pure also makes the clipping arithmetic -
/// the part that can raise `out_of_buffer` and kill the connection - testable
/// without a compositor.
fn placement_delta(
    placement: SurfacePlacement,
    buffer: (u32, u32),
    latched_position: Option<(i32, i32)>,
    latched_destination: Option<(i32, i32)>,
    latched_source: Option<(u32, u32, u32, u32)>,
) -> PlacementDelta {
    let position = (placement.logical_x, placement.logical_y);
    let destination = (
        placement.logical_width.max(1),
        placement.logical_height.max(1),
    );
    let source = clamp_source(placement, buffer);
    PlacementDelta {
        position: (latched_position != Some(position)).then_some(position),
        destination: (latched_destination != Some(destination)).then_some(destination),
        source: (latched_source != Some(source)).then_some(source),
    }
}

/// Confines a placement's source rectangle to a buffer of size `buffer`.
///
/// `wp_viewport` raises `out_of_buffer` for a source rectangle that leaves the
/// attached buffer, which would kill the whole Wayland connection. The only
/// window in which the requested rectangle can exceed the buffer is between a
/// widget growing and Servo presenting at the new size, so clamping costs at
/// most one frame of slightly over-scaled content.
fn clamp_source(placement: SurfacePlacement, buffer: (u32, u32)) -> (u32, u32, u32, u32) {
    let (width, height) = (buffer.0.max(1), buffer.1.max(1));
    let x = placement.source_x.min(width - 1);
    let y = placement.source_y.min(height - 1);
    (
        x,
        y,
        placement.source_width.clamp(1, width - x),
        placement.source_height.clamp(1, height - y),
    )
}

impl HasDisplayHandle for WaylandLease {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, raw_window_handle::HandleError> {
        let display = NonNull::new(self.host.display as *mut c_void)
            .expect("validated non-null Wayland display handle");
        let raw = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display));
        // SAFETY: the host retains the parent display owner for this borrow.
        Ok(unsafe { DisplayHandle::borrow_raw(raw) })
    }
}

impl HasWindowHandle for WaylandLease {
    fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
        let surface = NonNull::new(self.surface.id().as_ptr().cast::<c_void>())
            .expect("live child surface has a native wl_proxy");
        let raw = RawWindowHandle::Wayland(WaylandWindowHandle::new(surface));
        // SAFETY: the wl_surface proxy is retained by this lease for the borrow.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

impl WaylandSubsurfaceLease for WaylandLease {
    fn set_geometry(&self, placement: SurfacePlacement) -> Result<(), BrowserError> {
        self.placement.set(placement);
        // The child is desynchronized, so its own commit applies the viewport
        // state immediately. The source rectangle is clamped to the buffer the
        // compositor already holds, because there is no new buffer to widen it.
        if self.apply_placement(self.attached.get()) {
            self.surface.commit();
        }
        Ok(())
    }

    fn set_visible(&self, visible: bool) -> Result<(), BrowserError> {
        if self.visible.replace(visible) == visible {
            return Ok(());
        }
        if !visible {
            // A null attachment unmaps the child without a software buffer or
            // readback. Runtime throttling prevents Servo from repainting it.
            self.surface.attach(None, 0, 0);
            self.surface.commit();
        }
        Ok(())
    }

    fn before_present(&self, buffer: PhysicalViewport) -> Result<(), BrowserError> {
        // The rendering context is already at `buffer`, and the buffer it is
        // about to attach applies atomically with any viewport state pending
        // here, so this is the only point at which a grown source rectangle can
        // be latched without risking `out_of_buffer`. No commit: the rendering
        // context's present issues the attach and the commit.
        let buffer = (buffer.width, buffer.height);
        self.attached.set(buffer);
        self.apply_placement(buffer);
        Ok(())
    }

    fn flush(&self) -> Result<(), BrowserError> {
        self.dispatch_pending()?;
        self.host.flush()
    }

    fn dispatch_pending(&self) -> Result<(), BrowserError> {
        self.host.dispatch_pending()
    }
}

impl Drop for WaylandLease {
    fn drop(&mut self) {
        // Servo's WindowRenderingContext is declared before the lease in
        // FallbackSurface, so it is gone before these native objects are torn
        // down. Explicitly unmap the child, then destroy protocol roles in
        // dependency order while the retained parent is still valid. The
        // desynchronized child's own commit unmaps it; the parent is never
        // committed from a browser thread.
        self.surface.attach(None, 0, 0);
        self.surface.commit();
        self.viewport.destroy();
        self.subsurface.destroy();
        self.surface.destroy();
        let _ = self.host.flush();
    }
}

pub(crate) struct FallbackSurface {
    pub context: Rc<servo::WindowRenderingContext>,
    pub lease: Box<dyn WaylandSubsurfaceLease>,
}

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetCurrentDisplay() -> *mut c_void;
    fn eglGetCurrentSurface(which: i32) -> *mut c_void;
    fn eglQuerySurface(
        display: *mut c_void,
        surface: *mut c_void,
        attribute: i32,
        value: *mut i32,
    ) -> u32;
    fn eglChooseConfig(
        display: *mut c_void,
        attributes: *const i32,
        configs: *mut *mut c_void,
        config_size: i32,
        count: *mut i32,
    ) -> u32;
    fn eglGetConfigAttrib(
        display: *mut c_void,
        config: *mut c_void,
        attribute: i32,
        value: *mut i32,
    ) -> u32;
    fn eglSwapInterval(display: *mut c_void, interval: i32) -> u32;
    fn eglGetError() -> u32;
}

const EGL_CONFIG_ID: i32 = 0x3028;
const EGL_MIN_SWAP_INTERVAL: i32 = 0x303b;
const EGL_MAX_SWAP_INTERVAL: i32 = 0x303c;
const EGL_NONE: i32 = 0x3038;
const EGL_DRAW: i32 = 0x3059;

fn require_zero_swap_interval(minimum: i32, maximum: i32) -> Result<(), BrowserError> {
    if minimum <= 0 && maximum >= 0 {
        Ok(())
    } else {
        Err(BrowserError::Wayland(format!(
            "Servo EGL draw surface requires swap interval {minimum}..={maximum}; interval zero is required to keep hidden browser cards responsive"
        )))
    }
}

impl FallbackSurface {
    pub(crate) fn create(
        factory: &Arc<dyn WaylandSubsurfaceFactory>,
        viewport: PhysicalViewport,
        scale_factor: f32,
    ) -> Result<Self, BrowserError> {
        let lease = factory.create(viewport, scale_factor)?;
        let display = lease
            .display_handle()
            .map_err(|error| BrowserError::Wayland(error.to_string()))?;
        let window = lease
            .window_handle()
            .map_err(|error| BrowserError::Wayland(error.to_string()))?;
        let context = servo::WindowRenderingContext::new(
            display,
            window,
            dpi::PhysicalSize::new(viewport.width, viewport.height),
        )
        .map_err(|error| BrowserError::Wayland(format!("hardware Servo context: {error:?}")))?;
        let rendering = Self {
            context: Rc::new(context),
            lease,
        };
        rendering.prepare_for_paint()?;
        rendering.validate_swap_interval()?;
        Ok(rendering)
    }

    /// EGL silently clamps a requested swap interval to the selected config's
    /// range. A successful eglSwapInterval(0) is therefore insufficient: reject
    /// a surface that requires frame pacing before any page can paint into it.
    fn validate_swap_interval(&self) -> Result<(), BrowserError> {
        // SAFETY: prepare_for_paint selected our live EGL draw surface on this
        // thread. Every output pointer below refers to a live initialized
        // local, and each attribute array is terminated with EGL_NONE.
        unsafe {
            let display = eglGetCurrentDisplay();
            let surface = eglGetCurrentSurface(EGL_DRAW);
            let mut config_id = 0;
            if display.is_null()
                || surface.is_null()
                || eglQuerySurface(display, surface, EGL_CONFIG_ID, &mut config_id) == 0
            {
                return Err(BrowserError::Wayland(format!(
                    "query Servo EGL draw-surface config: {:#x}",
                    eglGetError()
                )));
            }
            let attributes = [EGL_CONFIG_ID, config_id, EGL_NONE];
            let mut config = std::ptr::null_mut();
            let mut count = 0;
            if eglChooseConfig(display, attributes.as_ptr(), &mut config, 1, &mut count) == 0
                || count != 1
                || config.is_null()
            {
                return Err(BrowserError::Wayland(format!(
                    "resolve Servo EGL draw-surface config: {:#x}",
                    eglGetError()
                )));
            }
            let mut minimum = 0;
            let mut maximum = 0;
            if eglGetConfigAttrib(display, config, EGL_MIN_SWAP_INTERVAL, &mut minimum) == 0
                || eglGetConfigAttrib(display, config, EGL_MAX_SWAP_INTERVAL, &mut maximum) == 0
            {
                return Err(BrowserError::Wayland(format!(
                    "query Servo EGL swap-interval range: {:#x}",
                    eglGetError()
                )));
            }
            require_zero_swap_interval(minimum, maximum)
        }
    }

    /// Keep presentation independent of a compositor frame callback. A hidden
    /// Wayland child is allowed to retain such a callback indefinitely. Waiting
    /// for it in EGL would block the shared Servo thread, including the command
    /// that makes the child visible again and every other browser card.
    ///
    /// Servo's new-frame notifications and the engine animation cadence already
    /// pace paints. Interval zero makes Mesa use a display sync callback instead
    /// of a visibility-dependent surface callback. Apply it before the first
    /// paint and each later paint because resizing can recreate the EGL surface
    /// with its default swap interval.
    pub(crate) fn prepare_for_paint(&self) -> Result<(), BrowserError> {
        use servo::RenderingContext;

        self.context
            .make_current()
            .map_err(|error| BrowserError::Wayland(format!("make Servo context current: {error:?}")))?;
        // SAFETY: WindowRenderingContext is an EGL Wayland context, owned by
        // this browser thread, and make_current above selected its live draw
        // surface. EGL handles are opaque; no pointer is dereferenced here.
        unsafe {
            let display = eglGetCurrentDisplay();
            if display.is_null() || eglSwapInterval(display, 0) == 0 {
                return Err(BrowserError::Wayland(format!(
                    "disable visibility-dependent EGL swap throttling: {:#x}",
                    eglGetError()
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draw_surface_must_support_zero_before_any_paint() {
        assert!(require_zero_swap_interval(0, 1).is_ok());
        assert!(require_zero_swap_interval(0, 0).is_ok());

        // EGL reports success when zero is clamped to this config's minimum.
        // Accepting it would keep the shared engine's hidden-surface deadlock.
        let error = require_zero_swap_interval(1, 1).unwrap_err();
        assert!(error.to_string().contains("requires swap interval 1..=1"));
        assert!(require_zero_swap_interval(2, 4).is_err());
        assert!(require_zero_swap_interval(-1, -1).is_err());
    }

    fn clipped(
        source_x: u32,
        source_y: u32,
        source_width: u32,
        source_height: u32,
    ) -> SurfacePlacement {
        SurfacePlacement {
            logical_x: 10,
            logical_y: 20,
            logical_width: source_width.max(1) as i32,
            logical_height: source_height.max(1) as i32,
            source_x,
            source_y,
            source_width,
            source_height,
        }
    }

    #[test]
    fn a_fitting_source_rectangle_is_passed_through_unchanged() {
        assert_eq!(
            clamp_source(clipped(40, 10, 200, 300), (480, 600)),
            (40, 10, 200, 300)
        );
    }

    #[test]
    fn a_source_rectangle_never_leaves_the_attached_buffer() {
        // A card that grew before Servo presented at the new size: the
        // requested rectangle must be cropped, never committed as-is, because
        // wp_viewport answers out_of_buffer by killing the connection.
        assert_eq!(
            clamp_source(clipped(0, 0, 900, 1200), (480, 600)),
            (0, 0, 480, 600)
        );
        assert_eq!(
            clamp_source(clipped(400, 500, 200, 300), (480, 600)),
            (400, 500, 80, 100)
        );
        // Even a source origin past the end of the buffer stays committable.
        let (x, y, width, height) = clamp_source(clipped(9_000, 9_000, 10, 10), (480, 600));
        assert!(x < 480 && y < 600 && x + width <= 480 && y + height <= 600);
        assert!(width >= 1 && height >= 1);
    }

    #[test]
    fn a_pan_that_only_moves_the_card_sends_one_request() {
        let placement = clipped(0, 0, 480, 600);
        let buffer = (480, 600);
        let latched = placement_delta(placement, buffer, None, None, None);
        assert!(
            latched.position.is_some() && latched.destination.is_some() && latched.source.is_some()
        );

        let moved = SurfacePlacement {
            logical_x: placement.logical_x + 7,
            ..placement
        };
        let delta = placement_delta(
            moved,
            buffer,
            latched.position,
            latched.destination,
            latched.source,
        );
        assert_eq!(delta.position, Some((17, 20)));
        assert_eq!(delta.destination, None);
        assert_eq!(delta.source, None);
    }

    #[test]
    fn an_unchanged_placement_needs_no_requests_at_all() {
        let placement = clipped(12, 34, 100, 200);
        let buffer = (480, 600);
        let first = placement_delta(placement, buffer, None, None, None);
        assert_eq!(
            placement_delta(
                placement,
                buffer,
                first.position,
                first.destination,
                first.source
            ),
            PlacementDelta::default()
        );
    }

    #[test]
    fn growing_the_buffer_relatches_the_source_rectangle() {
        // The clip the widget asked for is remembered, so the first present at
        // the grown size restores the full requested region.
        let placement = clipped(0, 0, 900, 1200);
        let cropped = placement_delta(placement, (480, 600), None, None, None);
        assert_eq!(cropped.source, Some((0, 0, 480, 600)));

        let grown = placement_delta(
            placement,
            (900, 1200),
            cropped.position,
            cropped.destination,
            cropped.source,
        );
        assert_eq!(grown.source, Some((0, 0, 900, 1200)));
        assert_eq!(grown.position, None);
    }
}

#[cfg(all(test, feature = "browser-protocol-fixture"))]
#[path = "protocol_fixture.rs"]
mod protocol_fixture;
