//! Chat hover for tool rows and messages, preserving the semantic subtree,
//! targeted actions and the scrollable's virtual offset.
//! Adapted from iced/widget/src/helpers.rs hover at libcosmic caec74c2559924443f12fc6faf97a5bcefe6271d.
/*
Copyright 2019 Héctor Ramón, Iced contributors

Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
*/
use cosmic::iced;
use iced::advanced as core;
use iced::advanced::widget::Operation as _;
use iced::advanced::{Widget, widget::operation};
use iced::{Element, Length, window};

/// Where the hover actions sit relative to the content they act on.
#[derive(Clone, Copy)]
enum Placement {
    /// Beside the content, in space reserved for them, as a tool row's header.
    Beside(f32),
    /// Beside a header that more content follows, as an open tool row's: the
    /// actions keep to the header's row and the content below keeps to the
    /// header's width rather than running under them. Where they move below
    /// the header in a narrow pane, they come before that content.
    BesideFirst(f32),
    /// Over the content, within its bounds, as iced's own `hover` places them.
    Over,
}

/// Actions beside a tool row's header, revealed on hover or focus.
pub(super) fn view<'a, Message, Theme, Renderer>(
    base: impl Into<Element<'a, Message, Theme, Renderer>>,
    top: impl Into<Element<'a, Message, Theme, Renderer>>,
    spacing: f32,
) -> Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: core::Renderer + 'a,
{
    hover(base, top, None, Placement::Beside(spacing))
}

/// Where actions `height` tall sit beside a header: centred on its first
/// row, the line that names the step, rather than on the whole header, whose
/// later lines are previews of its output. The first row is the band of the
/// first line of each element in the header's row: its icons, its verb, and
/// the first line of the column beside them. No line in a header is taller
/// than body text's, so a command that wraps onto a second line, laid out as
/// one element, adds only its first line to the band.
fn first_row_offset(header: &core::layout::Node, height: f32) -> f32 {
    // Through the wrappers, a button and its padding, to the row itself.
    let mut row = header;
    let mut offset = 0.0;
    while let [only] = row.children() {
        offset += only.bounds().y;
        row = only;
    }
    let band = row
        .children()
        .iter()
        .map(|element| {
            let first = element.children().first();
            let top = offset + element.bounds().y + first.map_or(0.0, |first| first.bounds().y);
            let height = first
                .map_or(element.bounds().height, |first| first.bounds().height)
                .min(crate::design::LINE_BODY);
            (top, top + height)
        })
        .reduce(|(top, bottom), (t, b)| (top.min(t), bottom.max(b)));
    let (top, bottom) = band.unwrap_or((0.0, header.size().height));
    (top + (bottom - top - height) / 2.0).clamp(0.0, (header.size().height - height).max(0.0))
}

/// Where the pointer reveals the actions: the whole wrapper, or for an open
/// row, the band of its header and the actions themselves, so the open
/// detail below keeps to its own controls.
fn hover_bounds(placement: Placement, layout: core::Layout<'_>) -> iced::Rectangle {
    let bounds = layout.bounds();
    match placement {
        Placement::BesideFirst(_) => {
            let bottom = layout
                .children()
                .take(2)
                .map(|child| child.bounds().y + child.bounds().height)
                .fold(bounds.y, f32::max);
            iced::Rectangle {
                height: bottom - bounds.y,
                ..bounds
            }
        }
        _ => bounds,
    }
}

/// Actions beside `header`, with `rest` below them both: an open row, whose
/// detail keeps to its header's width. Focus moves from the header to the
/// actions and then into the detail, in the order they are read.
pub(super) fn beside_first<'a, Message, Theme, Renderer>(
    header: impl Into<Element<'a, Message, Theme, Renderer>>,
    top: impl Into<Element<'a, Message, Theme, Renderer>>,
    rest: impl Into<Element<'a, Message, Theme, Renderer>>,
    spacing: f32,
) -> Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: core::Renderer + 'a,
{
    hover(
        header,
        top,
        Some(rest.into()),
        Placement::BesideFirst(spacing),
    )
}

