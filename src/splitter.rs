//! Reusable, accessible pane splitter.
//!
//! The widget owns only transient gesture state. Pane width and persistence
//! remain with the caller, so the same splitter can resize either edge without
//! duplicating event handling or performing I/O from the view.

use cosmic::iced::advanced::Renderer as _;
use cosmic::{
    Element, Renderer, Theme,
    iced::{
        Border, Color, Event, Length, Point, Rectangle, Shadow, Size,
        advanced::{
            Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
            widget::{Operation, Tree, operation, tree},
        },
        id::Id,
        keyboard, window,
    },
    widget,
};

/// Only the shared rule participates in layout; the pointer target overlaps it.
pub const WIDTH: f32 = crate::design::BORDER_WIDTH;
const HIT_WIDTH: f32 = 8.0;
pub const KEYBOARD_STEP: f32 = 16.0;

/// The pane edge occupied by the splitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// The splitter is after the pane; moving right grows the pane.
    Trailing,
    /// The splitter is before the pane; moving left grows the pane.
    Leading,
}

pub struct Splitter<'a, Message> {
    content: Element<'a, Message>,
    width: f32,
    default_width: f32,
    min: f32,
    max: f32,
    edge: Edge,
    modal_open: bool,
    id: Id,
    label: &'a str,
    on_resize: Box<dyn Fn(f32) -> Message + 'a>,
    on_finish: Box<dyn Fn() -> Message + 'a>,
}

