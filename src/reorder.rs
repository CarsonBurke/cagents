//! Local pointer reordering inside native horizontal scrollables.
//!
//! Layout, cursor and viewport all stay in the scrollable's content coordinates.
//! Only a completed drop reaches the application; pointer motion never mutates
//! its order or clones its editor/terminal state.

use crate::{design, model::{CardId, ChannelId}};
use cosmic::iced::advanced::Renderer as _;
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree, tree},
};
use cosmic::iced::{Event, Length, Point, Rectangle, Size, Vector, keyboard, mouse, window};
use cosmic::{Element, Renderer, Theme};

const THRESHOLD: f32 = 6.0;

#[derive(Clone, Copy, Debug)]
pub struct Item {
    pub id: CardId,
    pub start: f32,
    pub width: f32,
}

/// The destination after removing `source`, expressed by a stable successor.
pub fn destination(order: &[CardId], source: CardId, before: Option<CardId>) -> Option<usize> {
    let from = order.iter().position(|id| *id == source)?;
    let to = match before {
        Some(id) if id == source => return None,
        Some(id) => order.iter().position(|candidate| *candidate == id)?,
        None => order.len(),
    };
    Some(to - usize::from(from < to))
}

/// Marks the title region, leaving adjacent close and action buttons native.
pub fn handle<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    id: CardId,
) -> Element<'a, Message> {
    Element::new(Reorder {
        content: content.into(),
        kind: Kind::Handle(id),
    })
}

/// Wrap content *inside* its horizontal scrollable. Alt mode lets a pane be
/// dragged from anywhere; tab mode starts only at a marked title handle.
pub fn strip<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    items: Vec<Item>,
    alt: bool,
    enabled: bool,
    scroll: Option<Box<dyn Fn(f32) -> Message + 'a>>,
    select: impl Fn(CardId) -> Message + 'a,
    dropped: impl Fn(CardId, Option<CardId>) -> Message + 'a,
) -> Element<'a, Message> {
    Element::new(Reorder {
        content: content.into(),
        kind: Kind::Strip {
            items,
            alt,
            enabled,
            scroll,
            select: Box::new(select),
            dropped: Box::new(dropped),
        },
    })
}

/// A sidebar channel can receive an existing pane without becoming a drag source.
pub fn channel_target<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>, channel: ChannelId, enabled: bool,
) -> Element<'a, Message> {
    Element::new(Reorder { content: content.into(), kind: Kind::ChannelTarget { channel, enabled } })
}

/// Observe tab drags across the entire native surface, keeping ordinary strip
/// reordering local until a valid channel actually receives the release.
pub fn channel_scope<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>, source_strip: Id, generation: u64, enabled: bool,
    allowed: impl Fn(CardId, ChannelId) -> bool + 'a,
    dropped: impl Fn(CardId, ChannelId) -> Message + 'a,
) -> Element<'a, Message> {
    Element::new(Reorder { content: content.into(), kind: Kind::ChannelScope {
        source_strip, generation, enabled, allowed: Box::new(allowed), dropped: Box::new(dropped),
    } })
}

enum Kind<'a, Message> {
    Handle(CardId),
    ChannelTarget { channel: ChannelId, enabled: bool },
    ChannelScope {
        source_strip: Id, generation: u64, enabled: bool,
        allowed: Box<dyn Fn(CardId, ChannelId) -> bool + 'a>,
        dropped: Box<dyn Fn(CardId, ChannelId) -> Message + 'a>,
    },
    Strip {
        items: Vec<Item>,
        alt: bool,
        enabled: bool,
        scroll: Option<Box<dyn Fn(f32) -> Message + 'a>>,
        select: Box<dyn Fn(CardId) -> Message + 'a>,
        dropped: Box<dyn Fn(CardId, Option<CardId>) -> Message + 'a>,
    },
}
struct Reorder<'a, Message> {
    content: Element<'a, Message>,
    kind: Kind<'a, Message>,
}
#[derive(Default)]
struct State {
    modifiers: keyboard::Modifiers,
    drag: Option<Drag>,
    suppress_release: bool,
    channel_drag: Option<ChannelDrag>,
    channel_targeted: bool,
}
struct ChannelDrag { source: CardId, origin: Point, generation: u64, moved: bool }
struct ChannelTarget { channel: ChannelId, enabled: bool, highlighted: bool }
struct WindowHandle(CardId);
struct ChannelDropBlocker;

