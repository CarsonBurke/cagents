//! Content shown up to a height, with a control that shows the rest.
//!
//! Whether there is a rest to show is a question about the content as drawn:
//! Markdown joins a paragraph's lines and wraps them again at the width it is
//! given, so neither its line count nor its length says how tall it will be.
//! The fold lays the content out and decides from that, offering its control
//! only when the content does not fit.

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use cosmic::iced::{
    Background, Border, Color, Event, Gradient, Length, Point, Radians, Rectangle, Shadow, Size,
    Vector, gradient::Linear, mouse,
};
use cosmic::{Element, Renderer, Theme};

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
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], content, renderer, operation);
        if tree.state.downcast_ref::<State>().tall {
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
        let mut children = layout.children();
        let (Some(content), Some(toggle)) = (children.next(), children.next()) else {
            return iced_accessibility::A11yTree::default();
        };
        let content = self
            .content
            .as_widget()
            .a11y_nodes(content, &tree.children[0], cursor);
        if !tree.state.downcast_ref::<State>().tall {
            return content;
        }
        let toggle = self
            .toggle
            .as_widget()
            .a11y_nodes(toggle, &tree.children[1], cursor);
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
        // Extraction reverses stacking order; restore insertion order before
        // appending to the enclosing collection, which performs its own reversal.
        let mut rectangles = inner.into_rectangles();
        rectangles.reverse();
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
            let mut expected = vec![(
                "Content".to_owned(),
                Rect::new(23.0, 75.0, 123.0, f64::from(75.0 + height)),
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
                .into_rectangles()
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
