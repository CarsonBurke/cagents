//! Shell effects and input layers that retain native accessibility trees.

use std::{borrow::Cow, cell::RefCell, sync::Arc, time::Duration};

use cosmic::cosmic_theme::{Component, Container, palette::Srgba};
use cosmic::iced::advanced::Renderer as _;
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree, operation, tree},
};
use cosmic::iced::{
    Animation, Background, Border, Color, Event, Gradient, Length, Point, Radians, Rectangle,
    Shadow, Size, Vector, gradient::Linear, mouse, time::Instant, window,
};
use cosmic::{Element, Renderer, Theme, widget};
use serde::Deserialize;

mod context_menu;

const HOVER_DURATION: Duration = Duration::from_millis(120);
const TOOLTIP_DELAY: Duration = Duration::from_millis(300);

/// Reserve tab-history keys before editor widgets can treat Tab as indentation.
/// The application's keyboard subscription owns navigation and Control release.
pub fn reserve_tab_history<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    enabled: bool,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::TabHistory(enabled),
        tip: None,
    }
    .into()
}

/// Soft full-bounds hover feedback, including when a child captures the event.
/// The native animation clock requests frames only until its target is reached.
pub fn hover<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::Hover,
        tip: None,
    }
    .into()
}

/// Keeps passive window-frame subscriptions advancing only while state animates.
pub fn drive_animation<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    active: bool,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::DriveAnimation(active),
        tip: None,
    }
    .into()
}

/// Reveal a right-anchored pane without reflowing its contents on every frame.
pub fn reveal_width<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    width: f32,
    progress: f32,
) -> Element<'a, Message> {
    reveal_width_aligned(content, width, progress, false)
}

/// Treat a vertical mouse wheel as horizontal motion.
///
/// iced's native scrollable only swaps axes while Shift is held. Tab strips
/// are horizontal-only, so a normal wheel should move them without a modifier.
pub fn horizontal_wheel<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(WheelInput {
        content: content.into(),
        pass_to_parent: false,
    })
}

/// Lets the enclosing native scrollable own wheel gestures over a read-only
/// editor while preserving the editor's selection and keyboard handling.
pub fn parent_wheel<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(WheelInput {
        content: content.into(),
        pass_to_parent: true,
    })
}

/// Fade the leading/trailing edges of a horizontal strip when it can scroll.
///
/// Tab rows hide their scrollbar; the fade is the usual hint that more tabs
/// sit off-screen.
pub fn overflow_fade_x<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(OverflowFadeX {
        content: content.into(),
    })
}

/// Projects a wheel gesture onto the horizontal axis.
///
/// Summing the two components cancels a diagonal trackpad gesture exactly - a
/// gesture reporting `x = -d, y = +d` sums to zero - and [`WheelInput`]
/// has already replaced the original event by then, so the motion is discarded
/// rather than forwarded. A real horizontal delta therefore wins outright, and
/// only a purely vertical one is remapped.
fn wheel_as_horizontal(delta: mouse::ScrollDelta) -> Event {
    fn horizontal(x: f32, y: f32) -> f32 {
        if x == 0.0 { y } else { x }
    }
    let x = match delta {
        mouse::ScrollDelta::Lines { x, y } => horizontal(x, y) * 60.0,
        mouse::ScrollDelta::Pixels { x, y } => horizontal(x, y),
    };
    Event::Mouse(mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Pixels { x, y: 0.0 },
    })
}

/// Reveal a left-anchored pane without reflowing its contents on every frame.
pub fn reveal_leading_width<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    width: f32,
    progress: f32,
) -> Element<'a, Message> {
    reveal_width_aligned(content, width, progress, true)
}

fn reveal_width_aligned<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    width: f32,
    progress: f32,
    leading: bool,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::RevealWidth {
            width: width.max(0.0),
            progress: progress.clamp(0.0, 1.0),
            leading,
        },
        tip: None,
    }
    .into()
}

/// Fades semantic COSMIC surfaces and text without moving them.
///
/// This is not a renderer opacity group: the pinned renderer has no group-alpha
/// operation, and its SVG recoloring ignores color alpha, even for symbolic
/// icons. Icons/images must use their own `.opacity(progress)`; explicit custom
/// colors and styles which replace alpha must also apply `progress` themselves.
/// Child events, focus, overlays, and accessibility remain available throughout.
pub fn fade<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    progress: f32,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::Fade(progress.clamp(0.0, 1.0)),
        tip: None,
    }
    .into()
}

/// Delayed, stationary tooltips that do not hide their control from assistive
/// technology (the pinned upstream Tooltip does not forward a11y nodes).
/// Give the control its own accessible name/description as well. The dwell clock
/// follows the control, not its label, so live status text cannot postpone it.
pub fn tooltip<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    label: impl Into<Cow<'a, str>>,
) -> Element<'a, Message> {
    let label = label.into();
    // The shell's type scale owns this size. Tooltip text is the whole content
    // of its surface rather than secondary metadata beside something else, so
    // it takes the body role; `design::caption` also mutes its color, which an
    // overlay with its own background must not do.
    let tip = cosmic::widget::container(crate::design::body(label))
        .class(cosmic::theme::Container::custom(|_| Default::default()))
        .padding(8)
        .max_width(320)
        .into();
    Effect {
        content: content.into(),
        kind: Kind::Tooltip,
        tip: Some(Tip { content: tip }),
    }
    .into()
}

/// Keep the native menu beside its content: the pinned COSMIC ContextMenu
/// does not forward `mouse_interaction`, so wrapping the content inside it
/// loses every child cursor. The transparent front layer owns menu gestures;
/// the real content retains its native input, cursor and accessibility tree.
/// Schedule a frame when a stationary right click opens the menu.
pub fn context_menu<'a, Message: Clone + 'static>(
    content: impl Into<Element<'a, Message>>,
    menu: Vec<widget::menu::Tree<Message>>,
) -> Element<'a, Message> {
    Effect {
        content: layer(content.into(), context_menu::surface(menu)),
        kind: Kind::ContextMenu,
        tip: None,
    }
    .into()
}