/// iced's `hover`, laid out the same way, that keeps the scroll offset and
/// accessibility tree of what it wraps. The pinned upstream one rebuilds its
/// children's layouts without the scrollable's virtual offset, which anchors a
/// context menu inside a scrolled message in content space, and it exposes no
/// accessibility nodes for the message at all.
pub(super) fn over<'a, Message, Theme, Renderer>(
    base: impl Into<Element<'a, Message, Theme, Renderer>>,
    top: impl Into<Element<'a, Message, Theme, Renderer>>,
) -> Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: core::Renderer + 'a,
{
    hover(base, top, None, Placement::Over)
}

/// The hover widget: `base`, the `top` actions it reveals, and for an open
/// row, the `rest` that follows its header. Children are kept, laid out and
/// visited in reading order: base, top, rest.
fn hover<'a, Message, Theme, Renderer>(
    base: impl Into<Element<'a, Message, Theme, Renderer>>,
    top: impl Into<Element<'a, Message, Theme, Renderer>>,
    rest: Option<Element<'a, Message, Theme, Renderer>>,
    placement: Placement,
) -> Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: core::Renderer + 'a,
{
    use iced::advanced::Shell;
    use iced::advanced::layout::{self, Layout};
    use iced::advanced::mouse;
    use iced::advanced::renderer;
    use iced::advanced::widget::tree::{self, Tree};
    use iced::{Event, Rectangle, Size};

    struct Hover<'a, Message, Theme, Renderer> {
        base: Element<'a, Message, Theme, Renderer>,
        top: Element<'a, Message, Theme, Renderer>,
        rest: Option<Element<'a, Message, Theme, Renderer>>,
        placement: Placement,
    }

    impl<'a, Message, Theme, Renderer> Hover<'a, Message, Theme, Renderer> {
        /// The children in reading order: base, top, then any rest.
        fn elements(
            &self,
        ) -> impl DoubleEndedIterator<Item = &Element<'a, Message, Theme, Renderer>> {
            [Some(&self.base), Some(&self.top), self.rest.as_ref()]
                .into_iter()
                .flatten()
        }

        fn elements_mut(
            &mut self,
        ) -> impl Iterator<Item = &mut Element<'a, Message, Theme, Renderer>> {
            [
                Some(&mut self.base),
                Some(&mut self.top),
                self.rest.as_mut(),
            ]
            .into_iter()
            .flatten()
        }
    }

    fn child_layouts<'b>(
        layout: Layout<'b>,
    ) -> impl DoubleEndedIterator<Item = Layout<'b>> + ExactSizeIterator {
        layout
            .children()
            .map(move |child| child.with_virtual_offset(layout.virtual_offset()))
    }

    #[derive(Default)]
    struct State {
        is_top_focused: bool,
        is_top_overlay_active: bool,
        is_hovered: bool,
    }

    impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
        for Hover<'_, Message, Theme, Renderer>
    where
        Renderer: core::Renderer,
    {
        fn tag(&self) -> tree::Tag {
            tree::Tag::of::<State>()
        }

        fn state(&self) -> tree::State {
            tree::State::new(State::default())
        }

        fn children(&self) -> Vec<Tree> {
            self.elements().map(Tree::new).collect()
        }

        fn diff(&mut self, tree: &mut Tree) {
            tree.diff_children(&mut self.elements_mut().collect::<Vec<_>>());
        }

        fn size(&self) -> Size<Length> {
            self.base.as_widget().size()
        }

        fn size_hint(&self) -> Size<Length> {
            self.base.as_widget().size_hint()
        }

        fn layout(
            &mut self,
            tree: &mut Tree,
            renderer: &Renderer,
            limits: &layout::Limits,
        ) -> layout::Node {
            let spacing = match self.placement {
                Placement::Beside(spacing) => spacing,
                Placement::BesideFirst(spacing) => {
                    return self.layout_beside_first(tree, renderer, limits, spacing);
                }
                Placement::Over => {
                    let base =
                        self.base
                            .as_widget_mut()
                            .layout(&mut tree.children[0], renderer, limits);
                    let top = self.top.as_widget_mut().layout(
                        &mut tree.children[1],
                        renderer,
                        &layout::Limits::new(Size::ZERO, base.size()),
                    );
                    return layout::Node::with_children(base.size(), vec![base, top]);
                }
            };
            // The hidden actions reserve exactly the space they will paint.
            // A long date moves below the header in narrow panes, leaving at
            // least half the row for the command rather than covering it.
            let top =
                self.top
                    .as_widget_mut()
                    .layout(&mut tree.children[1], renderer, &limits.loose());
            let stacked = top.size().width + spacing > limits.max().width / 2.0;
            let base_limits = if stacked {
                limits.shrink(Size::new(0.0, top.size().height + spacing))
            } else {
                limits.shrink(Size::new(top.size().width + spacing, 0.0))
            };
            let base =
                self.base
                    .as_widget_mut()
                    .layout(&mut tree.children[0], renderer, &base_limits);
            let natural = if stacked {
                Size::new(
                    base.size().width.max(top.size().width),
                    base.size().height + spacing + top.size().height,
                )
            } else {
                Size::new(
                    base.size().width + spacing + top.size().width,
                    base.size().height.max(top.size().height),
                )
            };
            let size = limits.resolve(Length::Fill, Length::Shrink, natural);
            let (base, top) = if stacked {
                let y = base.size().height + spacing;
                let x = (size.width - top.size().width).max(0.0);
                (base, top.move_to(iced::Point::new(x, y)))
            } else {
                let base_y = (size.height - base.size().height) / 2.0;
                let top_y = base_y + first_row_offset(&base, top.size().height);
                let x = (size.width - top.size().width).max(0.0);
                (
                    base.move_to(iced::Point::new(0.0, base_y)),
                    top.move_to(iced::Point::new(x, top_y)),
                )
            };
            layout::Node::with_children(size, vec![base, top])
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
            if let Some(bounds) = layout.bounds().intersection(viewport) {
                let mut children = child_layouts(layout).zip(&tree.children);

                let (base_layout, base_tree) = children.next().unwrap();

                self.base.as_widget().draw(
                    base_tree,
                    renderer,
                    theme,
                    style,
                    base_layout,
                    cursor,
                    viewport,
                );

                if let Some(rest) = &self.rest {
                    let (rest_layout, rest_tree) =
                        child_layouts(layout).zip(&tree.children).nth(2).unwrap();
                    rest.as_widget().draw(
                        rest_tree,
                        renderer,
                        theme,
                        style,
                        rest_layout,
                        cursor,
                        viewport,
                    );
                }

                if cursor.is_over(hover_bounds(self.placement, layout))
                    || tree.state.downcast_ref::<State>().is_top_focused
                    || tree.state.downcast_ref::<State>().is_top_overlay_active
                {
                    let (top_layout, top_tree) = children.next().unwrap();

                    renderer.with_layer(bounds, |renderer| {
                        self.top.as_widget().draw(
                            top_tree, renderer, theme, style, top_layout, cursor, viewport,
                        );
                    });
                }
            }
        }

        fn operate(
            &mut self,
            tree: &mut Tree,
            layout: Layout<'_>,
            renderer: &Renderer,
            operation: &mut dyn operation::Operation,
        ) {
            // Reading order: the header, its actions, then what follows.
            let children = self
                .elements_mut()
                .zip(child_layouts(layout).zip(&mut tree.children));

            for (child, (layout, tree)) in children {
                child
                    .as_widget_mut()
                    .operate(tree, layout, renderer, operation);
            }
        }

        fn update(
            &mut self,
            tree: &mut Tree,
            event: &Event,
            layout: Layout<'_>,
            cursor: mouse::Cursor,
            renderer: &Renderer,
            clipboard: &mut dyn core::Clipboard,
            shell: &mut Shell<'_, Message>,
            viewport: &Rectangle,
        ) {
            if let Event::A11y(id, request) = event {
                use iced_accessibility::{
                    A11yId, IdEq,
                    accesskit::{Action, TreeId},
                };
                let target = A11yId::from(id.clone());
                if request.target_tree != TreeId::ROOT
                    || request.target_node != target.clone().into()
                {
                    return;
                }
                let branch = self
                    .elements()
                    .zip(child_layouts(layout).zip(&tree.children))
                    .position(|(child, (layout, tree))| {
                        let accessible = child.as_widget().a11y_nodes(layout, tree, cursor);
                        accessible
                            .root()
                            .iter()
                            .chain(accessible.children())
                            .any(|node| {
                                IdEq::eq(node.id(), &target)
                                    && node.node().supports_action(
                                        if request.action == Action::Blur {
                                            Action::Focus
                                        } else {
                                            request.action
                                        },
                                    )
                            })
                    });
                let Some(branch) = branch else { return };
                match request.action {
                    Action::Focus => {
                        let mut focus = operation::focusable::focus::<()>(id.clone());
                        self.operate(tree, layout, renderer, &mut focus);
                        tree.state.downcast_mut::<State>().is_top_focused = branch == 1;
                    }
                    Action::Blur => {
                        let mut unfocus = operation::focusable::unfocus::<()>();
                        let child = self.elements_mut().nth(branch).unwrap();
                        child.as_widget_mut().operate(
                            &mut tree.children[branch],
                            child_layouts(layout).nth(branch).unwrap(),
                            renderer,
                            &mut unfocus,
                        );
                        if branch == 1 {
                            tree.state.downcast_mut::<State>().is_top_focused = false;
                        }
                    }
                    _ => {
                        let child = self.elements_mut().nth(branch).unwrap();
                        child.as_widget_mut().update(
                            &mut tree.children[branch],
                            event,
                            child_layouts(layout).nth(branch).unwrap(),
                            cursor,
                            renderer,
                            clipboard,
                            shell,
                            viewport,
                        );
                        return;
                    }
                }
                shell.capture_event();
                shell.request_redraw();
                return;
            }
            let mut children = child_layouts(layout).zip(&mut tree.children);
            let (base_layout, base_tree) = children.next().unwrap();
            let (top_layout, top_tree) = children.next().unwrap();
            let rest = children.next();

            let is_hovered = cursor.is_over(hover_bounds(self.placement, layout));

            if matches!(event, Event::Window(window::Event::RedrawRequested(_))) {
                let mut count_focused = operation::focusable::count();

                self.top.as_widget_mut().operate(
                    top_tree,
                    top_layout,
                    renderer,
                    &mut operation::black_box(&mut count_focused),
                );

                tree.state.downcast_mut::<State>().is_top_focused = match count_focused.finish() {
                    operation::Outcome::Some(count) => count.focused.is_some(),
                    _ => false,
                };

                tree.state.downcast_mut::<State>().is_hovered = is_hovered;
            } else if is_hovered != tree.state.downcast_ref::<State>().is_hovered {
                shell.request_redraw();
            }

            let is_visible = is_hovered
                || tree.state.downcast_ref::<State>().is_top_focused
                || tree.state.downcast_ref::<State>().is_top_overlay_active;

            if matches!(
                event,
                Event::Keyboard(iced::keyboard::Event::KeyReleased { .. })
                    | Event::Mouse(
                        mouse::Event::CursorMoved { .. } | mouse::Event::ButtonReleased(_)
                    )
            ) || is_visible
            {
                let redraw_request = shell.redraw_request();

                self.top.as_widget_mut().update(
                    top_tree, event, top_layout, cursor, renderer, clipboard, shell, viewport,
                );

                // Ignore redraw requests of invisible content
                if !is_visible {
                    Shell::replace_redraw_request(shell, redraw_request);
                }

                if shell.is_event_captured() {
                    return;
                }
            };

            self.base.as_widget_mut().update(
                base_tree,
                event,
                base_layout,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
            if let (Some(element), Some((rest_layout, rest_tree))) = (&mut self.rest, rest)
                && !shell.is_event_captured()
            {
                element.as_widget_mut().update(
                    rest_tree,
                    event,
                    rest_layout,
                    cursor,
                    renderer,
                    clipboard,
                    shell,
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
            use iced_accessibility::{A11yTree, accesskit::Action};
            // Hover hides a paint layer, not the controls' keyboard/AT reach.
            let mut accessible = A11yTree::join(
                self.elements()
                    .zip(child_layouts(layout).zip(&tree.children))
                    .map(|(child, (layout, tree))| {
                        child.as_widget().a11y_nodes(layout, tree, cursor)
                    }),
            );
            for node in accessible.root_mut() {
                if node.node().supports_action(Action::Focus) {
                    node.node_mut().add_action(Action::Blur);
                }
            }
            for node in accessible.children_mut() {
                if node.node().supports_action(Action::Focus) {
                    node.node_mut().add_action(Action::Blur);
                }
            }
            accessible
        }

        fn mouse_interaction(
            &self,
            tree: &Tree,
            layout: Layout<'_>,
            cursor: mouse::Cursor,
            viewport: &Rectangle,
            renderer: &Renderer,
        ) -> mouse::Interaction {
            self.elements()
                .rev()
                .zip(child_layouts(layout).rev().zip(tree.children.iter().rev()))
                .map(|(child, (layout, tree))| {
                    child
                        .as_widget()
                        .mouse_interaction(tree, layout, cursor, viewport, renderer)
                })
                .find(|&interaction| interaction != mouse::Interaction::None)
                .unwrap_or_default()
        }

        fn overlay<'b>(
            &'b mut self,
            tree: &'b mut core::widget::Tree,
            layout: core::Layout<'b>,
            renderer: &Renderer,
            viewport: &Rectangle,
            translation: iced::Vector,
        ) -> Option<core::overlay::Element<'b, Message, Theme, Renderer>> {
            let mut overlays = [
                Some(&mut self.base),
                Some(&mut self.top),
                self.rest.as_mut(),
            ]
            .into_iter()
            .flatten()
            .zip(child_layouts(layout).zip(tree.children.iter_mut()))
            .map(|(child, (layout, tree))| {
                child
                    .as_widget_mut()
                    .overlay(tree, layout, renderer, viewport, translation)
            });

            if let Some(base_overlay) = overlays.next()? {
                return Some(base_overlay);
            }

            let top_overlay = overlays.next()?;
            tree.state.downcast_mut::<State>().is_top_overlay_active = top_overlay.is_some();

            top_overlay.or_else(|| overlays.next().flatten())
        }
    }

    impl<Message, Theme, Renderer> Hover<'_, Message, Theme, Renderer>
    where
        Renderer: core::Renderer,
    {
        /// The header with the actions beside it and the rest below, keeping
        /// to the header's width; or, where the actions would take more than
        /// half the row, the actions under the header and the rest under them.
        fn layout_beside_first(
            &mut self,
            tree: &mut Tree,
            renderer: &Renderer,
            limits: &layout::Limits,
            spacing: f32,
        ) -> layout::Node {
            let top =
                self.top
                    .as_widget_mut()
                    .layout(&mut tree.children[1], renderer, &limits.loose());
            let stacked = top.size().width + spacing > limits.max().width / 2.0;
            let inner = if stacked {
                *limits
            } else {
                limits.shrink(Size::new(top.size().width + spacing, 0.0))
            };
            let header = self
                .base
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, &inner);
            let header_height = header.size().height;
            let (top_y, rest_y) = if stacked {
                let top_y = header_height + spacing;
                (top_y, top_y + top.size().height + spacing)
            } else {
                (
                    first_row_offset(&header, top.size().height),
                    header_height + spacing,
                )
            };
            let rest = self.rest.as_mut().map(|rest| {
                rest.as_widget_mut()
                    .layout(
                        &mut tree.children[2],
                        renderer,
                        &inner.shrink(Size::new(0.0, rest_y)),
                    )
                    .move_to(iced::Point::new(0.0, rest_y))
            });
            let content_width = header
                .size()
                .width
                .max(rest.as_ref().map_or(0.0, |rest| rest.size().width));
            let bottom = rest
                .as_ref()
                .map_or(header_height.max(top_y + top.size().height), |rest| {
                    rest_y + rest.size().height
                });
            let natural = if stacked {
                Size::new(content_width.max(top.size().width), bottom)
            } else {
                Size::new(content_width + spacing + top.size().width, bottom)
            };
            let size = limits.resolve(Length::Fill, Length::Shrink, natural);
            let top_x = (size.width - top.size().width).max(0.0);
            let mut children = vec![header, top.move_to(iced::Point::new(top_x, top_y))];
            children.extend(rest);
            layout::Node::with_children(size, children)
        }
    }

    Element::new(Hover {
        base: base.into(),
        top: top.into(),
        rest,
        placement,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_controls::accessibility_probe::{Probe, headless_renderer};
    use iced::advanced::layout::{Layout, Limits};
    use iced::advanced::mouse;
    use iced::advanced::widget::Tree;
    use iced::{Point, Size, Vector};

    #[test]
    fn overlaid_actions_keep_their_message_scroll_offset_and_accessibility() {
        let mut element: Element<'_, (), cosmic::Theme, cosmic::Renderer> =
            over(Element::new(Probe), iced::widget::Space::new());
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &headless_renderer(),
            &Limits::new(Size::ZERO, Size::new(80.0, 40.0)),
        );
        // Laid out as iced's hover: the actions share the message's bounds.
        assert_eq!(node.size(), Size::new(80.0, 40.0));
        let node = node.move_to(Point::new(0.0, 100.0));
        let accessible = element.as_widget().a11y_nodes(
            Layout::new(&node).with_virtual_offset(Vector::new(0.0, 37.0)),
            &tree,
            mouse::Cursor::Unavailable,
        );
        let message = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .find(|node| node.node().label() == Some("Retained child"))
            .expect("the message stays in the accessibility tree");
        // The probe reports window space, so it saw the scrollable's offset.
        assert_eq!(message.node().bounds().map(|bounds| bounds.y0), Some(63.0));
    }
    type Test<'a> = Element<'a, (), cosmic::Theme, cosmic::Renderer>;

    fn fixed<'a>(width: f32, height: f32) -> Test<'a> {
        iced::widget::Space::new()
            .width(Length::Fixed(width))
            .height(Length::Fixed(height))
            .into()
    }

    /// A control that takes focus, filling its width as a row's header does.
    fn control<'a>(width: Length, height: f32) -> Test<'a> {
        cosmic::widget::button::custom(fixed(10.0, height))
            .width(width)
            .on_press(())
            .into()
    }

    /// Lays out an open row, `header` tall, with `actions` beside it and a
    /// detail below, in a pane `width` wide: the header's, the actions' and
    /// the detail's bounds.
    fn open_row(width: f32, actions: Size) -> (Test<'static>, Tree, iced::advanced::layout::Node) {
        let mut element = beside_first(
            control(Length::Fill, 32.0),
            control(Length::Fixed(actions.width), actions.height),
            iced::widget::container(control(Length::Fill, 200.0)).width(Length::Fill),
            4.0,
        );
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &headless_renderer(),
            &Limits::new(Size::ZERO, Size::new(width, 1000.0)),
        );
        (element, tree, node)
    }

    fn parts(node: &iced::advanced::layout::Node) -> [iced::Rectangle; 3] {
        let mut children = Layout::new(node).children().map(|child| child.bounds());
        let header = children.next().unwrap();
        let actions = children.next().unwrap();
        let detail = children.next().unwrap();
        [header, actions, detail]
    }

    /// An open row's actions sit within its header's row, centred on a
    /// one-line header, and its detail ends where the header does rather than
    /// running under them.
    #[test]
    fn actions_beside_an_open_rows_header_keep_its_detail_to_the_header() {
        for width in [320.0, 520.0, 900.0] {
            let (_, _, node) = open_row(width, Size::new(24.0, 20.0));
            let [header, actions, detail] = parts(&node);
            assert_eq!(node.size().width, width, "{width}");
            assert_eq!(header.y, 0.0, "{width}");
            assert!(
                actions.y >= header.y && actions.y + actions.height <= header.y + header.height,
                "{width}: the actions stay within the header's row"
            );
            assert_eq!(
                actions.y + actions.height / 2.0,
                header.y + header.height / 2.0,
                "{width}"
            );
            assert_eq!(actions.x + actions.width, width, "{width}");
            assert_eq!(header.x + header.width, actions.x - 4.0, "{width}");
            assert_eq!(detail.x + detail.width, header.x + header.width, "{width}");
            assert_eq!(detail.y, header.y + header.height + 4.0, "{width}");
            // Only the header's band reveals the actions.
            let band = hover_bounds(Placement::BesideFirst(4.0), Layout::new(&node));
            assert_eq!(band.height, header.height, "{width}");
        }
    }

    /// A header whose name is followed by a preview of its output keeps its
    /// actions on the name's line, open or closed, rather than centred
    /// between the name and the preview, so opening a row does not move them.
    /// A command that wraps onto a second line, one element however many
    /// lines it takes, keeps them on its first line as well.
    #[test]
    fn actions_sit_on_a_two_line_headers_first_line() {
        let line = crate::design::LINE_BODY;
        // A row's header: its glyph beside a name `lines` tall over a taller
        // preview.
        let header = |lines: f32| -> Test<'static> {
            cosmic::widget::button::custom(
                iced::widget::row![
                    fixed(14.0, 14.0),
                    iced::widget::column![fixed(100.0, line * lines), fixed(100.0, 40.0)]
                        .spacing(4.0),
                ]
                .spacing(8.0),
            )
            .padding(4)
            .width(Length::Fill)
            .on_press(())
            .into()
        };
        let lay_out = |mut element: Test<'static>| {
            let mut tree = Tree::new(element.as_widget());
            element.as_widget_mut().layout(
                &mut tree,
                &headless_renderer(),
                &Limits::new(Size::ZERO, Size::new(520.0, 1000.0)),
            )
        };
        // The name's first line spans 4..4 + line within the header's padding.
        let on_first_line = 4.0 + (line - 20.0) / 2.0;
        for lines in [1.0, 2.0] {
            let open = lay_out(beside_first(
                header(lines),
                fixed(24.0, 20.0),
                fixed(100.0, 50.0),
                4.0,
            ));
            let [header_bounds, actions, _] = parts(&open);
            assert_eq!(
                header_bounds.height,
                4.0 + line * lines + 4.0 + 40.0 + 4.0,
                "{lines}"
            );
            assert_eq!(actions.y, on_first_line, "{lines}");
            let closed = lay_out(view(header(lines), fixed(24.0, 20.0), 4.0));
            let mut children = Layout::new(&closed).children().map(|child| child.bounds());
            let (_, closed_actions) = (children.next().unwrap(), children.next().unwrap());
            assert_eq!(closed_actions.y, actions.y, "{lines}");
        }
    }

    /// Where the actions are too wide to share a narrow row, they move under
    /// the header, before the detail rather than after all of it, and the
    /// header and detail keep the row's whole width.
    #[test]
    fn stacked_actions_on_an_open_row_come_between_its_header_and_detail() {
        let (_, _, node) = open_row(320.0, Size::new(200.0, 20.0));
        let [header, actions, detail] = parts(&node);
        assert_eq!(header.width, 320.0);
        assert_eq!(actions.y, header.y + header.height + 4.0);
        assert_eq!(actions.x + actions.width, 320.0);
        assert_eq!(detail.y, actions.y + actions.height + 4.0);
        assert_eq!(detail.x + detail.width, header.x + header.width);
        assert_eq!(node.size().height, detail.y + detail.height);
        // The pointer can move from the header down onto the actions.
        let band = hover_bounds(Placement::BesideFirst(4.0), Layout::new(&node));
        assert_eq!(band.y + band.height, actions.y + actions.height);
    }

    /// Focus moves through an open row as it is read: its header, the
    /// header's actions, then the detail.
    #[test]
    fn focus_moves_from_an_open_rows_header_to_its_actions_then_its_detail() {
        struct Order(Vec<iced::Rectangle>);
        impl operation::Operation for Order {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn operation::Operation)) {
                operate(self);
            }
            fn focusable(
                &mut self,
                _id: Option<&iced::advanced::widget::Id>,
                bounds: iced::Rectangle,
                _state: &mut dyn operation::Focusable,
            ) {
                self.0.push(bounds);
            }
        }
        for actions in [Size::new(24.0, 20.0), Size::new(200.0, 20.0)] {
            let (mut element, mut tree, node) = open_row(320.0, actions);
            let mut order = Order(Vec::new());
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &headless_renderer(),
                &mut order,
            );
            assert_eq!(order.0, parts(&node).to_vec(), "{actions:?}");
        }
    }
}
