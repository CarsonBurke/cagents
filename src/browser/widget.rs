use std::{collections::BTreeSet, marker::PhantomData, sync::Arc};

use cosmic::iced::{
    Event, Length, Point, Rectangle, Size,
    advanced::{
        Clipboard, Layout, Shell, Widget,
        input_method::{self, InputMethod, Preedit, Purpose},
        layout, mouse, renderer,
        widget::{Tree, tree},
    },
    keyboard,
    mouse::{Button, ScrollDelta},
    touch, window,
};
use cosmic::{Element, Renderer, Theme};

use super::{
    BrowserError, BrowserHandle, BrowserInput, ButtonState, ImeInput, InputMethodState,
    PhysicalViewport, SurfacePlacement, TouchState, WheelUnit,
    input::{iced_key, iced_mouse_button, local_device_point, wheel_pixels},
};

#[derive(Default)]
struct WidgetState {
    shortcut_keys: Vec<keyboard::key::Physical>,
    focused: bool,
    focus_requested: bool,
    cursor_inside: bool,
    pointer_capture: u8,
    touch_capture: BTreeSet<u64>,
    last_cursor: Option<Point>,
    last_geometry: Option<(PhysicalViewport, SurfacePlacement)>,
    last_scale_factor: Option<f32>,
    last_presented: Option<bool>,
    geometry_presented: bool,
    preedit: Option<Preedit>,
}

/// Native geometry for one frame: the full buffer Servo renders, the clipped
/// region actually presented, and whether anything is presented at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativePlacement {
    /// Full physical size of the widget rectangle. Servo lays the page out at
    /// this size regardless of clipping.
    buffer: PhysicalViewport,
    placement: SurfacePlacement,
    /// False only when the widget rectangle and the clip rectangle do not
    /// overlap at all.
    presented: bool,
}

/// Intersects the widget rectangle with iced's clip rectangle.
///
/// Partial clipping is expressed as a `wp_viewport` source rectangle, not as
/// visibility: blanking the whole card the moment one pixel left the clip made
/// a workspace pan flicker every card fully off and on, and each flip cost a
/// Servo show/hide plus a forced repaint. Only a card with no overlap at all is
/// unmapped.
fn native_placement(
    bounds: Rectangle,
    clip: Rectangle,
    virtual_offset: f32,
    scale_factor: f32,
    ui_scale: f32,
) -> NativePlacement {
    let virtual_offset = if virtual_offset.is_finite() {
        virtual_offset.max(0.0)
    } else {
        0.0
    };
    let scale = if scale_factor.is_finite() {
        scale_factor.max(0.01)
    } else {
        1.0
    };
    let buffer = PhysicalViewport::from_logical(bounds.width, bounds.height, scale);

    let left = bounds.x.max(clip.x);
    let top = bounds.y.max(clip.y);
    let right = (bounds.x + bounds.width).min(clip.x + clip.width);
    let bottom = (bounds.y + bounds.height).min(clip.y + clip.height);
    let presented = right > left && bottom > top;
    // A card with no overlap is unmapped, so its placement is never committed;
    // collapse it to the buffer origin rather than to negative extents.
    let (width, height) = if presented {
        (right - left, bottom - top)
    } else {
        (0.0, 0.0)
    };

    // The source rectangle lives in Servo's buffer, whose origin is the widget
    // rectangle's origin, and may never leave it.
    let source_x = (((left - bounds.x).max(0.0) * scale).round() as u32).min(buffer.width - 1);
    let source_y = (((top - bounds.y).max(0.0) * scale).round() as u32).min(buffer.height - 1);
    let source = PhysicalViewport::from_logical(width, height, scale);

    NativePlacement {
        buffer,
        placement: SurfacePlacement {
            // iced units include the user's UI multiplier; Wayland parent
            // surface coordinates include only the compositor's display scale.
            logical_x: ((left - virtual_offset) * ui_scale).round() as i32,
            logical_y: (top * ui_scale).round() as i32,
            logical_width: ((width * ui_scale).round() as i32).max(1),
            logical_height: ((height * ui_scale).round() as i32).max(1),
            source_x,
            source_y,
            source_width: source.width.min(buffer.width - source_x),
            source_height: source.height.min(buffer.height - source_y),
        },
        presented,
    }
}