/// Blocks click-through without hiding the child's accessibility tree.
pub fn opaque<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::Opaque,
        tip: None,
    }
    .into()
}

/// Two visual layers with front-first input and complete accessibility traversal.
/// The pinned upstream Stack drops accessibility nodes for all descendants.
pub fn layer<'a, Message: 'a>(
    base: Element<'a, Message>,
    front: Element<'a, Message>,
) -> Element<'a, Message> {
    Element::new(Layers {
        children: [base, front],
    })
}

struct Layers<'a, Message> {
    children: [Element<'a, Message>; 2],
}

impl<Message> Widget<Message, Theme, Renderer> for Layers<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.children);
    }
    fn size(&self) -> Size<Length> {
        self.children[0].as_widget().size()
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let base = self.children[0]
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let size = base.size();
        let front = self.children[1].as_widget_mut().layout(
            &mut tree.children[1],
            renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        layout::Node::with_children(size, vec![base, front])
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((child, tree), layout) in self.children.iter_mut().zip(&mut tree.children).zip(
                layout
                    .children()
                    .map(|child| child.with_virtual_offset(layout.virtual_offset())),
            ) {
                child
                    .as_widget_mut()
                    .operate(tree, layout, renderer, operation);
            }
        });
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        mut cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let over = cursor.is_over(layout.bounds());
        for ((child, tree), layout) in self
            .children
            .iter_mut()
            .rev()
            .zip(tree.children.iter_mut().rev())
            .zip(
                layout
                    .children()
                    .rev()
                    .map(|child| child.with_virtual_offset(layout.virtual_offset())),
            )
        {
            child.as_widget_mut().update(
                tree, event, layout, cursor, renderer, clipboard, shell, viewport,
            );
            if shell.is_event_captured() {
                return;
            }
            if over
                && !cursor.is_levitating()
                && child
                    .as_widget()
                    .mouse_interaction(tree, layout, cursor, viewport, renderer)
                    != mouse::Interaction::None
            {
                cursor = cursor.levitate();
            }
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.children
            .iter()
            .rev()
            .zip(tree.children.iter().rev())
            .zip(
                layout
                    .children()
                    .rev()
                    .map(|child| child.with_virtual_offset(layout.virtual_offset())),
            )
            .map(|((child, tree), layout)| {
                child
                    .as_widget()
                    .mouse_interaction(tree, layout, cursor, viewport, renderer)
            })
            .find(|interaction| *interaction != mouse::Interaction::None)
            .unwrap_or_default()
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
        if layout.bounds().intersection(viewport).is_none() {
            return;
        }
        let mut layouts = layout
            .children()
            .map(|child| child.with_virtual_offset(layout.virtual_offset()));
        let base = layouts.next().expect("base layer");
        let front = layouts.next().expect("front layer");
        let front_active = self.children[1].as_widget().mouse_interaction(
            &tree.children[1],
            front,
            cursor,
            viewport,
            renderer,
        ) != mouse::Interaction::None;
        self.children[0].as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            base,
            if front_active {
                mouse::Cursor::Unavailable
            } else {
                cursor
            },
            viewport,
        );
        renderer.with_layer(*viewport, |renderer| {
            self.children[1].as_widget().draw(
                &tree.children[1],
                renderer,
                theme,
                style,
                front,
                cursor,
                viewport,
            )
        });
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        overlay::from_children(
            &mut self.children,
            tree,
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
        iced_accessibility::A11yTree::join(
            self.children
                .iter()
                .zip(&tree.children)
                .zip(
                    layout
                        .children()
                        .map(|child| child.with_virtual_offset(layout.virtual_offset())),
                )
                .map(|((child, tree), layout)| child.as_widget().a11y_nodes(layout, tree, cursor)),
        )
    }
    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        for ((child, tree), child_layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            child.as_widget().drag_destinations(
                tree,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                renderer,
                destinations,
            );
        }
    }
}

struct WheelInput<'a, Message> {
    content: Element<'a, Message>,
    pass_to_parent: bool,
}

impl<Message> Widget<Message, Theme, Renderer> for WheelInput<'_, Message> {
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
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout.with_virtual_offset(layout.virtual_offset()),
            renderer,
            operation,
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
        if self.pass_to_parent && matches!(event, Event::Mouse(mouse::Event::WheelScrolled { .. }))
        {
            return;
        }
        let remapped;
        let event = match event {
            Event::Mouse(mouse::Event::WheelScrolled { delta })
                if cursor.is_over(layout.bounds()) =>
            {
                remapped = wheel_as_horizontal(*delta);
                &remapped
            }
            other => other,
        };
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout.with_virtual_offset(layout.virtual_offset()),
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
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout.with_virtual_offset(layout.virtual_offset()),
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
            layout.with_virtual_offset(layout.virtual_offset()),
            cursor,
            viewport,
        )
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
            layout.with_virtual_offset(layout.virtual_offset()),
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
        self.content.as_widget().a11y_nodes(
            layout.with_virtual_offset(layout.virtual_offset()),
            &tree.children[0],
            cursor,
        )
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
            layout.with_virtual_offset(layout.virtual_offset()),
            renderer,
            destinations,
        );
    }
}

const OVERFLOW_FADE_WIDTH: f32 = 24.0;

fn overflow_edges(offset: f32, viewport: f32, content: f32) -> (bool, bool) {
    (offset > 0.5, content - offset > viewport + 0.5)
}

struct OverflowFadeX<'a, Message> {
    content: Element<'a, Message>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct OverflowState {
    start: bool,
    end: bool,
}

