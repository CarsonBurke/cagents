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
    hover(base, top, Placement::Beside(spacing))
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
    hover(base, top, Placement::Over)
}

fn hover<'a, Message, Theme, Renderer>(
    base: impl Into<Element<'a, Message, Theme, Renderer>>,
    top: impl Into<Element<'a, Message, Theme, Renderer>>,
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
        placement: Placement,
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
            vec![Tree::new(&self.base), Tree::new(&self.top)]
        }

        fn diff(&mut self, tree: &mut Tree) {
            tree.diff_children(&mut [&mut self.base, &mut self.top]);
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
            let Placement::Beside(spacing) = self.placement else {
                let base = self.base.as_widget_mut().layout(
                    &mut tree.children[0],
                    renderer,
                    limits,
                );
                let top = self.top.as_widget_mut().layout(
                    &mut tree.children[1],
                    renderer,
                    &layout::Limits::new(Size::ZERO, base.size()),
                );
                return layout::Node::with_children(base.size(), vec![base, top]);
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
                let top_y = (size.height - top.size().height) / 2.0;
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

                if cursor.is_over(layout.bounds())
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
            let children = [&mut self.base, &mut self.top]
                .into_iter()
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
                let branch = [&self.base, &self.top]
                    .into_iter()
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
                        let child = if branch == 0 {
                            &mut self.base
                        } else {
                            &mut self.top
                        };
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
                        let child = if branch == 0 {
                            &mut self.base
                        } else {
                            &mut self.top
                        };
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

            let is_hovered = cursor.is_over(layout.bounds());

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
                [&self.base, &self.top]
                    .into_iter()
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
            [&self.base, &self.top]
                .into_iter()
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
            let mut overlays = [&mut self.base, &mut self.top]
                .into_iter()
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

            top_overlay
        }
    }

    Element::new(Hover {
        base: base.into(),
        top: top.into(),
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
}