type ErrorMapper<Message> = dyn Fn(BrowserError) -> Message + Send + Sync + 'static;

pub struct BrowserWidget<'a, Message> {
    handle: &'a BrowserHandle,
    ime: Option<InputMethodState>,
    scale_factor: f32,
    ui_scale: f32,
    model_focused: bool,
    obscured: bool,
    virtual_offset: f32,
    on_focus: Option<Message>,
    on_shortcut: Option<Box<dyn Fn(keyboard::Event) -> Option<Message> + 'a>>,
    on_error: Option<Arc<ErrorMapper<Message>>>,
    _message: PhantomData<fn() -> Message>,
}

impl<'a, Message> std::fmt::Debug for BrowserWidget<'a, Message> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserWidget")
            .field("scale_factor", &self.scale_factor)
            .field("virtual_offset", &self.virtual_offset)
            .finish_non_exhaustive()
    }
}

impl<'a, Message> BrowserWidget<'a, Message> {
    pub fn new(handle: &'a BrowserHandle) -> Self {
        Self {
            handle,
            ime: None,
            scale_factor: 1.0,
            ui_scale: 1.0,
            virtual_offset: 0.0,
            model_focused: false,
            obscured: false,
            on_focus: None,
            on_shortcut: None,
            on_error: None,
            _message: PhantomData,
        }
    }

    pub fn scale_factor(mut self, scale_factor: f32) -> Self {
        self.scale_factor = scale_factor.max(0.01);
        self
    }

    pub fn on_shortcut(
        mut self,
        callback: impl Fn(keyboard::Event) -> Option<Message> + 'a,
    ) -> Self {
        self.on_shortcut = Some(Box::new(callback));
        self
    }

    /// Converts iced geometry to the parent Wayland surface's logical units.
    /// `scale_factor` separately includes both display and interface scaling.
    pub fn ui_scale(mut self, ui_scale: f32) -> Self {
        self.ui_scale = ui_scale.max(0.01);
        self
    }

    pub fn virtual_offset(mut self, virtual_offset: f32) -> Self {
        self.virtual_offset = if virtual_offset.is_finite() {
            virtual_offset.max(0.0)
        } else {
            0.0
        };
        self
    }

    pub fn input_method(mut self, ime: Option<InputMethodState>) -> Self {
        self.ime = ime;
        self
    }

    pub fn model_focused(mut self, focused: bool) -> Self {
        self.model_focused = focused;
        self
    }

    /// Native child surfaces stack above the application's rendered overlays.
    /// Hide their pixels while an overlay owns the workspace, retaining the
    /// browser's layout and residency so dismissal restores the same page.
    pub fn obscured(mut self, obscured: bool) -> Self {
        self.obscured = obscured;
        self
    }

    fn synchronize_presentation(&self, state: &mut WidgetState) -> Result<(), BrowserError> {
        let presented = state.geometry_presented && !self.obscured;
        if state.last_presented != Some(presented) {
            self.handle.set_presented(presented)?;
            state.last_presented = Some(presented);
        }
        Ok(())
    }

    pub fn on_focus(mut self, message: Message) -> Self {
        self.on_focus = Some(message);
        self
    }

    pub fn on_error(
        mut self,
        mapper: impl Fn(BrowserError) -> Message + Send + Sync + 'static,
    ) -> Self {
        self.on_error = Some(Arc::new(mapper));
        self
    }

    fn publish_error(&self, error: BrowserError, shell: &mut Shell<'_, Message>) {
        if let Some(mapper) = &self.on_error {
            shell.publish(mapper(error));
        }
    }

    fn send(&self, input: BrowserInput, shell: &mut Shell<'_, Message>) {
        if let Err(error) = self.handle.input(input) {
            self.publish_error(error, shell);
        }
    }

    fn synchronize_focus(&self, state: &mut WidgetState) -> Result<(), BrowserError> {
        let focused = self.model_focused && !self.obscured;
        if focused {
            state.focus_requested = false;
        }
        if state.focused == focused {
            return Ok(());
        }
        self.handle.set_focused(focused)?;
        state.focused = focused;
        if !focused {
            state.pointer_capture = 0;
            state.touch_capture.clear();
            state.preedit = None;
        }
        Ok(())
    }