struct OverflowProbe {
    start: bool,
    end: bool,
    found: bool,
}

impl<T> Operation<T> for OverflowProbe {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<T>)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        _id: Option<&Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
        _state: &mut dyn operation::Scrollable,
    ) {
        // The tab strip is the outermost scrollable. Nested ones (editor
        // panes, menus) would otherwise overwrite the fade with their offset.
        if self.found {
            return;
        }
        self.found = true;
        (self.start, self.end) = overflow_edges(translation.x, bounds.width, content_bounds.width);
    }
}

fn read_overflow<Message>(
    content: &mut Element<'_, Message>,
    tree: &mut Tree,
    layout: Layout<'_>,
    renderer: &Renderer,
) -> OverflowState {
    let mut probe = OverflowProbe {
        start: false,
        end: false,
        found: false,
    };
    content
        .as_widget_mut()
        .operate(tree, layout, renderer, &mut probe);
    OverflowState {
        start: probe.start,
        end: probe.end,
    }
}

impl<Message> Widget<Message, Theme, Renderer> for OverflowFadeX<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<OverflowState>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(OverflowState::default())
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
        let node = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let overflow = read_overflow(
            &mut self.content,
            &mut tree.children[0],
            Layout::new(&node),
            renderer,
        );
        *tree.state.downcast_mut::<OverflowState>() = overflow;
        node
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout.with_virtual_offset(layout.virtual_offset()),
            renderer,
            operation,
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
        let child_layout = layout.with_virtual_offset(layout.virtual_offset());
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            child_layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        let overflow = read_overflow(
            &mut self.content,
            &mut tree.children[0],
            child_layout,
            renderer,
        );
        let state = tree.state.downcast_mut::<OverflowState>();
        if *state != overflow {
            *state = overflow;
            shell.request_redraw();
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout.with_virtual_offset(layout.virtual_offset()),
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
            layout.with_virtual_offset(layout.virtual_offset()),
            cursor,
            viewport,
        );
        let state = tree.state.downcast_ref::<OverflowState>();
        if !state.start && !state.end {
            return;
        }
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };
        // Match the chrome behind the tabs, not the component fill sitting on it.
        let mut color: Color = theme.current_container().base.into();
        color.a = 1.0;
        let mut clear = color;
        clear.a = 0.0;
        renderer.with_layer(clip, |renderer| {
            if state.start {
                fill_overflow_fade(
                    renderer,
                    Rectangle {
                        x: bounds.x,
                        y: bounds.y,
                        width: OVERFLOW_FADE_WIDTH.min(bounds.width),
                        height: bounds.height,
                    },
                    color,
                    clear,
                );
            }
            if state.end {
                let width = OVERFLOW_FADE_WIDTH.min(bounds.width);
                fill_overflow_fade(
                    renderer,
                    Rectangle {
                        x: bounds.x + bounds.width - width,
                        y: bounds.y,
                        width,
                        height: bounds.height,
                    },
                    clear,
                    color,
                );
            }
        });
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
            layout.with_virtual_offset(layout.virtual_offset()),
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
        self.content.as_widget().a11y_nodes(
            layout.with_virtual_offset(layout.virtual_offset()),
            &tree.children[0],
            cursor,
        )
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
            layout.with_virtual_offset(layout.virtual_offset()),
            renderer,
            destinations,
        );
    }
}

fn fill_overflow_fade(renderer: &mut Renderer, bounds: Rectangle, from: Color, to: Color) {
    renderer.fill_quad(
        renderer::Quad {
            bounds,
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        },
        // iced treats 0 rad as upward (CSS 0deg). π/2 is left-to-right.
        Background::Gradient(Gradient::Linear(
            Linear::new(Radians(std::f32::consts::FRAC_PI_2))
                .add_stop(0.0, from)
                .add_stop(1.0, to),
        )),
    );
}

#[derive(Clone, Copy)]
enum Kind {
    TabHistory(bool),
    Hover,
    ContextMenu,
    Fade(f32),
    Tooltip,
    DriveAnimation(bool),
    RevealWidth {
        width: f32,
        progress: f32,
        leading: bool,
    },
    Opaque,
}

struct Effect<'a, Message> {
    content: Element<'a, Message>,
    kind: Kind,
    tip: Option<Tip<'a, Message>>,
}

struct Tip<'a, Message> {
    content: Element<'a, Message>,
}

// Distinct tags prevent a positional diff from reusing a tooltip's clock as a
// hover/fade clock when a conditional wrapper changes kind.
struct HoverTag;
struct TabHistoryTag;
struct FadeTag;
struct TooltipTag;
struct AnimationDriverTag;
struct OpaqueTag;
struct ContextMenuTag;
struct RevealWidthTag;

fn effect_layout(kind: Kind, layout: Layout<'_>) -> Layout<'_> {
    if matches!(kind, Kind::RevealWidth { .. }) {
        layout
            .children()
            .next()
            .expect("revealed pane layout")
            .with_virtual_offset(layout.virtual_offset())
    } else {
        layout
    }
}

struct State {
    hover: Animation<bool>,
    tooltip: Animation<bool>,
    entered: Option<Instant>,
    tip_open: bool,
    inspect_control: bool,
    disabled: bool,
    focused: bool,
    now: Instant,
    faded: RefCell<Option<FadedTheme>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            hover: Animation::new(false).duration(HOVER_DURATION),
            tooltip: Animation::new(false).duration(HOVER_DURATION),
            entered: None,
            tip_open: false,
            inspect_control: true,
            disabled: false,
            focused: true,
            now: Instant::now(),
            faded: RefCell::new(None),
        }
    }
}

