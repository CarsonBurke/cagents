//! Layout-time anchoring around iced's native scrollable. Its own state remains
//! authoritative for wheel, thumb, keyboard and touch input.
use super::*;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Id, Operation, Tree, operation, tree},
};
use iced::{Event, Rectangle, Size, Vector};

pub(super) fn transcript<'a>(
    content: impl Into<Element<'a, Message>>,
    first_item: Option<&'a str>,
    last_item: Option<&'a str>,
) -> Element<'a, Message> {
    Element::new(Anchored {
        content: content.into(),
        first_item,
        last_item,
    })
}

struct Anchored<'a> {
    content: Element<'a, Message>,
    first_item: Option<&'a str>,
    last_item: Option<&'a str>,
}

#[derive(Clone, Copy, Debug)]
struct Geometry {
    viewport: Size,
    content: Size,
}

impl Geometry {
    fn travel(self) -> f32 {
        (self.content.height - self.viewport.height).max(0.0)
    }

    // Only the scrollable itself is visited by our private operation; its
    // content tree is deliberately not traversed. Retain two sizes rather
    // than cloning the potentially large transcript's entire layout tree.
    fn node(self) -> layout::Node {
        layout::Node::with_children(self.viewport, vec![layout::Node::new(self.content)])
    }
}

#[derive(Default)]
struct Anchor {
    laid_out: Option<Geometry>,
    first_item: Option<String>,
    last_item: Option<String>,
}

impl Anchor {
    fn layout(
        &mut self,
        first_item: Option<&str>,
        last_item: Option<&str>,
        next: Geometry,
        current_top: Option<f32>,
    ) -> Option<f32> {
        let offset = self.laid_out.zip(current_top).and_then(|(previous, top)| {
            let bottom = (previous.travel() - top).max(0.0);
            // Compare with the last *layout*, not the last provider refresh:
            // prepend followed by any number of status updates is still a
            // prepend. Freeze its measured bottom distance as an absolute
            // offset: Home leaves iced at Relative(1), which would otherwise
            // follow the newly prepended top and repeatedly load older pages.
            if self.first_item.as_deref() != first_item {
                if self.last_item.as_deref() != last_item {
                    // Replacing a bounded history page starts at its newest
                    // boundary. It is not a prepend of the same transcript.
                    return Some(0.0);
                }
                return Some(bottom.min(next.travel()));
            }
            let desired = if bottom <= 0.5 {
                // The live end follows output and viewport resizing.
                0.0
            } else {
                // Read the real offset immediately before layout. A wheel,
                // thumb drag or keyboard operation is never overwritten by
                // an older application-level viewport notification.
                (next.travel() - top).max(0.0)
            };
            ((desired - bottom).abs() > 0.5).then_some(desired)
        });
        self.laid_out = Some(next);
        if self.first_item.as_deref() != first_item {
            self.first_item = first_item.map(str::to_owned);
        }
        if self.last_item.as_deref() != last_item {
            self.last_item = last_item.map(str::to_owned);
        }
        offset
    }
}

#[derive(Default)]
struct ReadTop(Option<f32>);

impl Operation for ReadTop {
    fn traverse(&mut self, _: &mut dyn FnMut(&mut dyn Operation)) {}

    fn scrollable(
        &mut self,
        _: Option<&Id>,
        _: Rectangle,
        _: Rectangle,
        translation: Vector,
        _: &mut dyn operation::Scrollable,
    ) {
        self.0 = Some(translation.y);
    }
}

struct SetBottom(f32);

impl Operation for SetBottom {
    fn traverse(&mut self, _: &mut dyn FnMut(&mut dyn Operation)) {}