impl<'a, Message: 'a> Splitter<'a, Message> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Id,
        label: &'a str,
        width: f32,
        min: f32,
        max: f32,
        edge: Edge,
        modal_open: bool,
        on_resize: impl Fn(f32) -> Message + 'a,
        on_finish: impl Fn() -> Message + 'a,
    ) -> Self {
        debug_assert!(min.is_finite() && max.is_finite() && min <= max);
        Self {
            content: widget::container(cosmic::iced::widget::Space::new())
                .center_x(Length::Fixed(WIDTH))
                .height(Length::Fill)
                .into(),
            width: clamp(width, min, max),
            default_width: clamp(width, min, max),
            min,
            max,
            edge,
            modal_open,
            id,
            label,
            on_resize: Box::new(on_resize),
            on_finish: Box::new(on_finish),
        }
    }

    pub fn default_width(mut self, width: f32) -> Self {
        self.default_width = clamp(width, self.min, self.max);
        self
    }

    fn child_layout<'layout>(layout: Layout<'layout>) -> Layout<'layout> {
        layout
            .children()
            .next()
            .expect("splitter content layout")
            .with_virtual_offset(layout.virtual_offset())
    }

    fn hit_bounds(bounds: Rectangle) -> Rectangle {
        Rectangle {
            x: bounds.center_x() - HIT_WIDTH / 2.0,
            width: HIT_WIDTH,
            ..bounds
        }
    }

    fn resize(&self, width: f32) -> Message {
        (self.on_resize)(clamp(width, self.min, self.max))
    }

    fn pointer_delta(self_edge: Edge, start: f32, current: f32) -> f32 {
        match self_edge {
            Edge::Trailing => current - start,
            Edge::Leading => start - current,
        }
    }

    fn keyboard_delta(&self, key: &keyboard::Key) -> Option<f32> {
        let physical = match key {
            keyboard::Key::Named(keyboard::key::Named::ArrowLeft) => -KEYBOARD_STEP,
            keyboard::Key::Named(keyboard::key::Named::ArrowRight) => KEYBOARD_STEP,
            _ => return None,
        };
        Some(match self.edge {
            Edge::Trailing => physical,
            Edge::Leading => -physical,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ResizeOrigin {
    pointer_x: f32,
    width: f32,
}

#[derive(Debug, Default)]
struct SplitterState {
    drag: Option<ResizeOrigin>,
    is_focused: bool,
    keyboard_gesture: bool,
    last_click: Option<std::time::Instant>,
}

impl SplitterState {
    fn begin_drag(&mut self, pointer_x: f32, width: f32) {
        self.drag = Some(ResizeOrigin { pointer_x, width });
    }

    fn dragged_width(&self, edge: Edge, pointer_x: f32) -> Option<f32> {
        self.drag.map(|origin| {
            origin.width + Splitter::<()>::pointer_delta(edge, origin.pointer_x, pointer_x)
        })
    }

    fn finish_drag(&mut self) -> bool {
        self.drag.take().is_some()
    }

    fn keyboard_resize(&mut self, delta: Option<f32>) -> Option<f32> {
        if delta.is_some() {
            self.keyboard_gesture = true;
        }
        delta
    }

    fn finish_keyboard_gesture(&mut self, resize_key: bool) -> bool {
        resize_key && std::mem::take(&mut self.keyboard_gesture)
    }
}

impl operation::Focusable for SplitterState {
    fn is_focused(&self) -> bool {
        self.is_focused
    }

    fn focus(&mut self) {
        self.is_focused = true;
    }

    fn unfocus(&mut self) {
        self.is_focused = false;
    }
}

fn clamp(width: f32, min: f32, max: f32) -> f32 {
    if width.is_finite() {
        width.clamp(min, max)
    } else {
        min
    }
}

fn chorded(modifiers: keyboard::Modifiers) -> bool {
    modifiers.control() || modifiers.alt() || modifiers.shift() || modifiers.logo()
}

impl<'view, Message: 'view> Widget<Message, Theme, Renderer> for Splitter<'view, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<SplitterState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(SplitterState::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let content = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
            .move_to(Point::ORIGIN);
        layout::Node::with_children(content.size(), vec![content])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let bounds = layout.bounds();
        operation.traverse(&mut |operation| {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                Self::child_layout(layout),
                renderer,
                operation,
            );
        });
        operation.focusable(
            Some(&self.id),
            bounds,
            tree.state.downcast_mut::<SplitterState>(),
        );
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let bounds = Self::hit_bounds(layout.bounds());
        let state = tree.state.downcast_mut::<SplitterState>();
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) if !self.modal_open => {
                state.is_focused = cursor.is_over(bounds);
                if let Some(position) = cursor.position().filter(|_| state.is_focused) {
                    let now = std::time::Instant::now();
                    if state.last_click.take().is_some_and(|previous| {
                        now.duration_since(previous) <= std::time::Duration::from_millis(400)
                    }) {
                        state.drag = None;
                        shell.publish(self.resize(self.default_width));
                        shell.publish((self.on_finish)());
                        shell.capture_event();
                        return;
                    }
                    state.last_click = Some(now);
                    state.begin_drag(position.x, self.width);
                    shell.capture_event();
                    return;
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                shell.request_redraw();
                if let Some(width) = state.dragged_width(self.edge, position.x) {
                    state.last_click = None;
                    shell.publish(self.resize(width));
                    shell.capture_event();
                    return;
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.finish_drag() {
                    shell.publish((self.on_finish)());
                    shell.capture_event();
                    return;
                }
            }
            Event::Window(window::Event::Unfocused) => {
                let dragging = state.finish_drag();
                let keyboard_gesture = state.finish_keyboard_gesture(true);
                if dragging || keyboard_gesture {
                    shell.publish((self.on_finish)());
                }
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            }) if state.drag.is_some() => {
                let origin = state.drag.take().expect("active resize gesture");
                shell.publish(self.resize(origin.width));
                shell.publish((self.on_finish)());
                shell.capture_event();
                return;
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modifiers,
                repeat: _,
                ..
            }) if state.is_focused && !self.modal_open && !chorded(*modifiers) => {
                if let Some(delta) = state.keyboard_resize(self.keyboard_delta(key)) {
                    shell.publish(self.resize(self.width + delta));
                    shell.capture_event();
                    return;
                }
            }
            Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) if state.keyboard_gesture => {
                if state.finish_keyboard_gesture(self.keyboard_delta(key).is_some()) {
                    shell.publish((self.on_finish)());
                    shell.capture_event();
                    return;
                }
            }
            Event::A11y(id, request) if *id == self.id => {
                use iced_accessibility::accesskit::Action;
                match request.action {
                    Action::Focus => state.is_focused = true,
                    Action::Blur => state.is_focused = false,
                    Action::Increment if !self.modal_open => {
                        shell.publish(self.resize(self.width + KEYBOARD_STEP));
                        shell.publish((self.on_finish)());
                    }
                    Action::Decrement if !self.modal_open => {
                        shell.publish(self.resize(self.width - KEYBOARD_STEP));
                        shell.publish((self.on_finish)());
                    }
                    _ => {}
                }
                shell.capture_event();
                return;
            }
            _ => {}
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            Self::child_layout(layout),
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if tree.state.downcast_ref::<SplitterState>().drag.is_some()
            || (!self.modal_open && cursor.is_over(Self::hit_bounds(layout.bounds())))
        {
            mouse::Interaction::ResizingHorizontally
        } else {
            mouse::Interaction::default()
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        if !layout.bounds().intersects(viewport) {
            return;
        }
        let scale = style.scale_factor as f32;
        let mut bounds = crate::separator::physical_bounds(
            layout.bounds(),
            layout.virtual_offset(),
            scale,
            crate::separator::Axis::Vertical,
            self.edge == Edge::Trailing,
        );
        // The same rule every separator paints (`design::rule`): the
        // component-level divider and pre-compositing against the window
        // background both rendered a brighter, stronger line than every
        // sibling border.
        let state = tree.state.downcast_ref::<SplitterState>();
        let active = !self.modal_open
            && (state.drag.is_some() || cursor.is_over(Self::hit_bounds(layout.bounds())));
        let color: Color = if active || state.is_focused {
            crate::design::accent_color(theme)
        } else {
            crate::design::rule(theme)
        };
        if active {
            bounds.x -= 1.0;
            bounds.width = 3.0;
        }
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                border: Border::default(),
                shadow: Shadow::default(),
                snap: false,
            },
            cosmic::iced::Background::Color(color),
        );
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: cosmic::iced::Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            Self::child_layout(layout),
            renderer,
            viewport,
            translation,
        )
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        _state: &Tree,
        _cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        use iced_accessibility::accesskit::{Action, Node, Rect, Role};

        let Rectangle {
            x,
            y,
            width,
            height,
        } = layout.bounds();
        let mut node = Node::new(Role::Splitter);
        node.add_action(Action::Focus);
        node.add_action(Action::Increment);
        node.add_action(Action::Decrement);
        node.set_bounds(Rect::new(
            f64::from(x),
            f64::from(y),
            f64::from(x + width),
            f64::from(y + height),
        ));
        node.set_label(self.label);
        node.set_numeric_value(f64::from(self.width));
        node.set_min_numeric_value(f64::from(self.min));
        node.set_max_numeric_value(f64::from(self.max));
        node.set_numeric_value_step(f64::from(KEYBOARD_STEP));
        iced_accessibility::A11yTree::leaf(node, self.id.clone())
    }

    fn id(&self) -> Option<Id> {
        Some(self.id.clone())
    }

    fn set_id(&mut self, id: Id) {
        self.id = id;
    }
}