impl<Message> Widget<Message, Theme, Renderer> for Effect<'_, Message> {
    fn tag(&self) -> tree::Tag {
        match self.kind {
            Kind::TabHistory(_) => tree::Tag::of::<TabHistoryTag>(),
            Kind::Hover => tree::Tag::of::<HoverTag>(),
            Kind::Fade(_) => tree::Tag::of::<FadeTag>(),
            Kind::Tooltip => tree::Tag::of::<TooltipTag>(),
            Kind::DriveAnimation(_) => tree::Tag::of::<AnimationDriverTag>(),
            Kind::Opaque => tree::Tag::of::<OpaqueTag>(),
            Kind::ContextMenu => tree::Tag::of::<ContextMenuTag>(),
            Kind::RevealWidth { .. } => tree::Tag::of::<RevealWidthTag>(),
        }
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }
    fn children(&self) -> Vec<Tree> {
        let mut children = vec![Tree::new(&self.content)];
        if let Some(tip) = &self.tip {
            children.push(Tree::new(&tip.content));
        }
        children
    }
    fn diff(&mut self, tree: &mut Tree) {
        let identity = control_identity(&tree.children[0]).cloned();
        if let Some(tip) = &mut self.tip {
            tree.diff_children(&mut [self.content.as_widget_mut(), tip.content.as_widget_mut()]);
        } else {
            tree.diff_children(std::slice::from_mut(&mut self.content));
        }
        let changed_control = identity.as_ref() != control_identity(&tree.children[0]);
        let state = tree.state.downcast_mut::<State>();
        state.inspect_control = true;
        if changed_control {
            *state = State::default();
        }
    }
    fn size(&self) -> Size<Length> {
        let mut size = self.content.as_widget().size();
        if let Kind::RevealWidth {
            width, progress, ..
        } = self.kind
        {
            size.width = Length::Fixed(width * progress);
        }
        size
    }
    fn size_hint(&self) -> Size<Length> {
        if matches!(self.kind, Kind::RevealWidth { .. }) {
            self.size()
        } else {
            self.content.as_widget().size_hint()
        }
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        if let Kind::RevealWidth {
            width,
            progress,
            leading,
        } = self.kind
        {
            let visible = (width * progress).min(limits.max().width);
            let child = self.content.as_widget_mut().layout(
                &mut tree.children[0],
                renderer,
                &layout::Limits::new(Size::new(width, 0.0), Size::new(width, limits.max().height)),
            );
            let height = child.size().height;
            let x = if leading { 0.0 } else { visible - width };
            return layout::Node::with_children(
                Size::new(visible, height),
                vec![child.move_to(Point::new(x, 0.0))],
            );
        }
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
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            effect_layout(self.kind, layout),
            renderer,
            operation,
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
        if let Kind::TabHistory(true) = self.kind
            && let Event::Keyboard(
                cosmic::iced::keyboard::Event::KeyPressed { key, modifiers, .. }
                | cosmic::iced::keyboard::Event::KeyReleased { key, modifiers, .. },
            ) = event
            && crate::tab_history::is_chord(key, *modifiers)
        {
            shell.capture_event();
            return;
        }
        if let Kind::RevealWidth { progress, .. } = self.kind {
            let clipped = layout.bounds().intersection(viewport).unwrap_or_default();
            let cursor = if progress < 1.0 || !cursor.is_over(clipped) {
                mouse::Cursor::Unavailable
            } else {
                cursor
            };
            if progress < 1.0 && !matches!(event, Event::Window(_)) {
                return;
            }
            self.content.as_widget_mut().update(
                &mut tree.children[0],
                event,
                effect_layout(self.kind, layout),
                cursor,
                renderer,
                clipboard,
                shell,
                &clipped,
            );
            return;
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
        if matches!(self.kind, Kind::TabHistory(_)) {
            return;
        }
        if matches!(self.kind, Kind::ContextMenu) {
            if matches!(
                event,
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right))
            ) && cursor.is_over(layout.bounds())
            {
                shell.invalidate_layout();
                shell.request_redraw();
            }
            return;
        }
        if matches!(self.kind, Kind::Opaque) {
            if matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_)))
                && cursor.is_over(layout.bounds())
            {
                shell.capture_event();
            }
            return;
        }
        if let Kind::DriveAnimation(active) = self.kind {
            if active {
                shell.request_redraw();
            }
            return;
        }
        if matches!(self.kind, Kind::Fade(_)) {
            return;
        }
        let state = tree.state.downcast_mut::<State>();
        let now = match event {
            Event::Window(window::Event::RedrawRequested(at)) => *at,
            _ => Instant::now(),
        };
        state.now = now;
        match event {
            Event::Window(window::Event::Focused) => state.focused = true,
            Event::Window(window::Event::Unfocused) => state.focused = false,
            _ => {}
        }
        let mut inside = state.focused
            && !matches!(event, Event::Mouse(mouse::Event::CursorLeft))
            && layout
                .bounds()
                .intersection(viewport)
                .is_some_and(|bounds| cursor.is_over(bounds));
        if !inside {
            state.inspect_control = true;
        }
        if matches!(self.kind, Kind::Tooltip) && inside && state.inspect_control {
            // The Widget API has no enabled query. COSMIC buttons expose this
            // through their root a11y node. Inspect on entry/rebuild, not every
            // animation frame; noninteractive explanatory tooltips still work.
            let nodes = self
                .content
                .as_widget()
                .a11y_nodes(layout, &tree.children[0], cursor);
            state.disabled = nodes.root().len() == 1 && nodes.root()[0].node().is_disabled();
            state.inspect_control = false;
        }
        inside &= !state.disabled;
        if state.hover.value() != inside {
            state.hover.go_mut(inside, now);
            state.entered = inside.then_some(now);
            shell.request_redraw();
        }
        match self.kind {
            Kind::Hover if state.hover.is_animating(now) => shell.request_redraw(),
            Kind::Tooltip => {
                let mut visible = false;
                if let Some(entered) = state.entered.filter(|_| inside) {
                    let deadline = entered + TOOLTIP_DELAY;
                    visible = now >= deadline;
                    if !visible {
                        shell.request_redraw_at(deadline);
                    }
                }
                if state.tooltip.value() != visible {
                    state.tooltip.go_mut(visible, now);
                    if visible && !state.tip_open {
                        state.tip_open = true;
                        shell.invalidate_layout();
                    }
                    shell.request_redraw();
                }
                if state.tooltip.is_animating(now) {
                    shell.request_redraw();
                } else if !visible && state.tip_open {
                    state.tip_open = false;
                    shell.invalidate_layout();
                    shell.request_redraw();
                }
            }
            _ => {}
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        if let Kind::RevealWidth { progress, .. } = self.kind {
            if progress < 1.0 || !cursor.is_over(layout.bounds()) {
                return mouse::Interaction::None;
            }
        }
        let interaction = self.content.as_widget().mouse_interaction(
            &tree.children[0],
            effect_layout(self.kind, layout),
            cursor,
            viewport,
            renderer,
        );
        if matches!(self.kind, Kind::Opaque)
            && interaction == mouse::Interaction::None
            && cursor.is_over(layout.bounds())
        {
            mouse::Interaction::Idle
        } else {
            interaction
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
        if matches!(self.kind, Kind::RevealWidth { .. }) {
            if let Some(clipped) = layout.bounds().intersection(viewport) {
                renderer.with_layer(clipped, |renderer| {
                    self.content.as_widget().draw(
                        &tree.children[0],
                        renderer,
                        theme,
                        style,
                        effect_layout(self.kind, layout),
                        cursor,
                        &clipped,
                    );
                });
            }
            return;
        }
        let state = tree.state.downcast_ref::<State>();
        if let Kind::Fade(progress) = self.kind {
            if progress <= 0.0 {
                return;
            }
            if progress < 1.0 {
                let mut cached = state.faded.borrow_mut();
                if cached.as_ref().is_none_or(|cached| cached.source != *theme) {
                    *cached = Some(FadedTheme::new(theme));
                }
                let faded = cached
                    .as_mut()
                    .expect("faded theme initialized")
                    .at(progress);
                let mut style = *style;
                style.text_color.a *= progress;
                style.icon_color.a *= progress;
                self.content.as_widget().draw(
                    &tree.children[0],
                    renderer,
                    faded,
                    &style,
                    layout,
                    cursor,
                    viewport,
                );
                return;
            }
        }
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        if matches!(self.kind, Kind::Hover) {
            // Keep low-emphasis rows legible against neutral surfaces without
            // turning hover feedback into a selected-state fill.
            let alpha = state.hover.interpolate(0.0, 0.10, state.now);
            if alpha > 0.0 {
                if let Some(bounds) = layout.bounds().intersection(viewport) {
                    let mut color: Color = theme.current_container().on.into();
                    color.a *= alpha;
                    // Draw after the child, otherwise opaque button surfaces
                    // completely cover the feedback. This captures no events.
                    renderer.with_layer(bounds, |renderer| {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: layout.bounds(),
                                border: cosmic::iced::Border {
                                    radius: theme.cosmic().corner_radii.radius_s.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            },
                            color,
                        )
                    });
                }
            }
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
        if matches!(self.kind, Kind::RevealWidth { progress, .. } if progress < 1.0) {
            return None;
        }
        let (content_state, tip_state) = tree.children.split_at_mut(1);
        let child = self.content.as_widget_mut().overlay(
            &mut content_state[0],
            effect_layout(self.kind, layout),
            renderer,
            viewport,
            translation,
        );
        let state = tree.state.downcast_ref::<State>();
        if state.tip_open {
            if let Some(tip) = &mut self.tip {
                let tip = overlay::Element::new(Box::new(TipOverlay {
                    content: &mut tip.content,
                    tree: &mut tip_state[0],
                    anchor: layout.bounds() + translation,
                    progress: state.tooltip.interpolate(0.0, 1.0, state.now),
                }));
                return Some(if let Some(child) = child {
                    overlay::Group::with_children(vec![child, tip]).overlay()
                } else {
                    tip
                });
            }
        }
        child
    }
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content.as_widget().a11y_nodes(
            effect_layout(self.kind, layout),
            &tree.children[0],
            cursor,
        )
    }
    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        if matches!(self.kind, Kind::RevealWidth { progress, .. } if progress < 1.0) {
            return;
        }
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            effect_layout(self.kind, layout),
            renderer,
            destinations,
        );
    }
}

