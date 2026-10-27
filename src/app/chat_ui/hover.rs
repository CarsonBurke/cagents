//! Chat hover actions preserving the semantic subtree and targeted actions.
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
    with_layout(base, top, Arrangement::Reserved(spacing))
}

/// Message actions paint over an already reserved footer instead of taking
/// width from the prose. They keep the same native focus and AT dispatch as
/// the tool row's reserved action layout.
pub(super) fn overlay<'a, Message, Theme, Renderer>(
    base: impl Into<Element<'a, Message, Theme, Renderer>>,
    top: impl Into<Element<'a, Message, Theme, Renderer>>,
) -> Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: core::Renderer + 'a,
{
    with_layout(base, top, Arrangement::Overlay)
}

#[derive(Clone, Copy)]
enum Arrangement {
    Reserved(f32),
    Overlay,
}

fn with_layout<'a, Message, Theme, Renderer>(
    base: impl Into<Element<'a, Message, Theme, Renderer>>,
    top: impl Into<Element<'a, Message, Theme, Renderer>>,
    arrangement: Arrangement,
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
        arrangement: Arrangement,
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
            let Arrangement::Reserved(spacing) = self.arrangement else {
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
        arrangement,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        Message,
        chat_ui::{Action, Target, selectable_prose},
    };
    use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, widget::Tree};
    use cosmic::iced::{Alignment, Event, Rectangle, Size, Vector, keyboard, mouse};
    use cosmic::widget;
    use iced_accessibility::{
        A11yId,
        accesskit::{Action as A11yAction, ActionRequest, Role, TreeId},
    };

    const REPORT: &str = "The completed report stays readable.";

    fn event(id: A11yId, action: A11yAction) -> Event {
        let A11yId::Widget(widget) = id.clone() else {
            panic!("native widget identity required")
        };
        Event::A11y(
            widget,
            ActionRequest {
                action,
                target_tree: TreeId::ROOT,
                target_node: id.into(),
                data: None,
            },
        )
    }

    fn enter() -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        })
    }

    #[test]
    fn message_overlay_exposes_translated_report_and_native_copy_without_hover() {
        let target = Target {
            card: crate::model::CardId::new(),
            generation: 0,
        };
        let mut element = overlay(
            widget::container(selectable_prose::plain(
                REPORT,
                14.0,
                iced::Font::default(),
                target,
            ))
            .width(300)
            .height(100),
            widget::container(
                crate::ui_controls::button::secondary("Copy report")
                    .on_press(Message::ChatUi(Action::CopyLiteral(REPORT.into()))),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .align_y(Alignment::End),
        );
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(500.0, 400.0)),
        );
        assert_eq!(node.size(), Size::new(300.0, 100.0));
        assert_eq!(node.children()[0].size(), node.size());
        assert_eq!(node.children()[1].size(), node.size());
        let native =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let report = native
            .root()
            .iter()
            .chain(native.children())
            .find(|node| node.node().role() == Role::Label && node.node().value() == Some(REPORT))
            .expect("the real selectable report is accessible without hover");
        let copy = native
            .root()
            .iter()
            .chain(native.children())
            .find(|node| {
                node.node().role() == Role::Button && node.node().label() == Some("Copy report")
            })
            .expect("the actual native Copy control is accessible without hover");
        let copy_id = copy.id().clone();
        let native_bounds = [
            report.node().bounds().unwrap(),
            copy.node().bounds().unwrap(),
        ];
        let translated = element.as_widget().a11y_nodes(
            Layout::with_offset(Vector::new(40.0, 80.0), &node)
                .with_virtual_offset(Vector::new(11.0, 23.0)),
            &tree,
            mouse::Cursor::Unavailable,
        );
        for (label, role, before) in [
            (REPORT, Role::Label, native_bounds[0]),
            ("Copy report", Role::Button, native_bounds[1]),
        ] {
            let after = translated
                .root()
                .iter()
                .chain(translated.children())
                .find(|node| {
                    node.node().role() == role
                        && (node.node().value() == Some(label)
                            || node.node().label() == Some(label))
                })
                .unwrap()
                .node()
                .bounds()
                .unwrap();
            assert!(
                before.x1 > before.x0 && before.y1 > before.y0,
                "{label}: {before:?}"
            );
            assert!(
                after.x1 > after.x0 && after.y1 > after.y0,
                "{label}: {after:?}"
            );
            // Layout translation, virtual-offset subtraction and edge sums
            // happen in f32 before AX widens them to f64. Widening first and
            // adding the exact delta can differ by a few f32 ULPs. Two relative
            // f32 epsilons stay below 0.0001 logical pixels in this fixture.
            for (edge, actual, origin, delta) in [
                ("x0", after.x0, before.x0, 29.0),
                ("x1", after.x1, before.x1, 29.0),
                ("y0", after.y0, before.y0, 57.0),
                ("y1", after.y1, before.y1, 57.0),
            ] {
                let expected = origin + delta;
                let tolerance = 2.0
                    * f64::from(f32::EPSILON)
                    * expected.abs().max(origin.abs()).max(delta.abs()).max(1.0);
                assert!(
                    (actual - expected).abs() <= tolerance,
                    "{label} {edge}: expected {expected}, actual {actual}, tolerance {tolerance}"
                );
            }
        }
        let viewport = Rectangle::with_size(Size::new(500.0, 400.0));
        let mut dispatch = |event: Event| {
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            messages
        };
        let copied = dispatch(event(copy_id.clone(), A11yAction::Click));
        assert!(
            matches!(copied.as_slice(), [Message::ChatUi(Action::CopyLiteral(text))] if text == REPORT)
        );
        assert!(dispatch(event(copy_id.clone(), A11yAction::Focus)).is_empty());
        drop(dispatch);
        // The native tree keeps AT focus across an ordinary element diff.
        tree.diff(element.as_widget_mut());
        let mut dispatch = |event: Event| {
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            messages
        };
        let copied = dispatch(enter());
        assert!(
            matches!(copied.as_slice(), [Message::ChatUi(Action::CopyLiteral(text))] if text == REPORT)
        );
        assert!(dispatch(event(copy_id, A11yAction::Blur)).is_empty());
        assert!(
            dispatch(enter()).is_empty(),
            "blurred hidden Copy cannot consume Enter"
        );
    }

    #[test]
    fn reserved_tool_actions_keep_their_side_by_side_and_narrow_layouts() {
        let renderer = crate::app::tests::headless_renderer();
        for (width, expected) in [
            (300.0, Size::new(300.0, 20.0)),
            (100.0, Size::new(100.0, 38.0)),
        ] {
            let mut element: cosmic::Element<'_, ()> = view(
                widget::Space::new().width(40).height(20),
                widget::Space::new().width(60).height(10),
                8.0,
            );
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(width, 400.0)),
            );
            assert_eq!(node.size(), expected);
            let base = node.children()[0].bounds();
            let actions = node.children()[1].bounds();
            if width == 300.0 {
                assert_eq!(base.x, 0.0);
                assert_eq!(actions.x, width - actions.width);
                assert_eq!(actions.y, 5.0);
            } else {
                assert_eq!(actions.y, base.height + 8.0);
                assert_eq!(actions.x, width - actions.width);
            }
        }
    }
}