impl<'a, Message: 'a> From<Splitter<'a, Message>> for Element<'a, Message> {
    fn from(splitter: Splitter<'a, Message>) -> Self {
        Self::new(splitter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::{
        Font, Pixels,
        advanced::{Layout, clipboard, layout, mouse, renderer::Headless, widget::Tree},
    };
    use iced_accessibility::accesskit::{Action, Role};

    #[test]
    fn fractional_scroll_keeps_a_complete_separator_pixel_inside_the_pane_clip() {
        let mut renderer = headless_renderer();
        let theme = Theme::default();
        for scale in [1.0_f32, 1.25, 1.5, 2.0] {
            for offset in [0.125, 0.5, 0.875] {
                let mut splitter = element(Edge::Trailing);
                let mut tree = Tree::new(&splitter);
                let node = splitter
                    .as_widget_mut()
                    .layout(
                        &mut tree,
                        &renderer,
                        &layout::Limits::new(Size::ZERO, Size::new(WIDTH, 80.0)),
                    )
                    .move_to(Point::new(220.0 - WIDTH, 0.0));
                let viewport =
                    Rectangle::new(Point::new(offset, 0.0), Size::new(220.0 - offset, 80.0));
                renderer.reset(Rectangle::with_size(Size::new(400.0, 80.0)));
                renderer.with_layer(
                    Rectangle::with_size(Size::new(220.0 - offset, 80.0)),
                    |renderer| {
                        renderer.with_translation(
                            cosmic::iced::Vector::new(-offset, 0.0),
                            |renderer| {
                                splitter.as_widget().draw(
                                    &tree,
                                    renderer,
                                    &theme,
                                    &renderer::Style {
                                        scale_factor: scale as f64,
                                        ..Default::default()
                                    },
                                    Layout::new(&node).with_virtual_offset(
                                        cosmic::iced::Vector::new(offset, 0.0),
                                    ),
                                    mouse::Cursor::Unavailable,
                                    &viewport,
                                )
                            },
                        )
                    },
                );
                let size = Size::new((400.0 * scale) as u32, (80.0 * scale) as u32);
                let pixels =
                    renderer.screenshot(size, scale, cosmic::iced::Color::from_rgb(1.0, 0.0, 1.0));
                let x = ((220.0 - offset) * scale).floor() as usize - 1;
                let y = (40.0 * scale) as usize;
                let pixel = (y * size.width as usize + x) * 4;
                assert_ne!(
                    &pixels[pixel..pixel + 3],
                    &[255, 0, 255],
                    "scale={scale}, offset={offset}"
                );
            }
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Message {
        Resize(f32),
        Finished,
    }

    fn element(edge: Edge) -> Element<'static, Message> {
        Splitter::new(
            Id::new("test.splitter"),
            "Resize test pane",
            360.0,
            320.0,
            420.0,
            edge,
            false,
            Message::Resize,
            || Message::Finished,
        )
        .into()
    }

    fn headless_renderer() -> Renderer {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(<Renderer as Headless>::new(
                Font::DEFAULT,
                Pixels::from(16),
                Some("tiny-skia"),
            ))
            .expect("create a headless splitter renderer")
    }

    fn arrow_event(repeat: Option<bool>) -> Event {
        let key = keyboard::Key::Named(keyboard::key::Named::ArrowRight);
        let physical_key = keyboard::key::Physical::Code(keyboard::key::Code::ArrowRight);
        let event = match repeat {
            Some(repeat) => keyboard::Event::KeyPressed {
                key: key.clone(),
                modified_key: key,
                physical_key,
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat,
            },
            None => keyboard::Event::KeyReleased {
                key: key.clone(),
                modified_key: key,
                physical_key,
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
            },
        };
        Event::Keyboard(event)
    }

    fn dispatch_event(
        width: f32,
        event: &Event,
        cursor: mouse::Cursor,
        tree: &mut Tree,
        renderer: &Renderer,
    ) -> Vec<Message> {
        let size = Size::new(WIDTH, 480.0);
        let mut clipboard = clipboard::Null;
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        let viewport = Rectangle::with_size(size);

        // Rebuild the widget with the width the application accepted from the
        // preceding event while preserving its real tree state across repeats.
        let mut element: Element<'static, Message> = Splitter::new(
            Id::new("test.splitter"),
            "Resize test pane",
            width,
            320.0,
            420.0,
            Edge::Trailing,
            false,
            Message::Resize,
            || Message::Finished,
        )
        .into();
        element.as_widget_mut().diff(tree);
        let node =
            element
                .as_widget_mut()
                .layout(tree, renderer, &layout::Limits::new(Size::ZERO, size));
        element.as_widget_mut().update(
            tree,
            event,
            Layout::new(&node),
            cursor,
            renderer,
            &mut clipboard,
            &mut shell,
            &viewport,
        );
        messages
    }

    #[test]
    fn pointer_travel_grows_the_pane_from_either_edge() {
        assert_eq!(
            Splitter::<Message>::pointer_delta(Edge::Trailing, 600.0, 640.0),
            40.0
        );
        assert_eq!(
            Splitter::<Message>::pointer_delta(Edge::Leading, 600.0, 560.0),
            40.0
        );
    }

    #[test]
    fn pointer_drag_tracks_one_origin_and_finishes_once() {
        let mut state = SplitterState::default();
        state.begin_drag(600.0, 360.0);
        assert_eq!(state.dragged_width(Edge::Leading, 560.0), Some(400.0));
        assert_eq!(state.dragged_width(Edge::Leading, 640.0), Some(320.0));
        assert_eq!(state.dragged_width(Edge::Leading, 600.0), Some(360.0));
        assert!(state.finish_drag());
        assert!(!state.finish_drag());
        assert_eq!(state.dragged_width(Edge::Leading, 560.0), None);
    }

    #[test]
    fn keyboard_direction_tracks_the_physical_splitter_edge() {
        let trailing = Splitter::new(
            Id::new("trailing"),
            "trailing",
            360.0,
            320.0,
            420.0,
            Edge::Trailing,
            false,
            Message::Resize,
            || Message::Finished,
        );
        let leading = Splitter::new(
            Id::new("leading"),
            "leading",
            360.0,
            320.0,
            420.0,
            Edge::Leading,
            false,
            Message::Resize,
            || Message::Finished,
        );
        let right = keyboard::Key::Named(keyboard::key::Named::ArrowRight);
        assert_eq!(trailing.keyboard_delta(&right), Some(KEYBOARD_STEP));
        assert_eq!(leading.keyboard_delta(&right), Some(-KEYBOARD_STEP));
    }

    #[test]
    fn single_and_repeated_keyboard_gestures_publish_each_resize_and_finish_once_at_release() {
        let renderer = headless_renderer();
        for repeats in [0, 3] {
            let initial = element(Edge::Trailing);
            let mut tree = Tree::new(&initial);
            tree.state.downcast_mut::<SplitterState>().is_focused = true;
            let mut width = 360.0;
            let mut resize_publications = 0;
            let mut finish_publications = 0;

            for event in std::iter::once(arrow_event(Some(false)))
                .chain((0..repeats).map(|_| arrow_event(Some(true))))
            {
                let messages = dispatch_event(
                    width,
                    &event,
                    mouse::Cursor::Unavailable,
                    &mut tree,
                    &renderer,
                );
                assert_eq!(messages.len(), 1, "every press must publish one resize");
                match messages[0] {
                    Message::Resize(next) => {
                        width = next;
                        resize_publications += 1;
                    }
                    Message::Finished => panic!("keyboard resize finished before key release"),
                }
            }

            for message in dispatch_event(
                width,
                &arrow_event(None),
                mouse::Cursor::Unavailable,
                &mut tree,
                &renderer,
            ) {
                match message {
                    Message::Resize(_) => panic!("key release published an extra resize"),
                    Message::Finished => finish_publications += 1,
                }
            }
            finish_publications += dispatch_event(
                width,
                &arrow_event(None),
                mouse::Cursor::Unavailable,
                &mut tree,
                &renderer,
            )
            .into_iter()
            .filter(|message| matches!(message, Message::Finished))
            .count();

            assert_eq!(resize_publications, repeats + 1);
            assert_eq!(finish_publications, 1);
            assert_eq!(
                width,
                (360.0 + KEYBOARD_STEP * (repeats + 1) as f32).min(420.0)
            );
        }
    }

    #[test]
    fn pointer_drag_widget_events_publish_live_resizes_and_one_finish() {
        let renderer = headless_renderer();
        let initial = element(Edge::Trailing);
        let mut tree = Tree::new(&initial);
        let mut width = 360.0;
        let start = Point::new(2.0, 120.0);

        assert!(
            dispatch_event(
                width,
                &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                mouse::Cursor::Available(start),
                &mut tree,
                &renderer,
            )
            .is_empty()
        );

        let moved = dispatch_event(
            width,
            &Event::Mouse(mouse::Event::CursorMoved {
                position: Point::new(50.0, 120.0),
            }),
            mouse::Cursor::Available(Point::new(50.0, 120.0)),
            &mut tree,
            &renderer,
        );
        assert_eq!(moved, vec![Message::Resize(408.0)]);
        width = 408.0;

        assert_eq!(
            dispatch_event(
                width,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                mouse::Cursor::Available(Point::new(50.0, 120.0)),
                &mut tree,
                &renderer,
            ),
            vec![Message::Finished]
        );
        assert!(
            dispatch_event(
                width,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                mouse::Cursor::Available(Point::new(50.0, 120.0)),
                &mut tree,
                &renderer,
            )
            .is_empty()
        );
    }

    #[test]
    fn splitter_exposes_focus_and_numeric_accessibility_actions() {
        let element = element(Edge::Leading);
        assert_eq!(
            element.as_widget().id().map(|id| id.to_string()),
            Some("test.splitter".into())
        );
        let tree = Tree::new(&element);
        let bounds = Size::new(WIDTH, 480.0);
        let node = layout::Node::with_children(bounds, vec![layout::Node::new(bounds)]);
        let accessible =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let node = accessible.root()[0].node();
        assert_eq!(node.role(), Role::Splitter);
        assert_eq!(node.label(), Some("Resize test pane"));
        assert_eq!(node.numeric_value(), Some(360.0));
        assert_eq!(node.min_numeric_value(), Some(320.0));
        assert_eq!(node.max_numeric_value(), Some(420.0));
        assert!(node.supports_action(Action::Increment));
        assert!(node.supports_action(Action::Decrement));
    }
}