impl<'a, Message: 'a> From<Effect<'a, Message>> for Element<'a, Message> {
    fn from(effect: Effect<'a, Message>) -> Self {
        Self::new(effect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vertical_wheel_becomes_horizontal_pixels() {
        let Event::Mouse(mouse::Event::WheelScrolled { delta }) =
            wheel_as_horizontal(mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 })
        else {
            panic!("expected a wheel event");
        };
        assert_eq!(delta, mouse::ScrollDelta::Pixels { x: 60.0, y: 0.0 });
    }

    #[test]
    fn a_diagonal_wheel_keeps_its_horizontal_component() {
        // Summing the axes cancelled this gesture to a standstill, and the
        // wrapper had already replaced the original event with the sum.
        for (delta, expected) in [
            (
                mouse::ScrollDelta::Lines { x: -1.0, y: 1.0 },
                mouse::ScrollDelta::Pixels { x: -60.0, y: 0.0 },
            ),
            (
                mouse::ScrollDelta::Pixels { x: -8.0, y: 8.0 },
                mouse::ScrollDelta::Pixels { x: -8.0, y: 0.0 },
            ),
            (
                mouse::ScrollDelta::Pixels { x: 0.0, y: 8.0 },
                mouse::ScrollDelta::Pixels { x: 8.0, y: 0.0 },
            ),
        ] {
            let Event::Mouse(mouse::Event::WheelScrolled { delta }) = wheel_as_horizontal(delta)
            else {
                panic!("expected a wheel event");
            };
            assert_eq!(delta, expected);
        }
    }

    #[test]
    fn overflow_edges_are_clear_when_content_fits() {
        assert_eq!(overflow_edges(0.0, 200.0, 180.0), (false, false));
        assert_eq!(overflow_edges(0.0, 200.0, 200.0), (false, false));
        assert_eq!(overflow_edges(0.25, 200.0, 200.4), (false, false));
    }

    #[test]
    fn overflow_edges_mark_whichever_side_can_still_scroll() {
        assert_eq!(overflow_edges(0.0, 200.0, 400.0), (false, true));
        assert_eq!(overflow_edges(12.0, 200.0, 400.0), (true, true));
        assert_eq!(overflow_edges(200.0, 200.0, 400.0), (true, false));
    }

    /// Whole frames of a real context menu, driven the way the runtime drives
    /// them: build (which diffs and lays out), then update (which builds the
    /// overlay, lays it out and feeds it the events). Menu state, overlays and
    /// reconciliation therefore interleave exactly as they do in the app.
    mod context_menus {
        use super::*;
        use cosmic::iced::advanced::clipboard;
        use cosmic::iced::event::Status;
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};

        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum Act {
            Item(usize),
        }

        impl widget::menu::Action for Act {
            type Message = Act;
            fn message(&self) -> Self::Message {
                *self
            }
        }

        /// A flat menu of `items` buttons, the shape every menu in this app has.
        fn menu(items: usize) -> Vec<widget::menu::Tree<Act>> {
            let entries = (0..items)
                .map(|index| {
                    widget::menu::Item::Button(format!("Item {index}"), None, Act::Item(index))
                })
                .collect();
            widget::menu::items(&std::collections::HashMap::new(), entries)
        }

        fn row(items: usize) -> Element<'static, Act> {
            context_menu(bare_row(), menu(items))
        }

