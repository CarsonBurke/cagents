//! Content shown up to a height, with a control that shows the rest.
//!
//! Whether there is a rest to show is a question about the content as drawn:
//! Markdown joins a paragraph's lines and wraps them again at the width it is
//! given, so neither its line count nor its length says how tall it will be.
//! The fold lays the content out and decides from that, offering its control
//! only when the content does not fit.

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{self, Operation, Tree, operation, tree},
};
use cosmic::iced::{
    Background, Border, Color, Event, Gradient, Length, Point, Radians, Rectangle, Shadow, Size,
    Vector, gradient::Linear, mouse,
};
use cosmic::{Element, Renderer, Theme};
use std::any::Any;

/// How tall a fold may be before it folds. Content up to a third taller is
/// shown whole: folding away a line or two saves the reader nothing.
const SLACK: f32 = 1.0 / 3.0;

/// How far above its edge folded content starts fading into `fill`.
const FADE: f32 = 32.0;

/// `content`, cut to `height` while `folded` if it is taller, with `toggle`
/// below it. `toggle` is laid out and drawn only when the content is tall
/// enough to fold, whether or not it is folded now, so the reader can fold
/// it again. `fill` is the colour behind the fold, which the cut fades into.
pub(super) fn fold<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    toggle: impl Into<Element<'a, Message>>,
    folded: bool,
    height: f32,
    spacing: f32,
    fill: fn(&Theme) -> Color,
) -> Element<'a, Message> {
    Element::new(Fold {
        content: content.into(),
        toggle: toggle.into(),
        folded,
        height,
        spacing,
        fill,
    })
}

struct Fold<'a, Message> {
    content: Element<'a, Message>,
    toggle: Element<'a, Message>,
    folded: bool,
    height: f32,
    spacing: f32,
    fill: fn(&Theme) -> Color,
}

/// What the last layout found, which drawing and input follow.
#[derive(Default)]
struct State {
    /// The content is tall enough to fold.
    tall: bool,
    /// The height of the content that is shown.
    shown: f32,
    /// The content is cut, not merely tall enough to be.
    cut: bool,
    /// A press began on the shown content and has not been released, as when
    /// the reader is selecting its text.
    pressed: bool,
}

impl<Message> Fold<'_, Message> {
    /// Where the shown part of the content is.
    fn shown(state: &State, layout: Layout<'_>) -> Rectangle {
        let bounds = layout.bounds();
        Rectangle {
            height: state.shown,
            ..bounds
        }
    }

    /// The cursor as the content sees it. Over the cut part the content is
    /// out of reach, so text the reader cannot see is neither hovered nor
    /// clicked; but a press that began on the shown part follows the cursor
    /// past the cut as far as the last shown line, so a selection can still
    /// reach it.
    fn cursor(
        state: &State,
        layout: Layout<'_>,
        content: Layout<'_>,
        cursor: mouse::Cursor,
    ) -> mouse::Cursor {
        let shown = Self::shown(state, layout);
        match cursor.position() {
            Some(position) if state.cut && position.y >= shown.y + shown.height => {
                if state.pressed {
                    mouse::Cursor::Available(Point::new(position.x, shown.y + shown.height - 1.0))
                } else if content.bounds().contains(position) {
                    mouse::Cursor::Unavailable
                } else {
                    cursor
                }
            }
            _ => cursor,
        }
    }
}

/// Passes an operation through to cut content, leaving out the controls the
/// cut hides. Keyboard traversal then skips what the reader cannot see, as
/// assistive technology does. A control that already holds focus is still
/// offered, so traversal can move focus off it. Focusing a cut control by its
/// id is dropped too, on purpose: unfold first to reach it.
struct ShownFocus<'a> {
    inner: &'a mut dyn Operation,
    shown: Rectangle,
}

