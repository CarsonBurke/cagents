//! Shell effects and input layers that retain native accessibility trees.

use std::{borrow::Cow, cell::RefCell, sync::Arc, time::Duration};

use cosmic::cosmic_theme::{Component, Container, palette::Srgba};
use cosmic::iced::advanced::Renderer as _;
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree, tree},
};
use cosmic::iced::{
    Animation, Color, Event, Length, Point, Rectangle, Size, Vector, mouse, time::Instant, window,
};
use cosmic::{Element, Renderer, Theme, widget};
use serde::Deserialize;

const HOVER_DURATION: Duration = Duration::from_millis(120);
const TOOLTIP_DELAY: Duration = Duration::from_millis(1_000);

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
    let tip = cosmic::widget::container(cosmic::widget::text(label).size(12))
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

/// The pinned context menu changes its overlay state without requesting a
/// frame. Schedule that frame explicitly so a stationary right click opens it.
pub fn context_menu<'a, Message: Clone + 'static>(
    content: impl Into<Element<'a, Message>>,
    menu: Vec<widget::menu::Tree<Message>>,
) -> Element<'a, Message> {
    Effect {
        content: widget::context_menu(content, Some(menu)).into(),
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
            for ((child, tree), layout) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
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
            .zip(layout.children().rev())
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
            .zip(layout.children().rev())
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
        let mut layouts = layout.children();
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
                .zip(layout.children())
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

#[derive(Clone, Copy)]
enum Kind {
    Hover,
    ContextMenu,
    Fade(f32),
    Tooltip,
    DriveAnimation(bool),
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
struct FadeTag;
struct TooltipTag;
struct AnimationDriverTag;
struct OpaqueTag;
struct ContextMenuTag;

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
            Kind::Hover => tree::Tag::of::<HoverTag>(),
            Kind::Fade(_) => tree::Tag::of::<FadeTag>(),
            Kind::Tooltip => tree::Tag::of::<TooltipTag>(),
            Kind::DriveAnimation(_) => tree::Tag::of::<AnimationDriverTag>(),
            Kind::Opaque => tree::Tag::of::<OpaqueTag>(),
            Kind::ContextMenu => tree::Tag::of::<ContextMenuTag>(),
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
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
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
        let interaction = self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
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
            let alpha = state.hover.interpolate(0.0, 0.06, state.now);
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
        let (content_state, tip_state) = tree.children.split_at_mut(1);
        let child = self.content.as_widget_mut().overlay(
            &mut content_state[0],
            layout,
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

impl<'a, Message: 'a> From<Effect<'a, Message>> for Element<'a, Message> {
    fn from(effect: Effect<'a, Message>) -> Self {
        Self::new(effect)
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