        fn bare_row() -> Element<'static, Act> {
            cosmic::iced::widget::Space::new()
                .width(Length::Fill)
                .height(ROW_HEIGHT)
                .into()
        }

        const ROW_HEIGHT: f32 = 32.0;
        const WINDOW: Size = Size {
            width: 400.0,
            height: 400.0,
        };
        /// Inside the first row, so a right click here opens that row's menu.
        const ANCHOR: Point = Point { x: 20.0, y: 16.0 };
        /// Inside the menu the anchor opens, so an open menu captures motion
        /// here and a dismissed one does not.
        const OVER_MENU: Point = Point { x: 60.0, y: 60.0 };

        fn headless_renderer() -> Renderer {
            use cosmic::iced::advanced::renderer::Headless;
            use cosmic::iced::{Font, Pixels};

            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("executor")
                .block_on(Renderer::new(
                    Font::default(),
                    Pixels(14.0),
                    Some("tiny-skia"),
                ))
                .expect("a headless renderer")
        }

        struct Frames {
            renderer: Renderer,
            cache: Cache,
            messages: Vec<Act>,
        }

        impl Frames {
            fn new() -> Self {
                Self {
                    renderer: headless_renderer(),
                    cache: Cache::new(),
                    messages: Vec::new(),
                }
            }

            fn frame(
                &mut self,
                element: Element<'static, Act>,
                events: &[Event],
                cursor: Point,
            ) -> Vec<Status> {
                let mut ui = UserInterface::build(
                    element,
                    WINDOW,
                    std::mem::replace(&mut self.cache, Cache::new()),
                    &mut self.renderer,
                );
                let (_, statuses) = ui.update(
                    events,
                    mouse::Cursor::Available(cursor),
                    &mut self.renderer,
                    &mut clipboard::Null,
                    &mut self.messages,
                );
                self.cache = ui.into_cache();
                statuses
            }

            /// Whether the menu anchored at [`ANCHOR`] is on screen, asked the
            /// way a user asks: only an open menu swallows the pointer.
            fn menu_is_open(&mut self, element: Element<'static, Act>) -> bool {
                self.frame(element, &[moved(OVER_MENU)], OVER_MENU)
                    .contains(&Status::Captured)
            }

            /// Mounts the row, right-clicks it and lets the requested frame
            /// arrive, which is when the native widget measures the menu.
            fn open_menu(&mut self, items: usize) {
                let _ = self.frame(row(items), &[], ANCHOR);
                let _ = self.frame(row(items), &right_click(), ANCHOR);
                let _ = self.frame(row(items), &[redraw()], ANCHOR);
                assert!(
                    self.menu_is_open(row(items)),
                    "a right click on the row should have opened its {items}-item menu"
                );
            }
        }

        fn moved(position: Point) -> Event {
            Event::Mouse(mouse::Event::CursorMoved { position })
        }

        /// The frame the opening right click asks for. The native widget only
        /// measures its menu while the pointer is still over the row, so this
        /// is the event that puts the menu on screen.
        fn redraw() -> Event {
            Event::Window(window::Event::RedrawRequested(Instant::now()))
        }

        fn right_click() -> Vec<Event> {
            vec![
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
            ]
        }

        #[test]
        fn a_single_item_context_menu_activates_its_visible_item() {
            for move_before_press in [false, true] {
                let mut frames = Frames::new();
                // The full-width row makes COSMIC place this popup leftwards:
                // its measured bounds are x=180..420, y=16..52.
                let point = Point::new(220.0, 32.0);
                let _ = frames.frame(row(1), &[], ANCHOR);
                let _ = frames.frame(row(1), &right_click(), ANCHOR);
                let _ = frames.frame(row(1), &[redraw()], ANCHOR);
                if move_before_press {
                    let _ = frames.frame(row(1), &[moved(point)], point);
                }
                let _ = frames.frame(
                    row(1),
                    &[Event::Mouse(mouse::Event::ButtonPressed(
                        mouse::Button::Left,
                    ))],
                    point,
                );
                let _ = frames.frame(
                    row(1),
                    &[Event::Mouse(mouse::Event::ButtonReleased(
                        mouse::Button::Left,
                    ))],
                    point,
                );
                assert_eq!(frames.messages, vec![Act::Item(0)]);
            }
        }

        #[test]
        fn an_open_menu_outlives_frames_that_leave_its_items_alone() {
            // The dismissal below must cost something, or it would be no fix at
            // all: a menu that closes on the next frame regardless is useless.
            let mut frames = Frames::new();
            frames.open_menu(6);
            for _ in 0..3 {
                assert!(
                    frames.menu_is_open(row(6)),
                    "an unchanged menu should stay open across frames"
                );
            }
        }

        #[test]
        fn an_open_menu_losing_items_dismisses_itself() {
            // The native widget measures a position and a size per item once,
            // when the menu opens, and then reads that geometry against every
            // later frame's item list. Shrinking the list used to leave the
            // overlay indexing the new list by the old geometry.
            let mut frames = Frames::new();
            frames.open_menu(6);
            assert!(
                !frames.menu_is_open(row(4)),
                "a menu missing two of its items is not the menu that was opened"
            );
            for items in [3, 1] {
                assert!(!frames.menu_is_open(row(items)));
            }
        }

        #[test]
        fn an_open_menu_gaining_items_dismisses_itself() {
            let mut frames = Frames::new();
            frames.open_menu(2);
            assert!(!frames.menu_is_open(row(9)));
        }

        #[test]
        fn a_row_that_loses_its_menu_entirely_can_get_it_back() {
            // Sidebar and file rows drop the whole wrapper when a modal opens
            // or the host forbids the commands, so the menu leaves and returns.
            let mut frames = Frames::new();
            frames.open_menu(5);
            assert!(!frames.menu_is_open(bare_row()));
            let _ = frames.frame(row(5), &right_click(), ANCHOR);
            let _ = frames.frame(row(5), &[redraw()], ANCHOR);
            assert!(
                frames.menu_is_open(row(5)),
                "the row should open a fresh menu once its wrapper is back"
            );
        }

        #[test]
        fn a_dismissed_menu_leaves_the_row_able_to_open_another() {
            let mut frames = Frames::new();
            frames.open_menu(6);
            assert!(!frames.menu_is_open(row(3)));
            let _ = frames.frame(row(3), &right_click(), ANCHOR);
            let _ = frames.frame(row(3), &[redraw()], ANCHOR);
            assert!(
                frames.menu_is_open(row(3)),
                "the surface should still open the menu it now has"
            );
        }

        /// `rows` rows in a scrollable, the one at `anchored` carrying `items`.
        fn scrolled(rows: usize, anchored: usize, items: usize) -> Element<'static, Act> {
            let children = (0..rows)
                .map(|index| {
                    if index == anchored {
                        row(items)
                    } else {
                        row(items + 1)
                    }
                })
                .collect::<Vec<_>>();
            widget::scrollable(widget::column::with_children(children))
                .height(Length::Fixed(WINDOW.height))
                .into()
        }

        #[test]
        fn a_menu_in_a_scrollable_survives_its_row_moving_and_leaving() {
            // A row can scroll away, or be replaced at its index by a row whose
            // menu is a different length, while its menu is still up.
            let mut frames = Frames::new();
            let _ = frames.frame(scrolled(40, 0, 5), &[], ANCHOR);
            let _ = frames.frame(scrolled(40, 0, 5), &right_click(), ANCHOR);
            let _ = frames.frame(scrolled(40, 0, 5), &[redraw()], ANCHOR);
            for events in [
                vec![Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -240.0 },
                })],
                vec![moved(OVER_MENU)],
            ] {
                let _ = frames.frame(scrolled(40, 0, 5), &events, OVER_MENU);
            }
            // The anchored row's menu grows, then the row is gone altogether.
            let _ = frames.frame(scrolled(40, 0, 8), &[moved(OVER_MENU)], OVER_MENU);
            let _ = frames.frame(scrolled(3, 2, 8), &[moved(OVER_MENU)], OVER_MENU);
            assert!(
                !frames.menu_is_open(scrolled(3, 9, 5)),
                "no menu should survive its row being replaced"
            );
        }
    }
}

