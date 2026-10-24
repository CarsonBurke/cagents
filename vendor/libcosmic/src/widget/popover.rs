// Copyright 2022 System76 <info@system76.com>
// SPDX-License-Identifier: MPL-2.0

//! A container which displays an overlay when a popup widget is attached.

use iced::widget;
use iced_core::event::{self, Event};
use iced_core::widget::{Operation, Tree};
use iced_core::{
    Clipboard, Element, Layout, Length, Point, Rectangle, Shell, Size, Vector, Widget, layout,
    mouse, overlay, renderer, touch,
};

pub use iced_widget::container::{Catalog, Style};

pub fn popover<'a, Message, Renderer>(
    content: impl Into<Element<'a, Message, crate::Theme, Renderer>>,
) -> Popover<'a, Message, Renderer> {
    Popover::new(content)
}

#[derive(Clone, Copy, Debug, Default)]
pub enum Position {
    #[default]
    Center,
    Bottom,
    Top,
    Point(Point),
}

/// A container which displays overlays when a popup widget is assigned.
#[must_use]
pub struct Popover<'a, Message, Renderer> {
    id: widget::Id,
    content: Element<'a, Message, crate::Theme, Renderer>,
    modal: bool,
    popup: Option<Element<'a, Message, crate::Theme, Renderer>>,
    position: Position,
    on_close: Option<Message>,
}

#[cfg(feature = "a11y")]
#[derive(Default)]
struct AccessibilityState {
    // Geometry from the real overlay pass, paired with this popup's Tree.
    // Base layout has no popup child and cannot stand in for its position.
    popup_layout: Option<layout::Node>,
    modal: bool,
    order: usize,
    scope: Option<std::rc::Rc<AccessibilityScope>>,
    // Only native root IDs, refreshed during AX export. Never retain AX nodes,
    // widgets or text; ancestors move the already-exported foreground nodes.
    roots: std::cell::RefCell<Vec<iced_accessibility::accesskit::NodeId>>,
}

#[cfg(feature = "a11y")]
#[derive(Default)]
struct AccessibilityScope {
    next: std::cell::Cell<usize>,
    foreground: std::cell::Cell<Option<usize>>,
}

#[cfg(feature = "a11y")]
fn share_accessibility_scope(tree: &mut Tree, scope: &std::rc::Rc<AccessibilityScope>) {
    if tree.tag == iced_core::widget::tree::Tag::of::<AccessibilityState>() {
        tree.state.downcast_mut::<AccessibilityState>().scope = Some(std::rc::Rc::clone(scope));
    }
    for child in &mut tree.children {
        share_accessibility_scope(child, scope);
    }
}

#[cfg(feature = "a11y")]
fn accessibility_id(node: &iced_accessibility::A11yNode) -> iced_accessibility::accesskit::NodeId {
    use iced_accessibility::{A11yId, id::Internal};
    let id = match node.id() {
        A11yId::Window(id) => *id,
        A11yId::Widget(id) => match &id.0 {
            Internal::Unique(id) | Internal::Custom(id, _) => *id,
            Internal::Set(_) => unreachable!("AX nodes cannot have set IDs"),
        },
    };
    iced_accessibility::accesskit::NodeId(id)
}

#[cfg(feature = "a11y")]
fn clear_accessibility_roots(tree: &Tree) {
    if tree.tag == iced_core::widget::tree::Tag::of::<AccessibilityState>() {
        tree.state
            .downcast_ref::<AccessibilityState>()
            .roots
            .borrow_mut()
            .clear();
    }
    for child in &tree.children {
        clear_accessibility_roots(child);
    }
}

#[cfg(feature = "a11y")]
fn modal_accessibility_state(tree: &Tree) -> Option<&AccessibilityState> {
    // Actual overlay layout order, rather than widget-tree depth/order, owns
    // the foreground. Groups can contain several overlays at the same depth.
    if tree.tag == iced_core::widget::tree::Tag::of::<AccessibilityState>() {
        let state = tree.state.downcast_ref::<AccessibilityState>();
        if state.modal
            && state.popup_layout.is_some()
            && state
                .scope
                .as_ref()
                .is_some_and(|scope| scope.foreground.get() == Some(state.order))
        {
            return Some(state);
        }
    }
    tree.children.iter().find_map(modal_accessibility_state)
}

