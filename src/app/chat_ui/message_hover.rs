// Copyright 2019 Héctor Ramón, Iced contributors
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
// FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
// COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
// IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
// CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! Message footers retain native hover behavior and expose their real controls.
//!
//! Based on iced_widget::helpers::hover (MIT, iced contributors). The pinned
//! implementation omits a11y_nodes, so use this local forwarding correction.
use core::{Widget, overlay, widget::operation};
use cosmic::iced::{Element, Length, advanced as core, window};

pub(super) fn hover<'a, Message, Theme, Renderer>(
    base: impl Into<Element<'a, Message, Theme, Renderer>>,
    top: impl Into<Element<'a, Message, Theme, Renderer>>,
) -> Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: core::Renderer + 'a,
{
    use cosmic::iced::advanced::{
        Shell,
        layout::{self, Layout},
        mouse, renderer,
        widget::tree::{self, Tree},
    };
    use cosmic::iced::{Event, Rectangle, Size};

    struct Hover<'a, Message, Theme, Renderer> {
        base: Element<'a, Message, Theme, Renderer>,
        top: Element<'a, Message, Theme, Renderer>,
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
            struct Tag;
            tree::Tag::of::<Tag>()
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
            let base = self
                .base
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, limits);

            let top = self.top.as_widget_mut().layout(
                &mut tree.children[1],
                renderer,
                &layout::Limits::new(Size::ZERO, base.size()),
            );

            layout::Node::with_children(base.size(), vec![base, top])
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
                let mut children = layout.children().zip(&tree.children);

                let (base_layout, base_tree) = children.next().unwrap();

                self.base.as_widget().draw(
                    base_tree,
                    renderer,
                    theme,
                    style,
                    base_layout.with_virtual_offset(layout.virtual_offset()),
                    cursor,
                    viewport,
                );

                if cursor.is_over(layout.bounds())
                    || self.is_top_focused
                    || self.is_top_overlay_active
                {
                    let (top_layout, top_tree) = children.next().unwrap();

                    renderer.with_layer(bounds, |renderer| {
                        self.top.as_widget().draw(
                            top_tree,
                            renderer,
                            theme,
                            style,
                            top_layout.with_virtual_offset(layout.virtual_offset()),
                            cursor,
                            viewport,
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
            let children = [&mut self.base, &mut self.top].into_iter().zip(
                layout
                    .children()
                    .map(|node| node.with_virtual_offset(layout.virtual_offset()))
                    .zip(&mut tree.children),
            );

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
            let mut children = layout.children().zip(&mut tree.children);
            let (base_layout, base_tree) = children.next().unwrap();
            let (top_layout, top_tree) = children.next().unwrap();

            let is_hovered = cursor.is_over(layout.bounds());

            if matches!(event, Event::Window(window::Event::RedrawRequested(_))) {
                let mut count_focused = operation::focusable::count();

                self.top.as_widget_mut().operate(
                    top_tree,
                    top_layout.with_virtual_offset(layout.virtual_offset()),
                    renderer,
                    &mut operation::black_box(&mut count_focused),
                );

                self.is_top_focused = match count_focused.finish() {
                    operation::Outcome::Some(count) => count.focused.is_some(),
                    _ => false,
                };

                self.is_hovered = is_hovered;
            } else if is_hovered != self.is_hovered {
                shell.request_redraw();
            }

            let is_visible = is_hovered || self.is_top_focused || self.is_top_overlay_active;

            if matches!(
                event,
                Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::ButtonReleased(_))
            ) || is_visible
            {
                let redraw_request = shell.redraw_request();

                self.top.as_widget_mut().update(
                    top_tree,
                    event,
                    top_layout.with_virtual_offset(layout.virtual_offset()),
                    cursor,
                    renderer,
                    clipboard,
                    shell,
                    viewport,
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
                base_layout.with_virtual_offset(layout.virtual_offset()),
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
            [&self.base, &self.top]
                .into_iter()
                .rev()
                .zip(
                    layout
                        .children()
                        .rev()
                        .map(|node| node.with_virtual_offset(layout.virtual_offset()))
                        .zip(tree.children.iter().rev()),
                )
                .map(|(child, (layout, tree))| {
                    child
                        .as_widget()
                        .mouse_interaction(tree, layout, cursor, viewport, renderer)
                })
                .find(|&interaction| interaction != mouse::Interaction::None)
                .unwrap_or_default()
        }

        fn drag_destinations(
            &self,
            tree: &core::widget::Tree,
            layout: core::Layout<'_>,
            renderer: &Renderer,
            destinations: &mut core::clipboard::DndDestinationRectangles,
        ) {
            for (child, (node, state)) in [&self.base, &self.top]
                .into_iter()
                .zip(layout.children().zip(&tree.children))
            {
                child.as_widget().drag_destinations(
                    state,
                    node.with_virtual_offset(layout.virtual_offset()),
                    renderer,
                    destinations,
                );
            }
        }

        fn a11y_nodes(
            &self,
            layout: core::Layout<'_>,
            tree: &core::widget::Tree,
            cursor: core::mouse::Cursor,
        ) -> iced_accessibility::A11yTree {
            // Focusable actions remain in the accessibility tree while the
            // pointer is elsewhere, just as operate() keeps them focusable.
            iced_accessibility::A11yTree::join(
                [&self.base, &self.top]
                    .into_iter()
                    .zip(layout.children().zip(&tree.children))
                    .map(|(child, (node, state))| {
                        child.as_widget().a11y_nodes(
                            node.with_virtual_offset(layout.virtual_offset()),
                            state,
                            cursor,
                        )
                    }),
            )
        }

        fn overlay<'b>(
            &'b mut self,
            tree: &'b mut core::widget::Tree,
            layout: core::Layout<'b>,
            renderer: &Renderer,
            viewport: &Rectangle,
            translation: cosmic::iced::Vector,
        ) -> Option<core::overlay::Element<'b, Message, Theme, Renderer>> {
            let mut overlays = [&mut self.base, &mut self.top]
                .into_iter()
                .zip(
                    layout
                        .children()
                        .map(|node| node.with_virtual_offset(layout.virtual_offset()))
                        .zip(tree.children.iter_mut()),
                )
                .map(|(child, (layout, tree))| {
                    child
                        .as_widget_mut()
                        .overlay(tree, layout, renderer, viewport, translation)
                });

            if let Some(base_overlay) = overlays.next()? {
                return Some(base_overlay);
            }

            let top_overlay = overlays.next()?;
            self.is_top_overlay_active = top_overlay.is_some();

            top_overlay
        }
    }

    Element::new(Hover {
        base: base.into(),
        top: top.into(),
        is_top_focused: false,
        is_top_overlay_active: false,
        is_hovered: false,
    })
}