// Keep the wrapper anonymous: giving it its child's ID would register two
// incompatible states under one name in iced's named-widget reconciliation.
fn control_identity(tree: &Tree) -> Option<&Id> {
    tree.id
        .as_ref()
        .or_else(|| tree.children.first().and_then(control_identity))
}

struct TipOverlay<'a, 'b, Message> {
    content: &'a mut Element<'b, Message>,
    tree: &'a mut Tree,
    anchor: Rectangle,
    progress: f32,
}

impl<Message> overlay::Overlay<Message, Theme, Renderer> for TipOverlay<'_, '_, Message> {
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        let node = self.content.as_widget_mut().layout(
            self.tree,
            renderer,
            &layout::Limits::new(Size::ZERO, bounds),
        );
        let size = node.size();
        let x = (self.anchor.center_x() - size.width / 2.0)
            .clamp(0.0, (bounds.width - size.width).max(0.0));
        let below = self.anchor.y + self.anchor.height + 4.0;
        let y = if below + size.height <= bounds.height {
            below
        } else {
            self.anchor.y - size.height - 4.0
        };
        node.move_to(Point::new(
            x,
            y.clamp(0.0, (bounds.height - size.height).max(0.0)),
        ))
    }
    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        if self.progress <= 0.0 {
            return;
        }
        let mut background: Color = theme.cosmic().palette.neutral_2.into();
        background.a *= self.progress;
        let mut style = *style;
        style.text_color = theme.current_container().component.on.into();
        style.text_color.a *= self.progress;
        // The tooltip is known text plus padding: fade these two primitives
        // directly instead of deriving/serializing an entire theme per frame.
        renderer.with_layer(layout.bounds(), |renderer| {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: layout.bounds(),
                    border: cosmic::iced::Border {
                        radius: theme.cosmic().corner_radii.radius_l.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                background,
            );
            self.content.as_widget().draw(
                self.tree,
                renderer,
                theme,
                &style,
                layout,
                cursor,
                &layout.bounds(),
            );
        });
    }
}