#[cfg(feature = "a11y")]
fn foreground_accessibility_roots(
    tree: &Tree,
    foreground: &AccessibilityState,
    roots: &mut Vec<(usize, iced_accessibility::accesskit::NodeId)>,
) {
    if tree.tag == iced_core::widget::tree::Tag::of::<AccessibilityState>() {
        let state = tree.state.downcast_ref::<AccessibilityState>();
        if state.popup_layout.is_some()
            && state.order >= foreground.order
            && state
                .scope
                .as_ref()
                .zip(foreground.scope.as_ref())
                .is_some_and(|(a, b)| std::rc::Rc::ptr_eq(a, b))
        {
            roots.extend(state.roots.borrow().iter().map(|id| (state.order, *id)));
        }
    }
    for child in &tree.children {
        foreground_accessibility_roots(child, foreground, roots);
    }
}

#[cfg(feature = "a11y")]
fn modal_accessibility_tree(
    mut nodes: iced_accessibility::A11yTree,
    tree: &Tree,
) -> iced_accessibility::A11yTree {
    // This Popover already applies its own modal boundary. Only a nested
    // modal requires promoting roots out of the ancestor popup's AX tree.
    let Some(foreground) = tree.children.iter().find_map(modal_accessibility_state) else {
        return nodes;
    };
    let mut candidates = Vec::new();
    foreground_accessibility_roots(tree, foreground, &mut candidates);
    candidates.sort_by_key(|(order, _)| *order);
    let all = std::mem::take(nodes.root_mut())
        .into_iter()
        .chain(std::mem::take(nodes.children_mut()))
        .collect::<Vec<_>>();
    let edges = all
        .iter()
        .map(|node| (accessibility_id(node), node.node().children()))
        .collect::<std::collections::HashMap<_, _>>();
    let mut retained = std::collections::HashSet::new();
    let mut roots = Vec::new();
    for (_, root) in candidates {
        if retained.contains(&root) {
            continue;
        }
        roots.push(root);
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            if retained.insert(id) {
                if let Some(children) = edges.get(&id) {
                    pending.extend_from_slice(children);
                }
            }
        }
    }
    drop(edges);
    let mut root_nodes = Vec::new();
    let mut children = Vec::new();
    // Promote the real foreground roots out of ancestor AX containers while
    // retaining original node order and the foreground's own child links.
    for node in all {
        let id = accessibility_id(&node);
        if roots.contains(&id) {
            root_nodes.push(node);
        } else if retained.contains(&id) {
            children.push(node);
        }
    }
    iced_accessibility::A11yTree::new(root_nodes, children)
}

impl<'a, Message, Renderer> Popover<'a, Message, Renderer> {
    pub fn new(content: impl Into<Element<'a, Message, crate::Theme, Renderer>>) -> Self {
        Self {
            id: widget::Id::unique(),
            content: content.into(),
            modal: false,
            popup: None,
            position: Position::Center,
            on_close: None,
        }
    }

    /// Set the Id
    #[inline]
    pub fn id(mut self, id: widget::Id) -> Self {
        self.id = id;
        self
    }

    /// A modal popup intercepts user inputs while a popup is active.
    #[inline]
    pub fn modal(mut self, modal: bool) -> Self {
        self.modal = modal;
        self
    }

    /// Emitted when the popup is closed.
    #[inline]
    pub fn on_close(mut self, on_close: Message) -> Self {
        self.on_close = Some(on_close);
        self
    }

    #[inline]
    pub fn popup(mut self, popup: impl Into<Element<'a, Message, crate::Theme, Renderer>>) -> Self {
        self.popup = Some(popup.into());
        self
    }

    #[inline]
    pub fn position(mut self, position: Position) -> Self {
        self.position = position;
        self
    }
}

impl<Message: Clone, Renderer> Widget<Message, crate::Theme, Renderer>
    for Popover<'_, Message, Renderer>