impl Operation for ShownFocus<'_> {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        let shown = self.shown;
        self.inner.traverse(&mut |inner| {
            operate(&mut ShownFocus { inner, shown });
        });
    }

    fn container(&mut self, id: Option<&widget::Id>, bounds: Rectangle) {
        self.inner.container(id, bounds);
    }

    fn scrollable(
        &mut self,
        id: Option<&widget::Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        self.inner
            .scrollable(id, bounds, content_bounds, translation, state);
    }

    fn focusable(
        &mut self,
        id: Option<&widget::Id>,
        bounds: Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        if state.is_focused() || bounds.y < self.shown.y + self.shown.height {
            self.inner.focusable(id, bounds, state);
        }
    }

    fn text_input(
        &mut self,
        id: Option<&widget::Id>,
        bounds: Rectangle,
        state: &mut dyn operation::TextInput,
    ) {
        self.inner.text_input(id, bounds, state);
    }

    fn text(&mut self, id: Option<&widget::Id>, bounds: Rectangle, text: &str) {
        self.inner.text(id, bounds, text);
    }

    fn custom(&mut self, id: Option<&widget::Id>, bounds: Rectangle, state: &mut dyn Any) {
        self.inner.custom(id, bounds, state);
    }

    fn finish(&self) -> operation::Outcome<()> {
        self.inner.finish()
    }
}