struct FadedTheme {
    source: Theme,
    serialized: serde_json::Value,
    progress: f32,
    theme: Theme,
}

impl FadedTheme {
    fn new(source: &Theme) -> Self {
        Self {
            source: source.clone(),
            serialized: serde_json::to_value(source.cosmic())
                .expect("pinned COSMIC theme is serializable"),
            progress: 1.0,
            theme: source.clone(),
        }
    }

    fn at(&mut self, progress: f32) -> &Theme {
        if self.progress == progress {
            return &self.theme;
        }
        let source = self.source.cosmic();
        // Only the six surface containers are private in cosmic_theme::Theme.
        // Cache the source serialization, overwrite their colors from typed
        // originals, and deserialize by reference (no per-frame JSON clone).
        // ColorRepr explicitly accepts palette's floating-point RGBA variant;
        // hex serialization would quantize alpha to 8 bits on every frame.
        for (key, container) in [
            ("background", source.background(false)),
            ("primary", source.primary(false)),
            ("secondary", source.secondary(false)),
            ("transparent_background", source.background(true)),
            ("transparent_primary", source.primary(true)),
            ("transparent_secondary", source.secondary(true)),
        ] {
            set_container_colors(&mut self.serialized[key], container, progress);
        }
        let mut cosmic = cosmic::cosmic_theme::Theme::deserialize(&self.serialized)
            .expect("typed RGBA colors preserve the pinned COSMIC theme schema");
        // Public semantic colors can be adjusted directly without serializing,
        // parsing, or rounding their RGB components or original transparency.
        macro_rules! components {
            ($($field:ident),+ $(,)?) => {$(
                cosmic.$field = source.$field.clone();
                for color in component_colors(&mut cosmic.$field) { color.alpha *= progress; }
            )+};
        }
        components!(
            button,
            accent,
            success,
            destructive,
            warning,
            accent_button,
            success_button,
            destructive_button,
            warning_button,
            icon_button,
            link_button,
            list_button,
            text_button
        );
        cosmic.palette = source.palette.clone();
        macro_rules! palette_colors {
            ($($field:ident),+ $(,)?) => {$(cosmic.palette.$field.alpha *= progress;)+};
        }
        palette_colors!(
            bright_red,
            bright_green,
            bright_orange,
            gray_1,
            gray_2,
            neutral_0,
            neutral_1,
            neutral_2,
            neutral_3,
            neutral_4,
            neutral_5,
            neutral_6,
            neutral_7,
            neutral_8,
            neutral_9,
            neutral_10,
            accent_blue,
            accent_indigo,
            accent_purple,
            accent_pink,
            accent_red,
            accent_orange,
            accent_yellow,
            accent_green,
            accent_warm_grey,
            ext_warm_grey,
            ext_orange,
            ext_yellow,
            ext_blue,
            ext_purple,
            ext_pink,
            ext_indigo
        );
        cosmic.shade = source.shade;
        cosmic.shade.alpha *= progress;
        cosmic.accent_text = source.accent_text.map(|mut color| {
            color.alpha *= progress;
            color
        });
        cosmic.control_tint = source.control_tint;
        cosmic.text_tint = source.text_tint;
        self.theme = Theme {
            theme_type: cosmic::theme::ThemeType::Custom(Arc::new(cosmic)),
            ..self.source.clone()
        };
        self.progress = progress;
        &self.theme
    }
}

fn component_colors(component: &mut Component) -> [&mut Srgba; 12] {
    [
        &mut component.base,
        &mut component.hover,
        &mut component.pressed,
        &mut component.selected,
        &mut component.selected_text,
        &mut component.focus,
        &mut component.divider,
        &mut component.on,
        &mut component.disabled,
        &mut component.on_disabled,
        &mut component.border,
        &mut component.disabled_border,
    ]
}

fn set_container_colors(value: &mut serde_json::Value, source: &Container, progress: f32) {
    for (key, color) in [
        ("base", source.base),
        ("divider", source.divider),
        ("on", source.on),
        ("small_widget", source.small_widget),
    ] {
        set_color(&mut value[key], color, progress);
    }
    let component = &source.component;
    for (key, color) in [
        ("base", component.base),
        ("hover", component.hover),
        ("pressed", component.pressed),
        ("selected", component.selected),
        ("selected_text", component.selected_text),
        ("focus", component.focus),
        ("divider", component.divider),
        ("on", component.on),
        ("disabled", component.disabled),
        ("on_disabled", component.on_disabled),
        ("border", component.border),
        ("disabled_border", component.disabled_border),
    ] {
        set_color(&mut value["component"][key], color, progress);
    }
}

fn set_color(value: &mut serde_json::Value, source: Srgba, progress: f32) {
    if !value.is_object() {
        // Serialize the typed RGBA alternative once, not the hex-based theme
        // serializer. Subsequent frames replace only its numeric alpha.
        *value = serde_json::to_value(source).expect("palette RGBA is serializable");
    }
    value["alpha"] = (source.alpha * progress).into();
}