where
    Renderer: iced_core::Renderer,
{
    #[cfg(feature = "a11y")]
    fn tag(&self) -> iced_core::widget::tree::Tag {
        iced_core::widget::tree::Tag::of::<AccessibilityState>()
    }

    #[cfg(feature = "a11y")]
    fn state(&self) -> iced_core::widget::tree::State {
        iced_core::widget::tree::State::new(AccessibilityState::default())
    }

    fn id(&self) -> Option<widget::Id> {
        Some(self.id.clone())
    }

    fn set_id(&mut self, id: widget::Id) {
        self.id = id;
    }

    fn children(&self) -> Vec<Tree> {
        if let Some(popup) = &self.popup {
            vec![Tree::new(&self.content), Tree::new(popup)]
        } else {
            vec![Tree::new(&self.content)]
        }
    }

    fn diff(&mut self, tree: &mut Tree) {
        #[cfg(feature = "a11y")]
        {
            *tree.state.downcast_mut::<AccessibilityState>() = AccessibilityState::default();
        }
        if let Some(popup) = &mut self.popup {
            tree.diff_children(&mut [&mut self.content, popup]);
        } else {
            tree.diff_children(&mut [&mut self.content]);
        }
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
        #[cfg(feature = "a11y")]
        {
            *tree.state.downcast_mut::<AccessibilityState>() = AccessibilityState::default();
        }
        let node = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        #[cfg(feature = "a11y")]
        share_accessibility_scope(tree, &std::rc::Rc::default());
        node
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        // Skip operating on background content, prevents Tab from escaping
        if self.modal && self.popup.is_some() {
            return;
        }
        self.content
            .as_widget_mut()
            .operate(content_tree_mut(tree), layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor_position: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if self.popup.is_some() {
            if self.modal {
                // The overlay already had first refusal. A stale accessibility
                // target must not activate a control behind this modal.
                #[cfg(feature = "a11y")]
                if matches!(event, Event::A11y(..)) {
                    shell.capture_event();
                    return;
                }
                if matches!(event, Event::Mouse(_) | Event::Touch(_)) {
                    shell.capture_event();
                    return;
                }
            } else if let Some(on_close) = self.on_close.as_ref() {
                if matches!(
                    event,
                    Event::Mouse(mouse::Event::ButtonPressed(_))
                        | Event::Touch(touch::Event::FingerPressed { .. })
                ) && !cursor_position.is_over(layout.bounds())
                {
                    shell.publish(on_close.clone());
                }
            }
        }

        // Hide cursor from background content when modal popup is active
        let cursor = if self.modal && self.popup.is_some() {
            mouse::Cursor::Unavailable
        } else {
            cursor_position
        };
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        )
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor_position: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        if self.modal && self.popup.is_some() && cursor_position.is_over(layout.bounds()) {
            return mouse::Interaction::None;
        }
        self.content.as_widget().mouse_interaction(
            content_tree(tree),
            layout,
            cursor_position,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &crate::Theme,
        renderer_style: &renderer::Style,
        layout: Layout<'_>,
        cursor_position: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        // Hide cursor from background content when a modal popup is active
        let cursor = if self.modal && self.popup.is_some() {
            mouse::Cursor::Unavailable
        } else {
            cursor_position
        };
        self.content.as_widget().draw(
            content_tree(tree),
            renderer,
            theme,
            renderer_style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        mut translation: Vector,
    ) -> Option<overlay::Element<'b, Message, crate::Theme, Renderer>> {
        if let Some(popup) = &mut self.popup {
            let bounds = layout.bounds();

            // Calculate overlay position from relative position
            let mut overlay_position = match self.position {
                Position::Center => Point::new(
                    bounds.x + bounds.width / 2.0,
                    bounds.y + bounds.height / 2.0,
                ),
                Position::Bottom => {
                    Point::new(bounds.x + bounds.width / 2.0, bounds.y + bounds.height)
                }
                Position::Point(relative) => {
                    bounds.position() + Vector::new(relative.x, relative.y)
                }
                Position::Top => Point::new(bounds.x + bounds.width / 2.0, bounds.y),
            };

            // Round position to prevent rendering issues
            overlay_position.x = overlay_position.x.round();
            overlay_position.y = overlay_position.y.round();
            translation.x += overlay_position.x;
            translation.y += overlay_position.y;
            Some(overlay::Element::new(Box::new(Overlay {
                #[cfg(feature = "a11y")]
                accessibility: tree.state.downcast_mut::<AccessibilityState>(),
                tree: &mut tree.children[1],
                content: popup,
                position: self.position,
                pos: Point::new(translation.x, translation.y),
                modal: self.modal,
            })))
        } else {
            self.content.as_widget_mut().overlay(
                &mut tree.children[0],
                layout,
                renderer,
                viewport,
                translation,
            )
        }
    }

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        dnd_rectangles: &mut iced_core::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            content_tree(tree),
            layout,
            renderer,
            dnd_rectangles,
        );
    }

    #[cfg(feature = "a11y")]
    /// get the a11y nodes for the widget
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        state: &Tree,
        p: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        clear_accessibility_roots(state);
        let content = || {
            self.content
                .as_widget()
                .a11y_nodes(layout, content_tree(state), p)
        };
        let accessibility = state.state.downcast_ref::<AccessibilityState>();
        let nodes = if let Some(popup) = &self.popup {
            let popup = accessibility
                .popup_layout
                .as_ref()
                .map(|node| {
                    popup
                        .as_widget()
                        .a11y_nodes(Layout::new(node), &state.children[1], p)
                })
                .unwrap_or_default();
            // Record this actual popup's roots before selecting a deeper modal.
            // Its Tree remains the authority; no guessed geometry or IDs.
            *accessibility.roots.borrow_mut() = popup.root().iter().map(accessibility_id).collect();
            if self.modal {
                popup
            } else {
                iced_accessibility::A11yTree::join([content(), popup].into_iter())
            }
        } else {
            content()
        };
        modal_accessibility_tree(nodes, state)
    }
}

