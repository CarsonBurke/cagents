//! A row whose name keeps its room, and whose trailing state gives way to a
//! glyph when the two no longer fit.
//!
//! A transcript row reads as who or what, then how it stands: `Started
//! Sidebar direction 1 … Finished`. In a narrow pane a row sized in order
//! hands its room to the state at the end and cuts the name, which is the
//! part the reader needs. This lays the state out in full only when the
//! name fits beside it, and otherwise puts a compact form of it in its
//! place, such as a still dot or nothing at all, the leading glyph already
//! telling a failure.

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use cosmic::iced::{Event, Length, Point, Rectangle, Size, Vector};
use cosmic::{Element, Renderer, Theme};

const LEAD: usize = 0;
const MEASURE: usize = 1;
const FULL: usize = 2;
const COMPACT: usize = 3;

/// `lead`, which fills the row, then `full` at its trailing edge when
/// `name` fits beside it, or `compact` when it does not.
///
/// `name` is measured, never drawn: the words in `lead` that must not be
/// cut for the state's sake, at their own size and face, sized to their
/// contents. `lead` takes whatever the trail leaves it, so give its text a
/// [`FadeText`](super::FadeText) where it may still run short.
pub fn yielding<'a, Message: 'a>(
    lead: impl Into<Element<'a, Message>>,
    name: impl Into<Element<'a, Message>>,
    full: impl Into<Element<'a, Message>>,
    compact: Option<Element<'a, Message>>,
    spacing: f32,
) -> Element<'a, Message> {
    Element::new(Yielding {
        children: [
            lead.into(),
            name.into(),
            full.into(),
            compact.unwrap_or_else(|| cosmic::widget::Space::new().into()),
        ],
        spacing,
    })
}

struct Yielding<'a, Message> {
    children: [Element<'a, Message>; 4],
    spacing: f32,
}

/// Which trail the last layout chose.
#[derive(Default)]
struct Shown {
    compact: bool,
}

impl<Message> Yielding<'_, Message> {
    /// The children drawn, by index: the lead and the chosen trail.
    fn shown(tree: &Tree) -> [usize; 2] {
        if tree.state.downcast_ref::<Shown>().compact {
            [LEAD, COMPACT]
        } else {
            [LEAD, FULL]
        }
    }
}