    fn device_point(&self, bounds: Rectangle, point: Point) -> super::DevicePoint {
        local_device_point(point.x, point.y, bounds.x, bounds.y, self.scale_factor)
    }

    fn request_ime(&self, state: &WidgetState, bounds: Rectangle, shell: &mut Shell<'_, Message>) {
        let Some(ime) = self.ime.as_ref().filter(|_| state.focused) else {
            shell.request_input_method(&InputMethod::<String>::Disabled);
            return;
        };
        let scale = self.scale_factor.max(0.01);
        let (x, y, width, height) = ime.device_rect;
        shell.request_input_method(&InputMethod::Enabled {
            cursor: Rectangle::new(
                Point::new(bounds.x + x as f32 / scale, bounds.y + y as f32 / scale),
                Size::new(
                    (width.max(1) as f32) / scale,
                    (height.max(1) as f32) / scale,
                ),
            ),
            purpose: if ime.secure {
                Purpose::Secure
            } else {
                Purpose::Normal
            },
            preedit: state.preedit.as_ref().map(Preedit::as_ref),
        });
    }
}

impl<Message: Clone> Widget<Message, Theme, Renderer> for BrowserWidget<'_, Message> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<WidgetState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(WidgetState::default())
    }

    fn diff(&self, tree: &mut Tree) {
        // Overlay widgets may stop forwarding events to their covered content.
        // Reconcile at tree construction, before that overlay is first drawn.
        let state = tree.state.downcast_mut::<WidgetState>();
        let _ = self.synchronize_presentation(state);
        let _ = self.synchronize_focus(state);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let state = tree.state.downcast_mut::<WidgetState>();
        let native = native_placement(
            bounds,
            *viewport,
            self.virtual_offset,
            self.scale_factor,
            self.ui_scale,
        );
        if state.last_scale_factor != Some(self.scale_factor) {
            state.last_scale_factor = Some(self.scale_factor);
            if let Err(error) = self.handle.set_scale_factor(self.scale_factor)
                && let Some(mapper) = &self.on_error
            {
                shell.publish(mapper(error));
            }
        }
        let geometry = (native.buffer, native.placement);
        if state.last_geometry != Some(geometry) {
            state.last_geometry = Some(geometry);
            if let Err(error) = self.handle.place(native.buffer, native.placement)
                && let Some(mapper) = &self.on_error
            {
                shell.publish(mapper(error));
            }
        }
        // Presentation is this widget's half of the worker's effective
        // visibility; the application owns the other half through
        // `set_visible`. Partial clipping never lands here - it is carried by
        // the placement's source rectangle instead.
        state.geometry_presented = native.presented;
        if let Err(error) = self.synchronize_presentation(state) {
            self.publish_error(error, shell);
        }
        if let Err(error) = self.synchronize_focus(state) {
            self.publish_error(error, shell);
        }

        if self.obscured {
            return;
        }

        match event {
            Event::Mouse(cosmic::iced::mouse::Event::CursorMoved { position }) => {
                state.last_cursor = Some(*position);
                let inside = bounds.contains(*position);
                if inside || state.pointer_capture != 0 {
                    self.send(
                        BrowserInput::MouseMove {
                            point: self.device_point(bounds, *position),
                        },
                        shell,
                    );
                    state.cursor_inside = inside;
                    shell.capture_event();
                } else if state.cursor_inside {
                    state.cursor_inside = false;
                    self.send(BrowserInput::MouseLeave, shell);
                }
            }
            Event::Mouse(cosmic::iced::mouse::Event::CursorLeft) if state.cursor_inside => {
                state.cursor_inside = false;
                self.send(BrowserInput::MouseLeave, shell);
            }
            Event::Mouse(cosmic::iced::mouse::Event::ButtonPressed(button)) => {
                if let Some(position) = cursor.position_in(bounds) {
                    if !self.model_focused
                        && !state.focus_requested
                        && let Some(message) = self.on_focus.clone()
                    {
                        state.focus_requested = true;
                        shell.publish(message);
                    }
                    state.pointer_capture |= button_bit(*button);
                    state.last_cursor =
                        Some(Point::new(bounds.x + position.x, bounds.y + position.y));
                    self.send(
                        BrowserInput::MouseButton {
                            state: ButtonState::Pressed,
                            button: iced_mouse_button(*button),
                            point: self.device_point(
                                bounds,
                                state.last_cursor.expect("cursor was stored"),
                            ),
                        },
                        shell,
                    );
                    shell.capture_event();
                }
            }
            Event::Mouse(cosmic::iced::mouse::Event::ButtonReleased(button))
                if state.pointer_capture & button_bit(*button) != 0 =>
            {
                state.pointer_capture &= !button_bit(*button);
                if let Some(position) = state.last_cursor {
                    self.send(
                        BrowserInput::MouseButton {
                            state: ButtonState::Released,
                            button: iced_mouse_button(*button),
                            point: self.device_point(bounds, position),
                        },
                        shell,
                    );
                }
                shell.capture_event();
            }
            Event::Mouse(cosmic::iced::mouse::Event::WheelScrolled { delta })
                if cursor.is_over(bounds) && cursor.is_over(*viewport) =>
            {
                let (x, y, unit) = match delta {
                    ScrollDelta::Lines { x, y } => (f64::from(*x), f64::from(*y), WheelUnit::Lines),
                    ScrollDelta::Pixels { x, y } => {
                        (f64::from(*x), f64::from(*y), WheelUnit::Pixels)
                    }
                };
                let (x, y) = wheel_pixels(x, y, unit);
                let point = cursor.position().expect("wheel cursor is over the widget");
                self.send(
                    BrowserInput::Wheel {
                        x,
                        y,
                        unit: WheelUnit::Pixels,
                        point: self.device_point(bounds, point),
                    },
                    shell,
                );
                shell.capture_event();
            }
            Event::Touch(event) => {
                let (touch_state, id, position) = match event {
                    touch::Event::FingerPressed { id, position } if bounds.contains(*position) => {
                        state.touch_capture.insert(id.0);
                        (TouchState::Down, id.0, *position)
                    }
                    touch::Event::FingerMoved { id, position }
                        if state.touch_capture.contains(&id.0) =>
                    {
                        (TouchState::Move, id.0, *position)
                    }
                    touch::Event::FingerLifted { id, position }
                        if state.touch_capture.remove(&id.0) =>
                    {
                        (TouchState::Up, id.0, *position)
                    }
                    touch::Event::FingerLost { id, position }
                        if state.touch_capture.remove(&id.0) =>
                    {
                        (TouchState::Cancel, id.0, *position)
                    }
                    _ => return,
                };
                self.send(
                    BrowserInput::Touch {
                        state: touch_state,
                        id: id.min(i32::MAX as u64) as i32,
                        point: self.device_point(bounds, position),
                    },
                    shell,
                );
                shell.capture_event();
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                physical_key,
                location,
                modifiers,
                repeat,
                ..
            }) if self.model_focused => {
                if let Some(message) = self.on_shortcut.as_ref().and_then(|callback| {
                    let Event::Keyboard(event) = event else {
                        return None;
                    };
                    callback(event.clone())
                }) {
                    if !state.shortcut_keys.contains(physical_key) {
                        state.shortcut_keys.push(*physical_key);
                    }
                    shell.publish(message);
                    shell.capture_event();
                    return;
                }
                self.send(
                    iced_key(
                        ButtonState::Pressed,
                        key,
                        *physical_key,
                        *location,
                        *modifiers,
                        *repeat,
                        state.preedit.is_some(),
                    ),
                    shell,
                );
                shell.capture_event();
            }
            Event::Keyboard(keyboard::Event::KeyReleased {
                key,
                physical_key,
                location,
                modifiers,
                ..
            }) if self.model_focused => {
                if let Some(index) = state
                    .shortcut_keys
                    .iter()
                    .position(|key| key == physical_key)
                {
                    state.shortcut_keys.swap_remove(index);
                    shell.capture_event();
                    return;
                }
                self.send(
                    iced_key(
                        ButtonState::Released,
                        key,
                        *physical_key,
                        *location,
                        *modifiers,
                        false,
                        state.preedit.is_some(),
                    ),
                    shell,
                );
                shell.capture_event();
            }
            Event::InputMethod(event) if self.model_focused => {
                match event {
                    input_method::Event::Opened => {
                        state.preedit = Some(Preedit::new());
                        self.send(BrowserInput::Ime(ImeInput::Opened), shell);
                    }
                    input_method::Event::Preedit(text, selection) => {
                        state.preedit = Some(Preedit {
                            content: text.clone(),
                            selection: selection.clone(),
                            text_size: None,
                        });
                        self.send(
                            BrowserInput::Ime(ImeInput::Preedit {
                                text: text.clone(),
                                selection: selection.clone(),
                            }),
                            shell,
                        );
                    }
                    input_method::Event::Commit(text) => {
                        state.preedit = None;
                        self.send(BrowserInput::Ime(ImeInput::Commit(text.clone())), shell);
                    }
                    input_method::Event::Closed => {
                        state.preedit = None;
                        self.send(BrowserInput::Ime(ImeInput::Closed), shell);
                    }
                }
                shell.capture_event();
            }
            Event::Window(window::Event::Unfocused) if state.focused => {
                state.focused = false;
                state.pointer_capture = 0;
                state.touch_capture.clear();
                state.preedit = None;
                if let Err(error) = self.handle.set_focused(false) {
                    self.publish_error(error, shell);
                }
            }
            Event::Window(window::Event::RedrawRequested(_)) => {
                self.request_ime(state, bounds, shell);
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if tree.state.downcast_ref::<WidgetState>().cursor_inside {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
        _renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        // Servo renders directly into the positioned Wayland child surface.
    }
}

fn button_bit(button: Button) -> u8 {
    match button {
        Button::Left => 1,
        Button::Right => 2,
        Button::Middle => 4,
        Button::Back => 8,

        Button::Forward => 16,
        Button::Other(_) => 32,
    }
}

impl<'a, Message: Clone + 'a> From<BrowserWidget<'a, Message>> for Element<'a, Message> {
    fn from(widget: BrowserWidget<'a, Message>) -> Self {
        Self::new(widget)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_uses_current_visible_hit_without_requiring_a_previous_pointer_move() {
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let handle = BrowserHandle::fixture();
        let mut widget = BrowserWidget::<()>::new(&handle).scale_factor(2.0);
        let mut tree = Tree::new(&widget as &dyn Widget<(), Theme, Renderer>);
        let node = layout::Node::new(Size::new(200.0, 200.0));
        let clip = Rectangle::new(Point::new(50.0, 50.0), Size::new(100.0, 100.0));
        let wheel = Event::Mouse(mouse::Event::WheelScrolled {
            delta: ScrollDelta::Lines { x: 0.0, y: -1.0 },
        });
        let mut messages = Vec::new();
        widget.update(
            &mut tree,
            &wheel,
            Layout::new(&node),
            mouse::Cursor::Available(Point::new(60.0, 80.0)),
            &renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &clip,
        );
        assert_eq!(
            handle.mailbox.drain_coalesced().wheel,
            Some(BrowserInput::Wheel {
                x: 0.0,
                y: -76.0,
                unit: WheelUnit::Pixels,
                point: super::super::DevicePoint { x: 120.0, y: 160.0 },
            })
        );
        widget.update(
            &mut tree,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: ScrollDelta::Pixels { x: 2.5, y: -14.5 },
            }),
            Layout::new(&node),
            mouse::Cursor::Available(Point::new(60.0, 80.0)),
            &renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &clip,
        );
        assert_eq!(
            handle.mailbox.drain_coalesced().wheel,
            Some(BrowserInput::Wheel {
                x: 2.5,
                y: -14.5,
                unit: WheelUnit::Pixels,
                point: super::super::DevicePoint { x: 120.0, y: 160.0 },
            })
        );
        // An old hover or pressed mouse button must not send a wheel aimed
        // outside the browser or inside the portion clipped by the workspace.
        let state = tree.state.downcast_mut::<WidgetState>();
        state.cursor_inside = true;
        state.pointer_capture = 1;
        state.last_cursor = Some(Point::new(60.0, 80.0));
        for cursor in [
            mouse::Cursor::Available(Point::new(250.0, 80.0)),
            mouse::Cursor::Available(Point::new(20.0, 20.0)),
            mouse::Cursor::Unavailable,
        ] {
            widget.update(
                &mut tree,
                &wheel,
                Layout::new(&node),
                cursor,
                &renderer,
                &mut cosmic::iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &clip,
            );
            assert!(handle.mailbox.drain_coalesced().wheel.is_none());
        }
    }

    #[test]
    fn covering_overlay_hides_native_pixels_without_waiting_for_input() {
        let handle = BrowserHandle::fixture();
        let widget = BrowserWidget::<()>::new(&handle);
        let mut tree = Tree::new(&widget as &dyn Widget<(), Theme, Renderer>);
        {
            let state = tree.state.downcast_mut::<WidgetState>();
            state.geometry_presented = true;
            state.last_presented = Some(true);
            state.focused = true;
            state.pointer_capture = 1;
            state.touch_capture.insert(7);
            state.preedit = Some(Preedit::new());
        }
        let covered = BrowserWidget::<()>::new(&handle).obscured(true);
        covered.diff(&mut tree);
        let commands = handle.mailbox.drain_coalesced();
        assert_eq!(commands.presented, Some(false));
        assert_eq!(commands.focused, Some(false));
        let state = tree.state.downcast_ref::<WidgetState>();
        assert!(!state.focused);
        assert_eq!(state.pointer_capture, 0);
        assert!(state.touch_capture.is_empty());
        assert!(state.preedit.is_none());
        covered.diff(&mut tree);
        assert_eq!(handle.mailbox.drain_coalesced().presented, None);

        widget.diff(&mut tree);
        assert_eq!(handle.mailbox.drain_coalesced().presented, Some(true));
    }

    #[test]
    fn dismissing_an_overlay_does_not_reveal_a_clipped_browser() {
        let handle = BrowserHandle::fixture();
        let widget = BrowserWidget::<()>::new(&handle).obscured(true);
        let mut tree = Tree::new(&widget as &dyn Widget<(), Theme, Renderer>);
        widget.diff(&mut tree);
        assert_eq!(handle.mailbox.drain_coalesced().presented, Some(false));
        BrowserWidget::<()>::new(&handle).diff(&mut tree);
        assert_eq!(handle.mailbox.drain_coalesced().presented, None);
    }

    #[test]
    fn native_placement_tracks_the_horizontal_camera_offset() {
        let native = native_placement(
            Rectangle::new(Point::new(532.4, 44.0), Size::new(480.0, 600.0)),
            Rectangle::new(Point::new(400.0, 0.0), Size::new(800.0, 700.0)),
            300.4,
            1.25,
            1.0,
        );

        assert_eq!(native.placement.logical_x, 232);
        assert_eq!(native.placement.logical_y, 44);
        assert_eq!(
            native.buffer,
            PhysicalViewport {
                width: 600,
                height: 750,
            }
        );
        assert!(native.presented);
        // Nothing is clipped, so the whole buffer is presented at full size.
        assert_eq!(
            (native.placement.source_x, native.placement.source_y),
            (0, 0)
        );
        assert_eq!(
            (
                native.placement.source_width,
                native.placement.source_height
            ),
            (600, 750)
        );
        assert_eq!(
            (
                native.placement.logical_width,
                native.placement.logical_height
            ),
            (480, 600)
        );
    }

    #[test]
    fn a_partially_clipped_card_is_cropped_rather_than_blanked() {
        let body = Rectangle::new(Point::new(500.0, 44.0), Size::new(480.0, 596.0));

        // One logical pixel eaten on the left by the sidebar. The child moves
        // right by one pixel, presents one pixel less, and skips that pixel
        // column of the buffer - it must stay mapped.
        let clipped_left = native_placement(
            body,
            Rectangle::new(Point::new(501.0, 44.0), Size::new(479.0, 596.0)),
            400.0,
            1.0,
            1.0,
        );
        assert!(clipped_left.presented);
        assert_eq!(clipped_left.buffer, PhysicalViewport::new(480, 596));
        assert_eq!(clipped_left.placement.logical_x, 101);
        assert_eq!(clipped_left.placement.source_x, 1);
        assert_eq!(clipped_left.placement.source_width, 479);
        assert_eq!(clipped_left.placement.logical_width, 479);

        // One pixel eaten on the right by a neighbour: same size, same origin.
        let clipped_right = native_placement(
            body,
            Rectangle::new(Point::new(500.0, 44.0), Size::new(479.0, 596.0)),
            400.0,
            1.0,
            1.0,
        );
        assert!(clipped_right.presented);
        assert_eq!(clipped_right.placement.logical_x, 100);
        assert_eq!(clipped_right.placement.source_x, 0);
        assert_eq!(clipped_right.placement.source_width, 479);

        // One pixel eaten by the header above.
        let clipped_top = native_placement(
            body,
            Rectangle::new(Point::new(500.0, 45.0), Size::new(480.0, 595.0)),
            400.0,
            1.0,
            1.0,
        );
        assert!(clipped_top.presented);
        assert_eq!(clipped_top.placement.logical_y, 45);
        assert_eq!(clipped_top.placement.source_y, 1);
        assert_eq!(clipped_top.placement.source_height, 595);
    }

    #[test]
    fn a_clip_that_scales_maps_logical_overlap_to_buffer_pixels() {
        let native = native_placement(
            Rectangle::new(Point::new(100.0, 100.0), Size::new(400.0, 300.0)),
            // Half the card is cut off on the left and a quarter at the bottom.
            Rectangle::new(Point::new(300.0, 0.0), Size::new(400.0, 325.0)),
            0.0,
            2.0,
            1.0,
        );

        assert!(native.presented);
        assert_eq!(native.buffer, PhysicalViewport::new(800, 600));
        assert_eq!(native.placement.logical_x, 300);
        assert_eq!(native.placement.logical_y, 100);
        assert_eq!(
            (native.placement.source_x, native.placement.source_y),
            (400, 0)
        );
        assert_eq!(
            (
                native.placement.source_width,
                native.placement.source_height
            ),
            (400, 450)
        );
        assert_eq!(
            (
                native.placement.logical_width,
                native.placement.logical_height
            ),
            (200, 225)
        );
    }

    #[test]
    fn interface_scale_converts_clipped_placement_without_rescaling_buffer_coordinates() {
        let native = native_placement(
            Rectangle::new(Point::new(200.0, 40.0), Size::new(400.0, 300.0)),
            Rectangle::new(Point::new(240.0, 50.0), Size::new(320.0, 250.0)),
            100.0,
            1.5 * 1.25,
            1.25,
        );
        assert!(native.presented);
        assert_eq!(native.buffer, PhysicalViewport::new(750, 563));
        assert_eq!(
            native.placement,
            SurfacePlacement {
                logical_x: 175,
                logical_y: 63,
                logical_width: 400,
                logical_height: 313,
                source_x: 75,
                source_y: 19,
                source_width: 600,
                source_height: 469,
            }
        );
    }

    #[test]
    fn only_a_card_with_no_overlap_at_all_is_unmapped() {
        let body = Rectangle::new(Point::new(500.0, 44.0), Size::new(480.0, 596.0));

        assert!(native_placement(body, body, 400.0, 1.0, 1.0).presented);
        // Scrolled entirely past the right edge of the workspace viewport.
        assert!(
            !native_placement(
                body,
                Rectangle::new(Point::new(0.0, 0.0), Size::new(500.0, 700.0)),
                400.0,
                1.0,
                1.0,
            )
            .presented
        );
        // Entirely above the viewport.
        assert!(
            !native_placement(
                body,
                Rectangle::new(Point::new(500.0, 640.0), Size::new(480.0, 100.0)),
                400.0,
                1.0,
                1.0,
            )
            .presented
        );
    }

    #[test]
    fn an_unmapped_card_still_produces_a_committable_placement() {
        // The placement of an unmapped card is never committed, but it must not
        // be able to raise a Wayland protocol error if it ever were.
        let native = native_placement(
            Rectangle::new(Point::new(500.0, 44.0), Size::new(480.0, 596.0)),
            Rectangle::new(Point::new(0.0, 0.0), Size::new(10.0, 10.0)),
            400.0,
            1.0,
            1.0,
        );

        assert!(!native.presented);
        assert!(native.placement.logical_width >= 1 && native.placement.logical_height >= 1);
        assert!(native.placement.source_width >= 1 && native.placement.source_height >= 1);
        assert!(native.placement.source_x + native.placement.source_width <= native.buffer.width);
        assert!(native.placement.source_y + native.placement.source_height <= native.buffer.height);
    }
}