impl<Message> Widget<Message, Theme, Renderer> for Fold<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content), Tree::new(&self.toggle)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut [&mut self.content, &mut self.toggle]);
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let width = limits.max().width;
        let unbounded = layout::Limits::new(Size::ZERO, Size::new(width, f32::INFINITY));
        let content =
            self.content
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, &unbounded);
        let full = content.size().height;
        let tall = full > self.height * (1.0 + SLACK);
        let shown = if tall && self.folded {
            self.height
        } else {
            full
        };
        let (toggle, total) = if tall {
            let toggle = self
                .toggle
                .as_widget_mut()
                .layout(&mut tree.children[1], renderer, &unbounded)
                .move_to(Point::new(0.0, shown + self.spacing));
            let total = shown + self.spacing + toggle.size().height;
            (toggle, total)
        } else {
            (layout::Node::new(Size::ZERO), shown)
        };
        let state = tree.state.downcast_mut::<State>();
        state.tall = tall;
        state.shown = shown;
        state.cut = shown < full;
        layout::Node::with_children(
            limits.resolve(Length::Fill, Length::Shrink, Size::new(width, total)),
            vec![content, toggle],
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let mut children = layout.children();
        let (Some(content), Some(toggle)) = (children.next(), children.next()) else {
            return;
        };
        let content = content.with_virtual_offset(layout.virtual_offset());
        let toggle = toggle.with_virtual_offset(layout.virtual_offset());
        let state = tree.state.downcast_ref::<State>();
        let tall = state.tall;
        if state.cut {
            let mut shown = ShownFocus {
                inner: operation,
                shown: Self::shown(state, layout),
            };
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                content,
                renderer,
                &mut shown,
            );
        } else {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                content,
                renderer,
                operation,
            );
        }
        if tall {
            self.toggle
                .as_widget_mut()
                .operate(&mut tree.children[1], toggle, renderer, operation);
        }
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
        let mut children = layout.children();
        let (Some(content), Some(toggle)) = (children.next(), children.next()) else {
            return;
        };
        let content = content.with_virtual_offset(layout.virtual_offset());
        let toggle = toggle.with_virtual_offset(layout.virtual_offset());
        let state = tree.state.downcast_mut::<State>();
        if let Event::Mouse(mouse::Event::ButtonPressed(_)) = event {
            state.pressed = cursor.is_over(Self::shown(state, layout));
        }
        let tall = state.tall;
        let inner = Self::cursor(state, layout, content, cursor);
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            content,
            inner,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if tall {
            self.toggle.as_widget_mut().update(
                &mut tree.children[1],
                event,
                toggle,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }
        if matches!(event, Event::Mouse(mouse::Event::ButtonReleased(_))) {
            tree.state.downcast_mut::<State>().pressed = false;
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
        let mut children = layout.children();
        let (Some(content), Some(toggle)) = (children.next(), children.next()) else {
            return mouse::Interaction::None;
        };
        let content = content.with_virtual_offset(layout.virtual_offset());
        let toggle = toggle.with_virtual_offset(layout.virtual_offset());
        let state = tree.state.downcast_ref::<State>();
        let shown = self.content.as_widget().mouse_interaction(
            &tree.children[0],
            content,
            Self::cursor(state, layout, content, cursor),
            viewport,
            renderer,
        );
        if shown != mouse::Interaction::None || !state.tall {
            return shown;
        }
        self.toggle.as_widget().mouse_interaction(
            &tree.children[1],
            toggle,
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
        use cosmic::iced::advanced::Renderer as _;
        let mut children = layout.children();
        let (Some(content), Some(toggle)) = (children.next(), children.next()) else {
            return;
        };
        let content = content.with_virtual_offset(layout.virtual_offset());
        let toggle = toggle.with_virtual_offset(layout.virtual_offset());
        let state = tree.state.downcast_ref::<State>();
        let shown = Self::shown(state, layout);
        let cut = shown.height < content.bounds().height;
        let inner = Self::cursor(state, layout, content, cursor);
        if !cut {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                content,
                inner,
                viewport,
            );
        } else if let Some(clip) = shown.intersection(viewport) {
            renderer.with_layer(clip, |renderer| {
                self.content.as_widget().draw(
                    &tree.children[0],
                    renderer,
                    theme,
                    style,
                    content,
                    inner,
                    &clip,
                );
            });
            // A layer draws its quads beneath its text, so the fade over the
            // cut text is a layer of its own.
            renderer.with_layer(clip, |renderer| {
                let fill = (self.fill)(theme);
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            y: shown.y + shown.height - FADE,
                            height: FADE,
                            ..shown
                        },
                        border: Border::default(),
                        shadow: Shadow::default(),
                        snap: true,
                    },
                    // π runs the gradient from top to bottom.
                    Background::Gradient(Gradient::Linear(
                        Linear::new(Radians(std::f32::consts::PI))
                            .add_stop(0.0, Color { a: 0.0, ..fill })
                            .add_stop(1.0, fill),
                    )),
                );
            });
        }
        if state.tall {
            self.toggle.as_widget().draw(
                &tree.children[1],
                renderer,
                theme,
                style,
                toggle,
                cursor,
                viewport,
            );
        }
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        use crate::ui_controls::accessible_bounds::place;

        let mut children = layout.children();
        let (Some(content), Some(toggle)) = (children.next(), children.next()) else {
            return iced_accessibility::A11yTree::default();
        };
        // Descendants report layout coordinates, so the cut is applied in
        // them and the scroll offset once afterwards: text past the fold is
        // hidden as it is from sight, rather than lying over what follows.
        let state = tree.state.downcast_ref::<State>();
        let offset = layout.virtual_offset();
        let mut content = self
            .content
            .as_widget()
            .a11y_nodes(content, &tree.children[0], cursor);
        place(&mut content, offset, Some(Self::shown(state, layout)));
        if !state.tall {
            return content;
        }
        let mut toggle = self
            .toggle
            .as_widget()
            .a11y_nodes(toggle, &tree.children[1], cursor);
        place(&mut toggle, offset, None);
        iced_accessibility::A11yTree::join([content, toggle].into_iter())
    }

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        let mut children = layout.children();
        let (Some(content), Some(toggle)) = (children.next(), children.next()) else {
            return;
        };
        let state = tree.state.downcast_ref::<State>();
        let mut inner = cosmic::iced::advanced::clipboard::DndDestinationRectangles::new();
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            content.with_virtual_offset(layout.virtual_offset()),
            renderer,
            &mut inner,
        );
        let mut rectangles = inner.into_rectangles();
        crate::workspace_widget::clip_drag_destinations(
            &mut rectangles,
            Self::shown(state, layout),
        );
        destinations.append(&mut rectangles);
        if state.tall {
            self.toggle.as_widget().drag_destinations(
                &tree.children[1],
                toggle.with_virtual_offset(layout.virtual_offset()),
                renderer,
                destinations,
            );
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let content = layout
            .children()
            .next()?
            .with_virtual_offset(layout.virtual_offset());
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            content,
            renderer,
            viewport,
            translation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_widget::forwarding_test::{Traverse, leaf, renderer};
    use cosmic::iced::advanced::clipboard::DndDestinationRectangles;

    #[test]
    fn fold_keeps_native_control_bounds_and_action_anchors_in_scroll_coordinates() {
        use crate::workspace_widget::forwarding_test::{native_button, native_controls};
        use iced_accessibility::accesskit::Rect;

        let renderer = renderer();
        let offset = Vector::new(7.0, -35.0);
        for (height, folded, toggle_y) in [
            (200.0, true, Some(130.0)),
            (200.0, false, Some(250.0)),
            (60.0, true, None),
        ] {
            let mut widget = fold(
                native_button("Content", Size::new(100.0, height)),
                native_button("Show more", Size::new(100.0, 20.0)),
                folded,
                80.0,
                10.0,
                |_| Color::BLACK,
            );
            let mut tree = Tree::new(&widget);
            let node = widget
                .as_widget_mut()
                .layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(100.0, 500.0)),
                )
                .move_to(Point::new(30.0, 40.0));
            // The control is cut where the fold cuts what is painted.
            let shown = if folded && height > 60.0 {
                80.0
            } else {
                height
            };
            let mut expected = vec![(
                "Content".to_owned(),
                Rect::new(23.0, 75.0, 123.0, f64::from(75.0 + shown)),
                vec![(
                    "Content",
                    offset,
                    Rectangle::new(Point::new(30.0, 40.0), Size::new(100.0, height)),
                )],
            )];
            if let Some(y) = toggle_y {
                expected.push((
                    "Show more".to_owned(),
                    Rect::new(23.0, f64::from(y + 35.0), 123.0, f64::from(y + 55.0)),
                    vec![(
                        "Show more",
                        offset,
                        Rectangle::new(Point::new(30.0, y), Size::new(100.0, 20.0)),
                    )],
                ));
            }
            assert_eq!(
                native_controls(&mut widget, &mut tree, &node, offset, &renderer),
                expected
            );
        }
    }

    /// Plain text ignores the virtual offset and controls apply it, yet both
    /// must land where they are painted; and what the fold cuts away must not
    /// lie over what follows it.
    #[test]
    fn fold_places_every_accessible_node_where_it_is_painted_and_hides_the_cut() {
        use crate::workspace_widget::forwarding_test::{Press, native_button};
        use iced_accessibility::accesskit::{Rect, Role};

        let renderer = renderer();
        let offset = Vector::new(7.0, -35.0);
        let content: Element<'_, Press> = cosmic::widget::column::with_children(vec![
            cosmic::widget::text("Opening")
                .width(100.0)
                .height(40.0)
                .into(),
            native_button("Kept", Size::new(100.0, 40.0)),
            native_button("Cut", Size::new(100.0, 130.0)),
        ])
        .into();
        let mut widget = fold(
            content,
            cosmic::widget::text("Show more").width(100.0).height(20.0),
            true,
            80.0,
            10.0,
            |_| Color::BLACK,
        );
        let mut tree = Tree::new(&widget);
        let node = widget
            .as_widget_mut()
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(100.0, 500.0)),
            )
            .move_to(Point::new(30.0, 40.0));
        let accessible = widget.as_widget().a11y_nodes(
            Layout::new(&node).with_virtual_offset(offset),
            &tree,
            mouse::Cursor::Unavailable,
        );
        let named = |role: Role, name: &str| {
            accessible
                .root()
                .iter()
                .chain(accessible.children())
                .map(|node| node.node())
                .find(|node| node.role() == role && node.label() == Some(name))
                .unwrap_or_else(|| panic!("{name} is advertised"))
        };
        // Content starts at y 40, which the offset paints at 75.
        assert_eq!(
            named(Role::Paragraph, "Opening").bounds(),
            Some(Rect::new(23.0, 75.0, 123.0, 115.0))
        );
        // The fold shows 80 px: the second control ends there, the third
        // starts there.
        let kept = named(Role::Button, "Kept");
        assert_eq!(kept.bounds(), Some(Rect::new(23.0, 115.0, 123.0, 155.0)));
        assert!(!kept.is_hidden());
        assert!(named(Role::Button, "Cut").is_hidden());
        // The toggle sits past the cut and its spacing: 40 + 80 + 10.
        assert_eq!(
            named(Role::Paragraph, "Show more").bounds(),
            Some(Rect::new(23.0, 165.0, 123.0, 185.0))
        );
    }

    /// Keyboard traversal reaches what the fold shows, and leaves out what it
    /// cuts, unless that control already has focus and must be able to lose
    /// it.
    #[test]
    fn fold_offers_cut_controls_to_keyboard_traversal_only_while_they_hold_focus() {
        use crate::workspace_widget::forwarding_test::{Press, native_button};

        /// The controls traversal is offered, top first, and whether each
        /// has focus. Focuses the one at `focus`, if any.
        struct Offered {
            focus: Option<usize>,
            seen: Vec<(f32, bool)>,
        }
        impl Operation for Offered {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn focusable(
                &mut self,
                _: Option<&widget::Id>,
                bounds: Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                if self.focus == Some(self.seen.len()) {
                    state.focus();
                }
                self.seen.push((bounds.y, state.is_focused()));
            }
        }

        let renderer = renderer();
        let build = |folded| {
            fold(
                cosmic::widget::column::with_children(vec![
                    native_button("Kept", Size::new(100.0, 80.0)),
                    native_button("Cut", Size::new(100.0, 130.0)),
                ]),
                native_button("Show more", Size::new(100.0, 20.0)),
                folded,
                80.0,
                10.0,
                |_| Color::BLACK,
            )
        };
        let mut tree = Tree::new(&build(false));
        let mut offered = |widget: &mut Element<'_, Press>, tree: &mut Tree, focus| {
            tree.diff(widget.as_widget_mut());
            let node = widget.as_widget_mut().layout(
                tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(100.0, 500.0)),
            );
            let mut offered = Offered {
                focus,
                seen: Vec::new(),
            };
            widget.as_widget_mut().operate(
                tree,
                Layout::new(&node).with_virtual_offset(Vector::new(0.0, -35.0)),
                &renderer,
                &mut offered,
            );
            offered.seen
        };

        // The fold shows 80 px, where the cut control starts. Folded, it is
        // skipped; the toggle below the cut is not.
        assert_eq!(
            offered(&mut build(true), &mut tree, None),
            vec![(0.0, false), (90.0, false)]
        );
        // Unfolded, everything is offered. Focus the control the fold cuts.
        assert_eq!(
            offered(&mut build(false), &mut tree, Some(1)),
            vec![(0.0, false), (80.0, true), (220.0, false)]
        );
        // Folding again keeps offering it while it holds focus.
        assert_eq!(
            offered(&mut build(true), &mut tree, None),
            vec![(0.0, false), (80.0, true), (90.0, false)]
        );
    }

    #[test]
    fn fold_forwards_virtual_offsets_and_only_visible_drop_targets() {
        let renderer = renderer();
        let offset = Vector::new(7.0, -35.0);
        for (height, folded, expected) in [
            (
                200.0,
                true,
                vec![(4, 130.0, 20.0), (2, 100.0, 20.0), (1, 40.0, 50.0)],
            ),
            (
                200.0,
                false,
                vec![
                    (4, 250.0, 20.0),
                    (3, 140.0, 20.0),
                    (2, 100.0, 50.0),
                    (1, 40.0, 50.0),
                ],
            ),
            (60.0, true, vec![(1, 40.0, 50.0)]),
        ] {
            let mut targets = vec![(1, Rectangle::new(Point::ORIGIN, Size::new(60.0, 50.0)))];
            if height > 60.0 {
                targets.extend([
                    (
                        2,
                        Rectangle::new(Point::new(0.0, 60.0), Size::new(60.0, 50.0)),
                    ),
                    (
                        3,
                        Rectangle::new(Point::new(0.0, 100.0), Size::new(60.0, 20.0)),
                    ),
                ]);
            }
            let (content, visits) = leaf::<()>(Size::new(100.0, height), targets);
            let (toggle, toggle_visits) = leaf::<()>(
                Size::new(100.0, 20.0),
                vec![(4, Rectangle::new(Point::ORIGIN, Size::new(60.0, 20.0)))],
            );
            let mut widget = fold(content, toggle, folded, 80.0, 10.0, |_| Color::BLACK);
            let mut tree = Tree::new(&widget);
            let node = widget
                .as_widget_mut()
                .layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(100.0, 500.0)),
                )
                .move_to(Point::new(30.0, 40.0));
            let layout = Layout::new(&node).with_virtual_offset(offset);
            widget
                .as_widget_mut()
                .operate(&mut tree, layout, &renderer, &mut Traverse);
            assert!(
                widget
                    .as_widget_mut()
                    .overlay(&mut tree, layout, &renderer, &layout.bounds(), Vector::ZERO)
                    .is_none()
            );
            let mut destinations = DndDestinationRectangles::new();
            widget
                .as_widget()
                .drag_destinations(&tree, layout, &renderer, &mut destinations);
            let actual: Vec<_> = destinations
                .into_native_rectangles()
                .into_iter()
                .map(|target| (target.id, target.rectangle.y, target.rectangle.height))
                .collect();
            assert_eq!(actual, expected);
            assert_eq!(visits.offsets("operate"), vec![offset]);
            assert_eq!(visits.offsets("overlay"), vec![offset]);
            assert_eq!(visits.offsets("drag"), vec![offset]);
            assert_eq!(
                toggle_visits.offsets("drag"),
                if height > 60.0 { vec![offset] } else { vec![] }
            );
            assert_eq!(
                toggle_visits.offsets("operate"),
                if height > 60.0 { vec![offset] } else { vec![] }
            );
        }
    }
}
