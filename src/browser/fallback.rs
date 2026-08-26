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
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_compositor, wl_region, wl_registry, wl_subcompositor, wl_subsurface, wl_surface},
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};

use super::{BrowserError, PhysicalViewport, SurfacePlacement};

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
    fn create(
        &self,
        viewport: PhysicalViewport,
        scale_factor: f32,
    ) -> Result<Box<dyn WaylandSubsurfaceLease>, BrowserError>;
}

/// Production factory for embedding Servo into a child of an existing Wayland
/// `wl_surface` supplied through raw-window-handle 0.6.
///
/// A retained lifetime guard travels with every lease. Protocol objects and
/// their event queue are created lazily on the browser thread and reused across
/// hibernation/restoration cycles.
pub struct RawWaylandSubsurfaceFactory {
    display: usize,
    parent_surface: usize,
    lifetime_guard: Arc<dyn Any + Send + Sync>,
    host: Mutex<Option<Arc<WaylandHost>>>,
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
    /// connection and remain valid until every factory, lease, and browser
    /// thread using them has been dropped. `lifetime_guard` must either own the
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
            return Err(BrowserError::Wayland(format!(
                "expected a Wayland display handle, received {display:?}"
            )));
        };
        let RawWindowHandle::Wayland(window) = window else {
            return Err(BrowserError::Wayland(format!(
                "expected a Wayland window handle for the parent wl_surface, received {window:?}"
            )));
        };

        Ok(Self {
            display: display.display.as_ptr() as usize,
            parent_surface: window.surface.as_ptr() as usize,
            lifetime_guard,
            host: Mutex::new(None),
        })
    }

    fn host(&self) -> Result<Arc<WaylandHost>, BrowserError> {
        let mut slot = self
            .host
            .lock()
            .map_err(|_| BrowserError::Wayland("Wayland factory lock was poisoned".into()))?;
        if let Some(host) = slot.as_ref() {
            return Ok(host.clone());
        }

        let host = Arc::new(WaylandHost::connect(
            self.display,
            self.parent_surface,
            self.lifetime_guard.clone(),
        )?);
        *slot = Some(host.clone());
        Ok(host)
    }
}

impl WaylandSubsurfaceFactory for RawWaylandSubsurfaceFactory {
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

struct WaylandDispatch;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandDispatch {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
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
    _lifetime_guard: Arc<dyn Any + Send + Sync>,
}

impl WaylandHost {
    fn connect(
        display: usize,
        parent_surface: usize,
        lifetime_guard: Arc<dyn Any + Send + Sync>,
    ) -> Result<Self, BrowserError> {
        let display_ptr = display as *mut c_void;
        let parent_ptr = parent_surface as *mut c_void;

        // SAFETY: the factory's construction contract guarantees the foreign
        // display remains valid for this host's full lifetime.
        let backend = unsafe { Backend::from_foreign_display(display_ptr.cast()) };
        let connection = Connection::from_backend(backend);

        // SAFETY: that same contract guarantees a live wl_surface on this
        // foreign display. from_ptr additionally validates its proxy interface.
        let parent_id =
            unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), parent_ptr.cast()) }
                .map_err(|_| {
                    BrowserError::Wayland(
                        "the supplied Wayland window handle is not a live wl_surface".into(),
                    )
                })?;
        let parent = wl_surface::WlSurface::from_id(&connection, parent_id).map_err(|_| {
            BrowserError::Wayland("could not wrap the supplied parent wl_surface".into())
        })?;

        let (globals, event_queue) = registry_queue_init::<WaylandDispatch>(&connection)
            .map_err(|error| BrowserError::Wayland(format!("discover Wayland globals: {error}")))?;
        let queue = event_queue.handle();
        let compositor = globals
            .bind::<wl_compositor::WlCompositor, _, _>(&queue, 1..=6, ())
            .map_err(|error| BrowserError::Wayland(format!("bind wl_compositor: {error}")))?;
        let subcompositor = globals
            .bind::<wl_subcompositor::WlSubcompositor, _, _>(&queue, 1..=1, ())
            .map_err(|error| BrowserError::Wayland(format!("bind wl_subcompositor: {error}")))?;
        let viewporter = globals
            .bind::<wp_viewporter::WpViewporter, _, _>(&queue, 1..=1, ())
            .map_err(|error| {
                BrowserError::Wayland(format!(
                    "bind wp_viewporter (required for GPU fractional scaling): {error}"
                ))
            })?;

        for (name, id) in [
            ("wl_compositor", compositor.id()),
            ("wl_subcompositor", subcompositor.id()),
            ("wp_viewporter", viewporter.id()),
        ] {
            if id.is_null() {
                return Err(BrowserError::Wayland(format!(
                    "Wayland server returned an inert {name} proxy"
                )));
            }
        }

        Ok(Self {
            display,
            connection,
            dispatch: Mutex::new((event_queue, WaylandDispatch)),
            compositor,
            subcompositor,
            viewporter,
            parent,
            _lifetime_guard: lifetime_guard,
        })
    }

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
        Ok(Self {
            context: Rc::new(context),
            lease,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clipped(source_x: u32, source_y: u32, source_width: u32, source_height: u32) -> SurfacePlacement {
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
        assert!(latched.position.is_some() && latched.destination.is_some() && latched.source.is_some());

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