impl<'a, Message, Renderer> From<Popover<'a, Message, Renderer>>
    for Element<'a, Message, crate::Theme, Renderer>
where
    Message: 'static + Clone,
    Renderer: iced_core::Renderer + 'static,
{
    fn from(popover: Popover<'a, Message, Renderer>) -> Self {
        Self::new(popover)
    }
}

pub struct Overlay<'a, 'b, Message, Renderer> {
    #[cfg(feature = "a11y")]
    accessibility: &'a mut AccessibilityState,
    tree: &'a mut Tree,
    content: &'a mut Element<'b, Message, crate::Theme, Renderer>,
    position: Position,
    pos: Point,
    modal: bool,
}

impl<Message, Renderer> overlay::Overlay<Message, crate::Theme, Renderer>
    for Overlay<'_, '_, Message, Renderer>
where
    Message: Clone,
    Renderer: iced_core::Renderer,
{
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        let mut position = self.pos;
        let limits = layout::Limits::new(Size::UNIT, bounds);
        let node = self
            .content
            .as_widget_mut()
            .layout(self.tree, renderer, &limits);
        match self.position {
            Position::Center => {
                // Position is set to the center of the widget
                let width = node.size().width;
                let height = node.size().height;
                position.x = (position.x - width / 2.0).clamp(0.0, bounds.width - width);
                position.y = (position.y - height / 2.0).clamp(0.0, bounds.height - height);
            }
            Position::Top => {
                let width = node.size().width;
                let height = node.size().height;
                position.x = (position.x - width / 2.0).clamp(0.0, bounds.width - width);
                position.y = (position.y - height).clamp(0.0, bounds.height - height);
            }
            Position::Bottom => {
                // Position is set to the center bottom of the widget
                let width = node.size().width;
                let height = node.size().height;
                position.x = (position.x - width / 2.0).clamp(0.0, bounds.width - width);
                position.y = position.y.clamp(0.0, bounds.height - height);
            }
            Position::Point(_) => {
                // Position is using context menu logic
                let size = node.size();
                position.x = position.x.clamp(0.0, bounds.width - size.width);
                if position.y + size.height > bounds.height {
                    position.y = (position.y - size.height).clamp(0.0, bounds.height - size.height);
                }
            }
        }

        // Round position to prevent rendering issues
        position.x = position.x.round();
        position.y = position.y.round();

        let node = node.move_to(position);
        #[cfg(feature = "a11y")]
        {
            let scope = self
                .accessibility
                .scope
                .get_or_insert_with(Default::default);
            let order = scope.next.get() + 1;
            scope.next.set(order);
            if self.modal {
                scope.foreground.set(Some(order));
            }
            self.accessibility.order = order;
            self.accessibility.popup_layout = Some(node.clone());
            self.accessibility.modal = self.modal;
            // Child base layout may have reset its state. Give every nested
            // overlay this same layout family before Nested lays it out.
            share_accessibility_scope(self.tree, scope);
        }
        node
    }

    fn operate(
        &mut self,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation<()>,
    ) {
        self.content
            .as_widget_mut()
            .operate(self.tree, layout, renderer, operation);
    }

    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor_position: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        #[cfg(feature = "a11y")]
        if matches!(event, Event::A11y(..))
            && self.accessibility.scope.as_ref().is_some_and(|scope| {
                scope
                    .foreground
                    .get()
                    .is_some_and(|foreground| self.accessibility.order < foreground)
            })
        {
            // Group forwards to all of its children even after capture. An
            // overlay below the actual modal foreground must never publish.
            shell.capture_event();
            return;
        }
        #[cfg(feature = "a11y")]
        if matches!(event, Event::A11y(..))
            && self
                .accessibility
                .scope
                .as_ref()
                .is_some_and(|scope| scope.foreground.get().is_some())
        {
            // Group forwards the SAME Shell to every sibling, while COSMIC
            // buttons refuse events already captured by a preceding modal.
            // Eligible overlays must match their own actual native target
            // before merging capture into that shared Shell. Overlays below
            // the modal boundary returned above without updating content.
            let mut messages = Vec::new();
            let mut local = Shell::new(&mut messages);
            self.content.as_widget_mut().update(
                self.tree,
                event,
                layout,
                cursor_position,
                renderer,
                clipboard,
                &mut local,
                &layout.bounds(),
            );
            if self.modal {
                local.capture_event();
            }
            // Preserve messages, layout/widget invalidation, redraw, IME and
            // event capture; never expose an unknown target to the base.
            shell.merge(local, |message| message);
            return;
        }
        if self.modal
            && matches!(event, Event::Mouse(_) | Event::Touch(_))
            && !cursor_position.is_over(layout.bounds())
        {
            // Swallow new presses outside the popup, but still forward other
            // events so an interaction started inside it (such as a selection
            // drag) receives its release once the cursor leaves the bounds.
            let is_press = matches!(
                event,
                Event::Mouse(mouse::Event::ButtonPressed(_))
                    | Event::Touch(touch::Event::FingerPressed { .. })
            );
            if !is_press {
                self.content.as_widget_mut().update(
                    self.tree,
                    event,
                    layout,
                    cursor_position,
                    renderer,
                    clipboard,
                    shell,
                    &layout.bounds(),
                );
            }
            shell.capture_event();
            return;
        }

        self.content.as_widget_mut().update(
            self.tree,
            event,
            layout,
            cursor_position,
            renderer,
            clipboard,
            shell,
            &layout.bounds(),
        );
        // Nested::update will otherwise bubble an ignored AX target into the
        // parent overlay, whose other controls are behind this modal too.
        #[cfg(feature = "a11y")]
        if self.modal && matches!(event, Event::A11y(..)) {
            shell.capture_event();
        }
    }

    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor_position: mouse::Cursor,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        if self.modal && !cursor_position.is_over(layout.bounds()) {
            return mouse::Interaction::None;
        }

        self.content.as_widget().mouse_interaction(
            self.tree,
            layout,
            cursor_position,
            &layout.bounds(),
            renderer,
        )
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &crate::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor_position: mouse::Cursor,
    ) {
        let bounds = layout.bounds();
        self.content.as_widget().draw(
            self.tree,
            renderer,
            theme,
            style,
            layout,
            cursor_position,
            &bounds,
        );
    }

    fn overlay<'c>(
        &'c mut self,
        layout: Layout<'c>,
        renderer: &Renderer,
    ) -> Option<overlay::Element<'c, Message, crate::Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            self.tree,
            layout,
            renderer,
            &layout.bounds(),
            Default::default(),
        )
    }
}

/// The local state of a [`Popover`].
#[derive(Debug, Default)]
struct State {
    is_open: bool,
}

/// The first child in [`Popover::children`] is always the wrapped content.
fn content_tree(tree: &Tree) -> &Tree {
    &tree.children[0]
}

/// The first child in [`Popover::children`] is always the wrapped content.
fn content_tree_mut(tree: &mut Tree) -> &mut Tree {
    &mut tree.children[0]
}