/// Sticky headers cover scrolled member rows; the covered channel is not a target.
pub(crate) fn block_channel_drop(operation: &mut dyn Operation, bounds: Rectangle) {
    operation.custom(None, bounds, &mut ChannelDropBlocker);
}

struct ChannelHit {
    point: Point, source_strip: Id, strip_bounds: Option<Rectangle>,
    source: Option<CardId>, target: Option<ChannelId>,
    target_viewport: Option<Rectangle>, blocked: bool,
}
impl Operation for ChannelHit {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) { visit(self); }
    fn scrollable(&mut self, id: Option<&Id>, bounds: Rectangle, _: Rectangle, _: Vector,
        _: &mut dyn cosmic::iced::advanced::widget::operation::Scrollable) {
        if id == Some(&self.source_strip) { self.strip_bounds = Some(bounds); }
        if id == Some(&Id::new("sidebar.projects.scroll")) { self.target_viewport = Some(bounds); }
    }
    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if !bounds.contains(self.point) { return; }
        if let Some(handle) = state.downcast_ref::<WindowHandle>() { self.source = Some(handle.0); }
        if state.is::<ChannelDropBlocker>() { self.blocked = true; }
        if let Some(target) = state.downcast_ref::<ChannelTarget>().filter(|target| target.enabled) {
            self.target = Some(target.channel);
        }
    }
}
struct HighlightChannel(Option<ChannelId>);
impl Operation for HighlightChannel {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) { visit(self); }
    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(target) = state.downcast_mut::<ChannelTarget>() {
            target.highlighted = target.enabled && self.0 == Some(target.channel);
        }
    }
}
struct Drag {
    source: CardId,
    origin: Point,
    position: Point,
    moved: bool,
    order: Vec<CardId>,
}
struct Handle(CardId);
struct Hit {
    point: Point,
    found: Option<CardId>,
}
impl Operation for Hit {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if bounds.contains(self.point)
            && let Some(handle) = state.downcast_ref::<Handle>()
        {
            self.found = Some(handle.0);
        }
    }
}

fn before(items: &[Item], source: CardId, x: f32) -> Option<CardId> {
    items
        .iter()
        .find(|item| item.id != source && x < item.start + item.width / 2.0)
        .map(|item| item.id)
}