    fn scrollable(
        &mut self,
        _: Option<&Id>,
        _: Rectangle,
        _: Rectangle,
        _: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
            x: None,
            y: Some(self.0),
        });
    }
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Anchored<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Anchor>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Anchor::default())
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

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let anchor = tree.state.downcast_mut::<Anchor>();
        let child = &mut tree.children[0];
        let mut top = ReadTop::default();
        if let Some(previous) = anchor.laid_out {
            self.content.as_widget_mut().operate(
                child,
                Layout::new(&previous.node()),
                renderer,
                &mut top,
            );
        }
        let node = self.content.as_widget_mut().layout(child, renderer, limits);
        let next = Geometry {
            viewport: node.size(),
            content: node.children()[0].size(),
        };
        if let Some(offset) = anchor.layout(self.first_item, self.last_item, next, top.0) {
            self.content.as_widget_mut().operate(
                child,
                Layout::new(&node),
                renderer,
                &mut SetBottom(offset),
            );
        }
        node
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
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
        renderer: &cosmic::Renderer,
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
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
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
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, cosmic::Theme, cosmic::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(content: f32) -> Geometry {
        Geometry {
            viewport: Size::new(400.0, 400.0),
            content: Size::new(400.0, content),
        }
    }

    fn reading() -> Anchor {
        let mut anchor = Anchor::default();
        assert_eq!(
            anchor.layout(Some("first"), Some("last"), geometry(2000.0), None),
            None
        );
        anchor
    }

    #[test]
    fn appends_preserve_reading_position_and_live_end_follows() {
        assert_eq!(
            reading().layout(Some("first"), Some("last"), geometry(2200.0), Some(600.0)),
            Some(1200.0)
        );
        assert_eq!(
            reading().layout(Some("first"), Some("last"), geometry(2200.0), Some(1600.0)),
            None
        );
    }

    #[test]
    fn user_input_before_layout_is_authoritative() {
        // The app might still know top=600, but the user has scrolled to 900.
        assert_eq!(
            reading().layout(Some("first"), Some("last"), geometry(2200.0), Some(900.0)),
            Some(900.0)
        );
        // End was pressed before layout: stay at the live end, not old history.
        assert_eq!(
            reading().layout(Some("first"), Some("last"), geometry(2200.0), Some(1600.0)),
            None
        );
    }

    #[test]
    fn prepends_are_classified_against_last_layout_not_intermediate_refreshes() {
        let mut anchor = reading();
        // Any number of provider/status refreshes before layout cannot change
        // this marker. Bottom anchoring retains the old message after prepend.
        assert_eq!(
            anchor.layout(Some("earlier"), Some("last"), geometry(2500.0), Some(600.0)),
            Some(1000.0)
        );
        // The same message now sits 500px farther down. An append preserves it.
        assert_eq!(
            anchor.layout(
                Some("earlier"),
                Some("last"),
                geometry(2700.0),
                Some(1100.0)
            ),
            Some(1200.0)
        );
    }

    #[test]
    fn home_then_prepend_preserves_the_old_top_instead_of_following_new_history() {
        let mut anchor = reading();
        // Home snaps iced to Relative(1). Convert that old 1600px bottom
        // distance to Absolute(1600), putting the same message at top=500.
        let bottom = anchor
            .layout(Some("earlier"), Some("last"), geometry(2500.0), Some(0.0))
            .expect("prepend must replace the relative offset");
        assert_eq!(bottom, 1600.0);
        assert_eq!(geometry(2500.0).travel() - bottom, 500.0);
        assert_eq!(
            anchor.layout(Some("earlier"), Some("last"), geometry(2500.0), Some(500.0)),
            None,
            "an unchanged layout must not scroll back to the pagination edge"
        );
        assert_eq!(
            reading().layout(
                Some("earlier"),
                Some("last"),
                geometry(2500.0),
                Some(1600.0)
            ),
            Some(0.0),
            "prepending while at the live end keeps the live end visible"
        );
    }

    #[test]
    fn no_pending_correction_survives_an_unchanged_layout() {
        let mut anchor = reading();
        assert_eq!(
            anchor.layout(Some("first"), Some("last"), geometry(2000.0), Some(600.0)),
            None
        );
        assert_eq!(
            anchor.layout(Some("first"), Some("last"), geometry(2200.0), Some(900.0)),
            Some(900.0)
        );
    }
    #[test]
    fn replaced_history_pages_start_at_the_newest_boundary() {
        let mut anchor = reading();
        assert_eq!(
            anchor.layout(Some("older"), Some("first"), geometry(900.0), Some(0.0)),
            Some(0.0)
        );
        assert_eq!(
            anchor.layout(Some("older"), Some("first"), geometry(900.0), Some(500.0)),
            None
        );
    }
}