impl<Message> Widget<Message, Theme, Renderer> for Yielding<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Shown>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Shown::default())
    }

    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.children);
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
        let max = limits.max();
        let compression = limits.compression();
        let within = |width: f32| {
            layout::Limits::with_compression(Size::ZERO, Size::new(width, max.height), compression)
        };
        let name = self.children[MEASURE]
            .as_widget_mut()
            .layout(
                &mut tree.children[MEASURE],
                renderer,
                &within(f32::INFINITY),
            )
            .size()
            .width;
        let full = self.children[FULL].as_widget_mut().layout(
            &mut tree.children[FULL],
            renderer,
            &within(max.width),
        );
        let fits = name + self.spacing + full.size().width <= max.width;
        let trail = if fits {
            full
        } else {
            self.children[COMPACT].as_widget_mut().layout(
                &mut tree.children[COMPACT],
                renderer,
                &within(max.width),
            )
        };
        tree.state.downcast_mut::<Shown>().compact = !fits;
        let gap = if trail.size().width > 0.0 {
            self.spacing
        } else {
            0.0
        };
        let room = (max.width - trail.size().width - gap).max(0.0);
        let lead = self.children[LEAD].as_widget_mut().layout(
            &mut tree.children[LEAD],
            renderer,
            &within(room),
        );
        let height = lead.size().height.max(trail.size().height);
        let size = limits.resolve(Length::Fill, Length::Shrink, Size::new(max.width, height));
        let centred = |node: &layout::Node| (size.height - node.size().height) / 2.0;
        let lead_y = centred(&lead);
        let trail_y = centred(&trail);
        let trail_x = size.width - trail.size().width;
        let hidden = || layout::Node::new(Size::ZERO);
        let (full, compact) = if fits {
            (trail.move_to(Point::new(trail_x, trail_y)), hidden())
        } else {
            (hidden(), trail.move_to(Point::new(trail_x, trail_y)))
        };
        layout::Node::with_children(
            size,
            vec![
                lead.move_to(Point::new(0.0, lead_y)),
                hidden(),
                full,
                compact,
            ],
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let shown = Self::shown(tree);
        let layouts: Vec<_> = layout.children().collect();
        operation.traverse(&mut |operation| {
            for index in shown {
                self.children[index].as_widget_mut().operate(
                    &mut tree.children[index],
                    layouts[index].with_virtual_offset(layout.virtual_offset()),
                    renderer,
                    operation,
                );
            }
        });
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
        let layouts: Vec<_> = layout.children().collect();
        for index in Self::shown(tree) {
            self.children[index].as_widget_mut().update(
                &mut tree.children[index],
                event,
                layouts[index].with_virtual_offset(layout.virtual_offset()),
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
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
        let layouts: Vec<_> = layout.children().collect();
        for index in Self::shown(tree) {
            self.children[index].as_widget().draw(
                &tree.children[index],
                renderer,
                theme,
                style,
                layouts[index].with_virtual_offset(layout.virtual_offset()),
                cursor,
                viewport,
            );
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
        let layouts: Vec<_> = layout.children().collect();
        Self::shown(tree)
            .into_iter()
            .map(|index| {
                self.children[index].as_widget().mouse_interaction(
                    &tree.children[index],
                    layouts[index].with_virtual_offset(layout.virtual_offset()),
                    cursor,
                    viewport,
                    renderer,
                )
            })
            .max()
            .unwrap_or_default()
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        // Only the lead, a row's words, can hold an overlay such as a
        // tooltip; a trail is a caption or a glyph.
        let lead = layout.children().next()?;
        self.children[LEAD].as_widget_mut().overlay(
            &mut tree.children[LEAD],
            lead.with_virtual_offset(layout.virtual_offset()),
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
        let layouts: Vec<_> = layout.children().collect();
        iced_accessibility::A11yTree::join(Self::shown(tree).into_iter().map(|index| {
            self.children[index].as_widget().a11y_nodes(
                layouts[index].with_virtual_offset(layout.virtual_offset()),
                &tree.children[index],
                cursor,
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use cosmic::widget;

    fn fixed<'a>(width: f32) -> Element<'a, ()> {
        widget::Space::new().width(width).height(16.0).into()
    }

    /// Lays a row out `available` wide, with a name 100 wide, a full trail
    /// 60 wide and a compact one 6 wide: the lead's bounds, and the shown
    /// trail's.
    fn row(available: f32) -> (Rectangle, Rectangle, bool) {
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut element: Element<'_, ()> = yielding(
            widget::container(fixed(100.0)).width(Length::Fill),
            fixed(100.0),
            fixed(60.0),
            Some(fixed(6.0)),
            8.0,
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(available, 600.0)),
        );
        let compact = tree.state.downcast_ref::<Shown>().compact;
        let parts: Vec<_> = Layout::new(&node)
            .children()
            .map(|part| part.bounds())
            .collect();
        let trail = if compact { parts[COMPACT] } else { parts[FULL] };
        (parts[LEAD], trail, compact)
    }

    /// Where the name and the state fit, both show, the state at the end.
    #[test]
    fn a_wide_row_shows_its_state_in_full() {
        let (lead, trail, compact) = row(400.0);
        assert!(!compact);
        assert_eq!(trail.width, 60.0);
        assert_eq!(trail.x + trail.width, 400.0);
        assert!(lead.x + lead.width <= trail.x - 8.0 + 0.01);
    }

    /// Where they no longer fit, the state gives way to its glyph and the
    /// name keeps its room.
    #[test]
    fn a_narrow_row_keeps_its_name_and_marks_its_state() {
        for available in [167.0, 140.0, 114.0] {
            let (lead, trail, compact) = row(available);
            assert!(compact, "{available}");
            assert_eq!(trail.width, 6.0, "{available}");
            assert_eq!(trail.x + trail.width, available, "{available}");
            assert_eq!(lead.width, available - 6.0 - 8.0, "{available}");
            assert!(lead.width >= 100.0, "{available}: the name fits");
        }
    }
}