impl<Message> Widget<Message, Theme, Renderer> for Reorder<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        if let Kind::Strip { items, enabled, .. } = &self.kind {
            if !enabled
                || state.drag.as_ref().is_some_and(|drag| {
                    !items
                        .iter()
                        .map(|item| item.id)
                        .eq(drag.order.iter().copied())
                })
            {
                state.suppress_release |= state.drag.take().is_some();
            }
        }
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
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        if let Kind::Handle(id) = self.kind {
            operation.custom(None, layout.bounds(), &mut Handle(id));
            operation.custom(None, layout.bounds() - layout.virtual_offset(), &mut WindowHandle(id));
        }
        if let Kind::ChannelTarget { channel, enabled } = self.kind {
            let state = tree.state.downcast_mut::<State>();
            let mut target = ChannelTarget { channel, enabled, highlighted: state.channel_targeted };
            operation.custom(None, layout.bounds() - layout.virtual_offset(), &mut target);
            state.channel_targeted = target.highlighted;
        }
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
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
        if let Kind::ChannelScope { source_strip, generation, enabled, allowed, dropped } = &self.kind {
            let mut drag = tree.state.downcast_mut::<State>().channel_drag.take();
            let was_dragging = drag.is_some();
            let cancel = !enabled || drag.as_ref().is_some_and(|drag| drag.generation != *generation)
                || matches!(event, Event::Window(window::Event::Unfocused))
                || matches!(event, Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape), .. }));
            if cancel { drag = None; }
            let observe = drag.is_some() || (*enabled && matches!(event,
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))));
            let mut hit = observe.then(|| cursor.position()).flatten().map(|point| ChannelHit {
                point, source_strip: source_strip.clone(), strip_bounds: None, source: None, target: None,
                target_viewport: None, blocked: false,
            });
            if let Some(hit) = hit.as_mut() {
                self.content.as_widget_mut().operate(&mut tree.children[0], layout, renderer, hit);
            }
            if *enabled && !shell.is_event_captured()
                && matches!(event, Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)))
                && let Some(hit) = hit.as_ref()
                && hit.strip_bounds.is_some_and(|bounds| bounds.contains(hit.point))
                && let Some(source) = hit.source
            {
                drag = Some(ChannelDrag { source, origin: hit.point, generation: *generation, moved: false });
            }
            if let (Some(drag), Some(hit)) = (drag.as_mut(), hit.as_ref()) {
                drag.moved |= drag.origin.distance(hit.point) >= THRESHOLD;
            }
            let destination = drag.as_ref().filter(|drag| drag.moved).and_then(|drag| {
                hit.as_ref().filter(|hit| !hit.blocked && hit.target_viewport.is_none_or(|bounds| bounds.contains(hit.point)))
                    .and_then(|hit| hit.target).filter(|channel| allowed(drag.source, *channel))
            });
            if drag.is_some() || was_dragging {
                self.content.as_widget_mut().operate(&mut tree.children[0], layout, renderer,
                    &mut HighlightChannel(destination));
            }
            if matches!(event, Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))) {
                if let Some((drag, channel)) = drag.take().zip(destination) {
                    // Cancel the source strip's held press before dispatching
                    // the transfer; no adjacent tab or close button may fire.
                    self.content.as_widget_mut().update(&mut tree.children[0], event, layout,
                        mouse::Cursor::Unavailable, renderer, clipboard, shell, viewport);
                    self.content.as_widget_mut().operate(&mut tree.children[0], layout, renderer,
                        &mut HighlightChannel(None));
                    shell.publish(dropped(drag.source, channel));
                    shell.capture_event();
                    shell.request_redraw();
                    return;
                }
                drag = None;
            }
            if ((drag.is_some() || was_dragging) && matches!(event, Event::Mouse(
                mouse::Event::CursorMoved { .. } | mouse::Event::ButtonPressed(_) | mouse::Event::ButtonReleased(_))))
                || (cancel && was_dragging) { shell.request_redraw(); }
            tree.state.downcast_mut::<State>().channel_drag = drag;
        }
        if let Kind::Strip {
            items,
            alt,
            enabled,
            scroll,
            select,
            dropped,
        } = &self.kind
        {
            let state = tree.state.downcast_mut::<State>();
            if let Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) = event {
                state.modifiers = *modifiers;
            }
            if matches!(event, Event::Window(window::Event::Unfocused)) {
                state.modifiers = keyboard::Modifiers::empty();
            }
            let cancel = !enabled
                || matches!(event, Event::Window(window::Event::Unfocused))
                || matches!(
                    event,
                    Event::Keyboard(keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(keyboard::key::Named::Escape),
                        ..
                    })
                )
                || (*alt && !state.modifiers.alt());
            if cancel && state.drag.take().is_some() {
                state.suppress_release = true;
                shell.request_redraw();
            }
            if matches!(
                event,
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            ) {
                state.suppress_release = false;
            }
            if matches!(
                event,
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            ) && std::mem::take(&mut state.suppress_release)
            {
                shell.capture_event();
                return;
            }
            let clipped = layout.bounds().intersection(viewport);
            if !cancel
                && !shell.is_event_captured()
                && matches!(
                    event,
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                )
                && let Some(point) = cursor
                    .position()
                    .filter(|point| clipped.is_some_and(|bounds| bounds.contains(*point)))
            {
                let source = if *alt {
                    let x = point.x - layout.bounds().x;
                    items
                        .iter()
                        .find(|item| x >= item.start && x <= item.start + item.width)
                        .map(|item| item.id)
                } else {
                    let mut hit = Hit { point, found: None };
                    self.content.as_widget_mut().operate(
                        &mut tree.children[0],
                        layout,
                        renderer,
                        &mut hit,
                    );
                    hit.found
                        .filter(|id| items.iter().any(|item| item.id == *id))
                };
                if let Some(source) = source {
                    state.drag = Some(Drag {
                        source,
                        origin: point,
                        position: point,
                        moved: false,
                        order: items.iter().map(|item| item.id).collect(),
                    });
                    shell.capture_event();
                    return;
                }
            }
            if let Some(drag) = state.drag.as_mut() {
                if let Some(point) = cursor.position() {
                    drag.position = point;
                    drag.moved |= point.distance(drag.origin) >= THRESHOLD;
                }
                if matches!(
                    event,
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                ) {
                    let drag = state.drag.take().unwrap();
                    if cursor
                        .position()
                        .is_some_and(|point| clipped.is_some_and(|bounds| bounds.contains(point)))
                    {
                        if drag.moved {
                            let successor =
                                before(items, drag.source, drag.position.x - layout.bounds().x);
                            if destination(&drag.order, drag.source, successor)
                                != drag.order.iter().position(|id| *id == drag.source)
                            {
                                shell.publish(dropped(drag.source, successor));
                            }
                        } else {
                            shell.publish(select(drag.source));
                        }
                    }
                    shell.capture_event();
                    shell.request_redraw();
                    return;
                }
                if matches!(event, Event::Mouse(mouse::Event::CursorMoved { .. })) {
                    shell.capture_event();
                    shell.request_redraw();
                    return;
                }
                // A wheel while held must reach the enclosing scrollable so
                // users can move to tabs beyond the visible strip.
                if let Event::Mouse(mouse::Event::WheelScrolled { delta }) = event {
                    if let Some(scroll) = scroll {
                        let (x, y, scale) = match *delta {
                            mouse::ScrollDelta::Lines { x, y } => (x, y, 60.0),
                            mouse::ScrollDelta::Pixels { x, y } => (x, y, 1.0),
                        };
                        let movement = if x == 0.0 { y } else { x } * scale;
                        let offset = viewport.x - layout.bounds().x;
                        let maximum = (layout.bounds().width - viewport.width).max(0.0);
                        shell.publish(scroll((offset - movement).clamp(0.0, maximum)));
                        shell.capture_event();
                    }
                    return;
                }
            }
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
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
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        if tree.state.downcast_ref::<State>().channel_drag.as_ref().is_some_and(|drag| drag.moved) {
            return mouse::Interaction::Grabbing;
        }
        if tree
            .state
            .downcast_ref::<State>()
            .drag
            .as_ref()
            .is_some_and(|drag| drag.moved)
        {
            return mouse::Interaction::Grabbing;
        }
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
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
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        if matches!(self.kind, Kind::ChannelTarget { .. })
            && tree.state.downcast_ref::<State>().channel_targeted
            && let Some(bounds) = layout.bounds().intersection(viewport)
        {
            renderer.fill_quad(renderer::Quad { bounds, border: cosmic::iced::Border {
                color: design::accent_color(theme), width: 1.0, radius: 4.0.into(),
            }, ..Default::default() }, design::accent_color(theme).scale_alpha(0.12));
        }
        if let Kind::Strip { items, .. } = &self.kind
            && let Some(drag) = tree
                .state
                .downcast_ref::<State>()
                .drag
                .as_ref()
                .filter(|drag| drag.moved)
            && let Some(clipped) = layout.bounds().intersection(viewport)
        {
            let bounds = layout.bounds();
            let successor = before(items, drag.source, drag.position.x - bounds.x);
            let x = successor
                .and_then(|id| {
                    items
                        .iter()
                        .find(|item| item.id == id)
                        .map(|item| item.start)
                })
                .or_else(|| items.last().map(|item| item.start + item.width))
                .unwrap_or(0.0);
            let accent = design::accent_color(theme);
            renderer.with_layer(clipped, |renderer| {
                if let Some(source) = items.iter().find(|item| item.id == drag.source) {
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds: Rectangle {
                                x: bounds.x + source.start,
                                y: bounds.y,
                                width: source.width,
                                height: bounds.height.min(design::PRIMARY_CONTROL_SIZE),
                            },
                            ..Default::default()
                        },
                        accent.scale_alpha(0.15),
                    );
                }
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: (bounds.x + x - 1.0)
                                .clamp(clipped.x, (clipped.x + clipped.width - 2.0).max(clipped.x)),
                            y: bounds.y,
                            width: 2.0,
                            height: bounds.height,
                        },
                        ..Default::default()
                    },
                    accent,
                );
            });
        }
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor)
    }
    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            layout,
            renderer,
            destinations,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::{
        iced::{
            Font, Pixels,
            advanced::{clipboard, renderer::Headless},
        },
        widget,
    };

    #[derive(Debug, Clone, PartialEq)]
    enum Action {
        Select(CardId),
        Close(CardId),
        Drop(CardId, Option<CardId>),
        Scroll(f32),
        Move(CardId, ChannelId),
    }

    fn tabs(ids: &[CardId], alt: bool) -> Element<'static, Action> {
        let row = widget::row::with_children(ids.iter().map(|&id| {
            crate::tab_strip::middle_click_close(
                widget::row::with_children(vec![
                    handle(
                        widget::button::custom(cosmic::iced::widget::Space::new())
                            .width(60)
                            .height(30)
                            .padding(0)
                            .on_press(Action::Select(id)),
                        id,
                    ),
                    widget::button::custom(cosmic::iced::widget::Space::new())
                        .width(20)
                        .height(30)
                        .padding(0)
                        .on_press(Action::Close(id))
                        .into(),
                ]),
                Action::Close(id),
            )
        }))
        .height(30);
        let content = strip(
            row,
            ids.iter()
                .enumerate()
                .map(|(index, &id)| Item {
                    id,
                    start: index as f32 * 80.0,
                    width: 80.0,
                })
                .collect(),
            alt,
            true,
            alt.then(|| Box::new(Action::Scroll) as Box<dyn Fn(f32) -> Action>),
            Action::Select,
            Action::Drop,
        );
        crate::interaction::horizontal_wheel(
            widget::scrollable::horizontal(content)
                .id(widget::Id::new("reorder.test"))
                .direction(cosmic::iced::widget::scrollable::Direction::Horizontal(
                    cosmic::iced::widget::scrollable::Scrollbar::hidden(),
                ))
                .width(160)
                .height(30),
        )
    }

    struct Surface {
        element: Element<'static, Action>,
        tree: Tree,
        renderer: Renderer,
        node: layout::Node,
    }
    impl Surface {
        fn new(ids: &[CardId], alt: bool) -> Self {
            let renderer = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(Renderer::new(
                    Font::default(),
                    Pixels(14.0),
                    Some("tiny-skia"),
                ))
                .unwrap();
            let mut element = tabs(ids, alt);
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(160.0, 30.0)),
            );
            Self {
                element,
                tree,
                renderer,
                node,
            }
        }
        fn event(&mut self, event: Event, x: f32) -> Vec<Action> {
            let mut messages = Vec::new();
            self.element.as_widget_mut().update(
                &mut self.tree,
                &event,
                Layout::new(&self.node),
                mouse::Cursor::Available(Point::new(x, 15.0)),
                &self.renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &Rectangle::with_size(Size::new(160.0, 30.0)),
            );
            messages
        }
        fn scroll(&mut self, x: f32) {
            self.element.as_widget_mut().operate(
                &mut self.tree,
                Layout::new(&self.node),
                &self.renderer,
                &mut cosmic::iced::advanced::widget::operation::scrollable::scroll_to::<()>(
                    widget::Id::new("reorder.test"),
                    cosmic::iced::widget::scrollable::AbsoluteOffset {
                        x: Some(x),
                        y: Some(0.0),
                    },
                ),
            );
        }
        fn press(&mut self, x: f32) -> Vec<Action> {
            self.event(
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                x,
            )
        }
        fn moved(&mut self, x: f32) -> Vec<Action> {
            self.event(
                Event::Mouse(mouse::Event::CursorMoved {
                    position: Point::new(x, 15.0),
                }),
                x,
            )
        }
        fn release(&mut self, x: f32) -> Vec<Action> {
            self.event(
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                x,
            )
        }
    }

    #[test]
    fn dropping_a_tab_on_a_channel_transfers_without_selecting_or_reordering() {
        for (allowed, scroll) in [(false, 0.0), (true, 0.0), (true, 80.0)] {
            let ids = [CardId::new(), CardId::new(), CardId::new(), CardId::new()];
            let channel = ChannelId::new();
            let mut surface = Surface::new(&ids, false);
            let target = channel_target(widget::button::custom(widget::Space::new())
                .width(120).height(30).on_press(Action::Select(ids[1])), channel, true);
            surface.element = channel_scope(widget::row::with_children(vec![target, tabs(&ids, false)]),
                widget::Id::new("reorder.test"), 1, true, move |_, _| allowed, Action::Move);
            surface.tree = Tree::new(surface.element.as_widget());
            surface.node = surface.element.as_widget_mut().layout(&mut surface.tree, &surface.renderer,
                &layout::Limits::new(Size::ZERO, Size::new(280.0, 30.0)));
            surface.scroll(scroll);
            assert!(surface.press(130.0).is_empty());
            assert!(surface.moved(20.0).is_empty());
            let moved = if scroll > 0.0 { ids[1] } else { ids[0] };
            assert_eq!(surface.release(20.0), if allowed { vec![Action::Move(moved, channel)] } else { vec![] });
            assert!(surface.release(130.0).is_empty(), "the original held tab press was cancelled");
        }
    }

    #[test]
    fn changing_workspaces_cancels_a_retained_channel_drag() {
        let ids = [CardId::new(), CardId::new()];
        let channel = ChannelId::new();
        let make = |generation| {
            let target = channel_target(widget::button::custom(widget::Space::new())
                .width(120).height(30).on_press(Action::Select(ids[1])), channel, true);
            channel_scope(widget::row::with_children(vec![target, tabs(&ids, false)]),
                widget::Id::new("reorder.test"), generation, true, |_, _| true, Action::Move)
        };
        let mut surface = Surface::new(&ids, false);
        surface.element = make(1);
        surface.tree = Tree::new(surface.element.as_widget());
        surface.node = surface.element.as_widget_mut().layout(&mut surface.tree, &surface.renderer,
            &layout::Limits::new(Size::ZERO, Size::new(280.0, 30.0)));
        assert!(surface.press(130.0).is_empty());
        assert!(surface.moved(20.0).is_empty());
        surface.element = make(2);
        surface.tree.diff(surface.element.as_widget_mut());
        surface.node = surface.element.as_widget_mut().layout(&mut surface.tree, &surface.renderer,
            &layout::Limits::new(Size::ZERO, Size::new(280.0, 30.0)));
        assert!(surface.release(20.0).is_empty(), "a stale drag must never move the prior workspace's pane");
    }

    #[test]
    fn queued_pointer_events_keep_their_targets_across_view_rebuilds() {
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};

        let ids = [CardId::new(), CardId::new()];
        let mut renderer = Surface::new(&ids, false).renderer;
        let moved = |x| {
            Event::Mouse(mouse::Event::CursorMoved {
                position: Point::new(x, 15.0),
            })
        };
        let press = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
        let release = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));

        for rebuild in [false, true] {
            let mut ui = UserInterface::build(
                tabs(&ids, false),
                Size::new(160.0, 30.0),
                Cache::new(),
                &mut renderer,
            );
            let mut messages = Vec::new();
            let events = if rebuild {
                let _ = ui.update(
                    &[moved(10.0)],
                    mouse::Cursor::Available(Point::new(10.0, 15.0)),
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
                ui = UserInterface::build(
                    tabs(&ids, false),
                    Size::new(160.0, 30.0),
                    ui.into_cache(),
                    &mut renderer,
                );
                // Winit may redraw at the batch's final pointer position before
                // draining the queued press/move/release at AboutToWait.
                let _ = ui.update(
                    &[Event::Window(cosmic::iced::window::Event::RedrawRequested(
                        std::time::Instant::now(),
                    ))],
                    mouse::Cursor::Available(Point::new(150.0, 15.0)),
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
                vec![press.clone(), moved(150.0), release.clone()]
            } else {
                vec![moved(10.0), press.clone(), moved(150.0), release.clone()]
            };
            let (_, statuses) = ui.update(
                &events,
                mouse::Cursor::Available(Point::new(150.0, 15.0)),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(statuses.len(), events.len());
            assert_eq!(
                messages,
                vec![Action::Drop(ids[0], None)],
                "rebuild={rebuild}"
            );
        }

        // A complete click must also retain its target when the pointer moves
        // elsewhere before the window dispatches the pending input.
        let mut ui = UserInterface::build(
            tabs(&ids, false),
            Size::new(160.0, 30.0),
            Cache::new(),
            &mut renderer,
        );
        let mut messages = Vec::new();
        let _ = ui.update(
            &[moved(70.0), press, release, moved(150.0)],
            mouse::Cursor::Available(Point::new(150.0, 15.0)),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(messages, vec![Action::Close(ids[0])]);

        // Leaving the window after a completed click must not erase that
        // click's position; a release after leaving must not close anything.
        for (events, expected) in [
            (
                vec![
                    moved(70.0),
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                    Event::Mouse(mouse::Event::CursorLeft),
                ],
                vec![Action::Close(ids[0])],
            ),
            (
                vec![
                    moved(70.0),
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                    Event::Mouse(mouse::Event::CursorLeft),
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                ],
                vec![],
            ),
        ] {
            let mut ui = UserInterface::build(
                tabs(&ids, false),
                Size::new(160.0, 30.0),
                Cache::new(),
                &mut renderer,
            );
            let mut messages = Vec::new();
            let _ = ui.update(
                &events,
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(messages, expected);
            messages.clear();
            ui = UserInterface::build(
                tabs(&ids, false),
                Size::new(160.0, 30.0),
                ui.into_cache(),
                &mut renderer,
            );
            // Wayland can retain a stale backend position after CursorLeft.
            let _ = ui.update(
                &[
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                ],
                mouse::Cursor::Available(Point::new(70.0, 15.0)),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert!(messages.is_empty());

            // Some backends report enter without a separate motion. Recover
            // the supplied position at the enter itself, after the cached
            // leave has already protected the preceding stale click.
            ui = UserInterface::build(
                tabs(&ids, false),
                Size::new(160.0, 30.0),
                ui.into_cache(),
                &mut renderer,
            );
            let _ = ui.update(
                &[
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                    Event::Mouse(mouse::Event::CursorEntered),
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                ],
                mouse::Cursor::Available(Point::new(70.0, 15.0)),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(messages, vec![Action::Close(ids[0])]);
        }
    }

    #[test]
    fn clicks_below_threshold_select_and_close_controls_do_not_arm_a_drag() {
        let ids = [CardId::new(), CardId::new(), CardId::new()];
        let mut surface = Surface::new(&ids, false);
        assert!(surface.press(10.0).is_empty());
        assert!(surface.moved(14.0).is_empty());
        assert_eq!(surface.release(14.0), vec![Action::Select(ids[0])]);
        assert!(surface.press(70.0).is_empty());
        assert_eq!(surface.release(70.0), vec![Action::Close(ids[0])]);
        assert!(surface.press(70.0).is_empty());
        assert!(surface.moved(140.0).is_empty());
        assert!(surface.release(140.0).is_empty());
        assert_eq!(
            surface.event(
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Middle)),
                10.0
            ),
            vec![Action::Close(ids[0])]
        );
    }

    #[test]
    fn scrolled_drag_uses_content_coordinates_and_wheel_reaches_offscreen_tabs() {
        let ids = [CardId::new(), CardId::new(), CardId::new(), CardId::new()];
        let mut surface = Surface::new(&ids, false);
        surface.scroll(80.0);
        assert!(surface.press(10.0).is_empty());
        assert!(surface.moved(20.0).is_empty());
        assert!(
            surface
                .event(
                    Event::Mouse(mouse::Event::WheelScrolled {
                        delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -80.0 }
                    }),
                    20.0
                )
                .is_empty()
        );
        assert!(surface.moved(140.0).is_empty());
        assert_eq!(surface.release(140.0), vec![Action::Drop(ids[1], None)]);
    }

    #[test]
    fn alt_pane_drag_reserves_content_only_with_alt_and_scrolls_while_held() {
        let ids = [CardId::new(), CardId::new(), CardId::new(), CardId::new()];
        let mut surface = Surface::new(&ids, true);
        assert!(surface.press(70.0).is_empty());
        assert_eq!(surface.release(70.0), vec![Action::Close(ids[0])]);
        surface.event(
            Event::Keyboard(keyboard::Event::ModifiersChanged(keyboard::Modifiers::ALT)),
            70.0,
        );
        assert!(surface.press(70.0).is_empty());
        assert!(surface.moved(80.0).is_empty());
        assert_eq!(
            surface.event(
                Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -80.0 }
                }),
                80.0
            ),
            vec![Action::Scroll(80.0)]
        );
        surface.scroll(80.0);
        assert_eq!(
            surface.release(140.0),
            vec![Action::Drop(ids[0], Some(ids[3]))]
        );
    }

    #[test]
    fn blur_escape_modifier_release_and_removed_targets_cancel_drag() {
        let ids = [CardId::new(), CardId::new(), CardId::new()];
        let escape = Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        });
        for cancel in [Event::Window(window::Event::Unfocused), escape] {
            let mut surface = Surface::new(&ids, false);
            surface.press(10.0);
            surface.moved(140.0);
            assert!(surface.event(cancel, 140.0).is_empty());
            assert!(surface.release(140.0).is_empty());
        }
        let mut surface = Surface::new(&ids, true);
        surface.event(
            Event::Keyboard(keyboard::Event::ModifiersChanged(keyboard::Modifiers::ALT)),
            10.0,
        );
        surface.press(10.0);
        surface.moved(140.0);
        surface.event(
            Event::Keyboard(keyboard::Event::ModifiersChanged(
                keyboard::Modifiers::empty(),
            )),
            140.0,
        );
        assert!(surface.release(140.0).is_empty());
        surface.event(
            Event::Keyboard(keyboard::Event::ModifiersChanged(keyboard::Modifiers::ALT)),
            70.0,
        );
        surface.press(70.0);
        surface.event(Event::Window(window::Event::Unfocused), 70.0);
        // Alt can be released in another window without this widget seeing a
        // modifier event. A fresh plain press must reach the native control.
        assert!(surface.press(70.0).is_empty());
        assert_eq!(surface.release(70.0), vec![Action::Close(ids[0])]);
        let mut surface = Surface::new(&ids, false);
        surface.press(10.0);
        surface.moved(140.0);
        let mut replacement = tabs(&ids[..2], false);
        surface.tree.diff(replacement.as_widget_mut());
        surface.element = replacement;
        assert!(surface.release(140.0).is_empty());
    }
}
