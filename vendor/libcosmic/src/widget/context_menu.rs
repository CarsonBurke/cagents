// Copyright 2024 System76 <info@system76.com>
// SPDX-License-Identifier: MPL-2.0

//! A context menu is a menu in a graphical user interface that appears upon user interaction, such as a right-click mouse operation.

#[cfg(wayland_platform)]
use crate::app::cosmic::{WINDOWING_SYSTEM, WindowingSystem};
use crate::widget::menu::{
    self, CloseCondition, Direction, ItemHeight, ItemWidth, MenuBarState, PathHighlight,
    init_root_menu, menu_roots_diff,
};
use derive_setters::Setters;
use iced::touch::Finger;
use iced::{Event, Vector, keyboard, window};
use iced_core::widget::{Tree, Widget, tree};
use iced_core::{Length, Point, Size, mouse, touch};
use std::collections::HashSet;
use std::sync::Arc;

/// A context menu is a menu in a graphical user interface that appears upon user interaction, such as a right-click mouse operation.
pub fn context_menu<'a, Message: 'static + Clone>(
    content: impl Into<crate::Element<'a, Message>>,
    // on_context: Message,
    context_menu: Option<Vec<menu::Tree<Message>>>,
) -> ContextMenu<'a, Message> {
    let mut this = ContextMenu {
        content: content.into(),
        context_menu: context_menu.map(|menus| {
            vec![menu::Tree::with_children(
                crate::Element::from(crate::widget::Row::new()),
                menus,
            )]
        }),
        close_on_escape: true,
        window_id: window::Id::RESERVED,
        on_surface_action: None,
    };

    if let Some(ref mut context_menu) = this.context_menu {
        context_menu.iter_mut().for_each(menu::Tree::set_index);
    }

    this
}

/// A context menu is a menu in a graphical user interface that appears upon user interaction, such as a right-click mouse operation.
#[derive(Setters)]
#[must_use]
pub struct ContextMenu<'a, Message> {
    #[setters(skip)]
    content: crate::Element<'a, Message>,
    #[setters(skip)]
    context_menu: Option<Vec<menu::Tree<Message>>>,
    pub window_id: window::Id,
    pub close_on_escape: bool,
    #[setters(skip)]
    pub(crate) on_surface_action:
        Option<Arc<dyn Fn(crate::surface::Action) -> Message + Send + Sync + 'static>>,
}

impl<Message: Clone + 'static> ContextMenu<'_, Message> {
    #[cfg(wayland_platform)]
    #[allow(clippy::too_many_lines)]
    fn create_popup(
        &mut self,
        layout: iced_core::Layout<'_>,
        view_cursor: iced_core::mouse::Cursor,
        renderer: &crate::Renderer,
        shell: &mut iced_core::Shell<'_, Message>,
        viewport: &iced::Rectangle,
        my_state: &mut LocalState,
    ) {
        if self.window_id != window::Id::NONE && self.on_surface_action.is_some() {
            use crate::surface::action::{LiveSettings, destroy_popup};
            use crate::theme::THEME;
            use crate::widget::menu::{Menu, StyleSheet as _};
            use iced_runtime::platform_specific::wayland::CornerRadius;
            use iced_runtime::platform_specific::wayland::popup::{
                SctkPopupSettings, SctkPositioner,
            };

            let mut bounds = layout.bounds();
            bounds.x = my_state.context_cursor.x;
            bounds.y = my_state.context_cursor.y;

            let (id, root_list) = my_state.menu_bar_state.inner.with_data_mut(|state| {
                if let Some(id) = state.popup_id.get(&self.window_id).copied() {
                    // close existing popups
                    state.menu_states.clear();
                    state.active_root.clear();

                    shell.publish(self.on_surface_action.as_ref().unwrap()(destroy_popup(id)));
                    state.view_cursor = view_cursor;
                    (
                        id,
                        layout.children().map(|lo| lo.bounds()).collect::<Vec<_>>(),
                    )
                } else {
                    (
                        window::Id::unique(),
                        layout.children().map(|lo| lo.bounds()).collect(),
                    )
                }
            });
            let Some(context_menu) = self.context_menu.as_mut() else {
                return;
            };

            let mut popup_menu: Menu<'static, _> = Menu {
                tree: my_state.menu_bar_state.clone(),
                menu_roots: std::borrow::Cow::Owned(context_menu.clone()),
                bounds_expand: 16,
                menu_overlays_parent: true,
                close_condition: CloseCondition {
                    leave: false,
                    click_outside: true,
                    click_inside: true,
                },
                item_width: ItemWidth::Uniform(240),
                item_height: ItemHeight::Dynamic(40),
                bar_bounds: bounds,
                main_offset: -(bounds.height as i32),
                cross_offset: 0,
                root_bounds_list: vec![bounds],
                path_highlight: Some(PathHighlight::MenuActive),
                style: std::borrow::Cow::Owned(crate::theme::menu_bar::MenuBarStyle::Default),
                position: Point::new(0., 0.),
                is_overlay: false,
                window_id: id,
                depth: 0,
                on_surface_action: self.on_surface_action.clone(),
            };

            init_root_menu(
                &mut popup_menu,
                renderer,
                shell,
                view_cursor.position().unwrap(),
                viewport.size(),
                Vector::new(0., 0.),
                layout.bounds(),
                -bounds.height,
            );
            let (anchor_rect, gravity) = my_state.menu_bar_state.inner.with_data_mut(|state| {
                use iced::Rectangle;

                state.popup_id.insert(self.window_id, id);
                ({
                    let pos = view_cursor.position().unwrap_or_default();
                    Rectangle {
                        x: pos.x as i32,
                        y: pos.y as i32,
                        width: 1,
                        height: 1,
                    }
                },
                match (state.horizontal_direction, state.vertical_direction) {
                    (Direction::Positive, Direction::Positive) => cctk::wayland_protocols::xdg::shell::client::xdg_positioner::Gravity::BottomRight,
                    (Direction::Positive, Direction::Negative) => cctk::wayland_protocols::xdg::shell::client::xdg_positioner::Gravity::TopRight,
                    (Direction::Negative, Direction::Positive) => cctk::wayland_protocols::xdg::shell::client::xdg_positioner::Gravity::BottomLeft,
                    (Direction::Negative, Direction::Negative) => cctk::wayland_protocols::xdg::shell::client::xdg_positioner::Gravity::TopLeft,
                })
            });

            let menu_node =
                popup_menu.layout(renderer, iced::Limits::NONE.min_width(1.).min_height(1.));
            let popup_size = menu_node.size();
            let positioner = SctkPositioner {
                size: Some((
                    popup_size.width.ceil() as u32 + 2,
                    popup_size.height.ceil() as u32 + 2,
                )),
                anchor_rect,
                anchor: cctk::wayland_protocols::xdg::shell::client::xdg_positioner::Anchor::None,
                gravity: cctk::wayland_protocols::xdg::shell::client::xdg_positioner::Gravity::BottomRight,
                reactive: true,
                ..Default::default()
            };
            let parent = self.window_id;
            let t = THEME.lock().unwrap();
            let styling = t.appearance(&crate::theme::menu_bar::MenuBarStyle::Default, false);
            drop(t);
            let rad = styling.menu_border_radius;

            shell.publish((self.on_surface_action.as_ref().unwrap())(
                crate::surface::action::simple_popup(
                    move || LiveSettings {
                        corners: Some(CornerRadius {
                            top_left: rad[0] as u32,
                            top_right: rad[1] as u32,
                            bottom_left: rad[2] as u32,
                            bottom_right: rad[3] as u32,
                        }),
                        ..Default::default()
                    },
                    move || SctkPopupSettings {
                        parent,
                        id,
                        positioner: positioner.clone(),
                        parent_size: None,
                        grab: true,
                        close_with_children: false,
                        input_zone: None,
                    },
                    Some(move || {
                        crate::Element::from(
                            crate::widget::container(popup_menu.clone()).center(Length::Fill),
                        )
                        .map(crate::action::app)
                    }),
                ),
            ));
        }
    }

    pub fn on_surface_action(
        mut self,
        handler: impl Fn(crate::surface::Action) -> Message + Send + Sync + 'static,
    ) -> Self {
        self.on_surface_action = Some(Arc::new(handler));
        self
    }
}

impl<Message: 'static + Clone> Widget<Message, crate::Theme, crate::Renderer>
    for ContextMenu<'_, Message>
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<LocalState>()
    }

    fn state(&self) -> tree::State {
        #[allow(clippy::default_trait_access)]
        tree::State::new(LocalState {
            context_cursor: Point::default(),
            fingers_pressed: Default::default(),
            menu_bar_state: Default::default(),
        })
    }

    fn children(&self) -> Vec<Tree> {
        let mut children = Vec::with_capacity(if self.context_menu.is_some() { 2 } else { 1 });

        children.push(Tree::new(self.content.as_widget()));

        // Assign the context menu's elements as this widget's children.
        if let Some(ref context_menu) = self.context_menu {
            let mut tree = Tree::empty();
            tree.children = context_menu
                .iter()
                .map(|root| {
                    let mut tree = Tree::empty();
                    let flat = root
                        .flattern()
                        .iter()
                        .map(|mt| Tree::new(mt.item.clone()))
                        .collect();
                    tree.children = flat;
                    tree
                })
                .collect();

            children.push(tree);
        }

        children
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
        let state = tree.state.downcast_mut::<LocalState>();
        state.menu_bar_state.inner.with_data_mut(|inner| {
            menu_roots_diff(self.context_menu.as_mut().unwrap(), &mut inner.tree);
        });
        #[cfg(feature = "a11y")]
        menu::Menu::<'_, Message>::clear_accessibility(&state.menu_bar_state);

        // if let Some(ref mut context_menus) = self.context_menu {
        //     for (menu, tree) in context_menus
        //         .iter_mut()
        //         .zip(tree.children[1].children.iter_mut())
        //     {
        //         menu.item.as_widget_mut().diff(tree);
        //     }
        // }
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &crate::Renderer,
        limits: &iced_core::layout::Limits,
    ) -> iced_core::layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut crate::Renderer,
        theme: &crate::Theme,
        style: &iced_core::renderer::Style,
        layout: iced_core::Layout<'_>,
        cursor: iced_core::mouse::Cursor,
        viewport: &iced::Rectangle,
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

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: iced_core::Layout<'_>,
        renderer: &crate::Renderer,
        operation: &mut dyn iced_core::widget::Operation<()>,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    #[allow(clippy::too_many_lines)]
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &iced::Event,
        layout: iced_core::Layout<'_>,
        cursor: iced_core::mouse::Cursor,
        renderer: &crate::Renderer,
        clipboard: &mut dyn iced_core::Clipboard,
        shell: &mut iced_core::Shell<'_, Message>,
        viewport: &iced::Rectangle,
    ) {
        let state = tree.state.downcast_mut::<LocalState>();
        let bounds = layout.bounds();

        // XXX this should reset the state if there are no other copies of the state, which implies no dropdown menus open.
        let reset = self.window_id != window::Id::NONE
            && state
                .menu_bar_state
                .inner
                .with_data(|d| !d.open && !d.active_root.is_empty());

        let open = state.menu_bar_state.inner.with_data_mut(|state| {
            if reset
                && let Some(popup_id) = state.popup_id.get(&self.window_id).copied()
                && let Some(handler) = self.on_surface_action.as_ref()
            {
                shell.publish((handler)(crate::surface::Action::DestroyPopup(popup_id)));
                state.reset();
            }
            state.open
        });
        let mut was_open = false;
        if matches!(event,
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            })
            | Event::Mouse(mouse::Event::ButtonPressed(
                mouse::Button::Right | mouse::Button::Left,
            ))
            | Event::Touch(touch::Event::FingerPressed { .. })
                if open )
        {
            state.menu_bar_state.inner.with_data_mut(|state| {
                was_open = true;
                state.menu_states.clear();
                state.active_root.clear();
                state.open = false;

                #[cfg(wayland_platform)]
                if matches!(WINDOWING_SYSTEM.get(), Some(WindowingSystem::Wayland))
                    && let Some(id) = state.popup_id.remove(&self.window_id)
                {
                    {
                        let surface_action = self.on_surface_action.as_ref().unwrap();
                        shell.publish(surface_action(crate::surface::action::destroy_popup(id)));
                    }
                    state.view_cursor = cursor;
                }
            });
        }

        if !was_open && cursor.is_over(bounds) {
            let fingers_pressed = state.fingers_pressed.len();

            match event {
                Event::Touch(touch::Event::FingerPressed { id, .. }) => {
                    state.fingers_pressed.insert(*id);
                }

                Event::Touch(touch::Event::FingerLifted { id, .. }) => {
                    state.fingers_pressed.remove(id);
                }

                _ => (),
            }

            // Present a context menu on a right click event.
            if !was_open
                && self.context_menu.is_some()
                && (right_button_released(event) || (touch_lifted(event) && fingers_pressed == 2))
            {
                state.context_cursor = cursor.position().unwrap_or_default();
                let state = tree.state.downcast_mut::<LocalState>();
                #[cfg(feature = "a11y")]
                if let Some(roots) = &mut self.context_menu {
                    for root in roots.iter() {
                        for item in root.flattern() {
                            item.item.element.with_data_mut(|element| {
                                if element.as_widget().id().is_some() {
                                    element
                                        .as_widget_mut()
                                        .set_id(iced_core::widget::Id::unique());
                                }
                            });
                        }
                    }
                    state.menu_bar_state.inner.with_data_mut(|inner| {
                        inner.tree = Tree::empty();
                        menu_roots_diff(roots, &mut inner.tree);
                        inner.reset();
                    });
                }
                state.menu_bar_state.inner.with_data_mut(|state| {
                    state.open = true;
                    state.view_cursor = cursor;
                });
                #[cfg(wayland_platform)]
                if matches!(WINDOWING_SYSTEM.get(), Some(WindowingSystem::Wayland)) {
                    self.create_popup(layout, cursor, renderer, shell, viewport, state);
                }

                shell.capture_event();
                return;
            } else if !was_open && right_button_released(event)
                || (touch_lifted(event))
                || left_button_released(event)
            {
                state.menu_bar_state.inner.with_data_mut(|state| {
                    was_open = true;
                    state.menu_states.clear();
                    state.active_root.clear();
                    state.open = false;

                    #[cfg(wayland_platform)]
                    if matches!(WINDOWING_SYSTEM.get(), Some(WindowingSystem::Wayland))
                        && let Some(id) = state.popup_id.remove(&self.window_id)
                    {
                        {
                            let surface_action = self.on_surface_action.as_ref().unwrap();
                            shell
                                .publish(surface_action(crate::surface::action::destroy_popup(id)));
                        }
                        state.view_cursor = cursor;
                    }
                });
            }
        }
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

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: iced_core::Layout<'_>,
        _renderer: &crate::Renderer,
        _viewport: &iced::Rectangle,
        translation: Vector,
    ) -> Option<iced_core::overlay::Element<'b, Message, crate::Theme, crate::Renderer>> {
        #[cfg(wayland_platform)]
        if matches!(WINDOWING_SYSTEM.get(), Some(WindowingSystem::Wayland))
            && self.window_id != window::Id::NONE
            && self.on_surface_action.is_some()
        {
            return None;
        }

        let state = tree.state.downcast_ref::<LocalState>();

        let context_menu = self.context_menu.as_mut()?;

        if !state.menu_bar_state.inner.with_data(|state| state.open) {
            return None;
        }

        let mut bounds = layout.bounds();
        bounds.x = state.context_cursor.x;
        bounds.y = state.context_cursor.y;
        Some(
            crate::widget::menu::Menu {
                tree: state.menu_bar_state.clone(),
                menu_roots: std::borrow::Cow::Owned(context_menu.clone()),
                bounds_expand: 16,
                menu_overlays_parent: true,
                close_condition: CloseCondition {
                    leave: false,
                    click_outside: true,
                    click_inside: true,
                },
                item_width: ItemWidth::Uniform(240),
                item_height: ItemHeight::Dynamic(40),
                bar_bounds: bounds,
                main_offset: -(bounds.height as i32),
                cross_offset: 0,
                root_bounds_list: vec![bounds],
                path_highlight: Some(PathHighlight::MenuActive),
                style: std::borrow::Cow::Borrowed(&crate::theme::menu_bar::MenuBarStyle::Default),
                position: Point::new(translation.x, translation.y),
                is_overlay: true,
                window_id: window::Id::NONE,
                depth: 0,
                on_surface_action: None,
            }
            .overlay(),
        )
    }

    #[cfg(feature = "a11y")]
    /// get the a11y nodes for the widget
    fn a11y_nodes(
        &self,
        layout: iced_core::Layout<'_>,
        state: &Tree,
        p: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        let content = self
            .content
            .as_widget()
            .a11y_nodes(layout, &state.children[0], p);
        // A separate native popup publishes its own Menu Widget tree. Avoid
        // exporting its surface-local coordinates into the parent window.
        #[cfg(wayland_platform)]
        if matches!(WINDOWING_SYSTEM.get(), Some(WindowingSystem::Wayland))
            && self.window_id != window::Id::NONE
            && self.on_surface_action.is_some()
        {
            return content;
        }
        let local = state.state.downcast_ref::<LocalState>();
        let menu = self
            .context_menu
            .as_ref()
            .map(|roots| {
                menu::Menu::accessibility_nodes_for(
                    roots,
                    &local.menu_bar_state,
                    window::Id::NONE,
                    p,
                    Vector::ZERO,
                )
            })
            .unwrap_or_default();
        iced_accessibility::A11yTree::join([content, menu].into_iter())
    }
}

impl<'a, Message: Clone + 'static> From<ContextMenu<'a, Message>> for crate::Element<'a, Message> {
    fn from(widget: ContextMenu<'a, Message>) -> Self {
        Self::new(widget)
    }
}

fn right_button_released(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right,))
    )
}

fn left_button_released(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left,))
    )
}

fn touch_lifted(event: &Event) -> bool {
    matches!(event, Event::Touch(touch::Event::FingerLifted { .. }))
}

pub struct LocalState {
    context_cursor: Point,
    fingers_pressed: HashSet<Finger>,
    menu_bar_state: MenuBarState,
}

impl Drop for LocalState {
    fn drop(&mut self) {
        // A native popup holds an Rc clone of this menu state. Replacing its
        // owning widget must retire its targets too, even while that popup's
        // surface is awaiting ordinary platform retirement.
        self.menu_bar_state
            .inner
            .with_data_mut(|state| state.reset());
    }
}

#[cfg(all(test, feature = "a11y", feature = "tokio"))]
mod accessibility_tests {
    use super::*;
    use iced::advanced::{clipboard, renderer::Headless};
    use iced_accessibility::accesskit::{Action, ActionRequest, NodeId, Role, TreeId};
    use iced_core::{Layout, Rectangle, Shell, layout::Limits};
    use std::borrow::Cow;

    #[test]
    fn popup_widget_uses_actual_parent_layout_and_retires_with_its_owner() {
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(crate::Renderer::new(
                iced::Font::default(),
                iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .expect("headless native renderer");
        for retire_owner in [false, true] {
            let mut owner = context_menu(
                iced::widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fill),
                Some(vec![menu::Tree::new(crate::Element::from(
                    crate::widget::button::custom(crate::widget::text("Copy branch"))
                        .name("Copy branch")
                        .on_press(7usize),
                ))]),
            );
            let mut owner_tree =
                Tree::new(&owner as &dyn Widget<usize, crate::Theme, crate::Renderer>);
            owner.diff(&mut owner_tree);
            let owner_node = owner.layout(
                &mut owner_tree,
                &renderer,
                &Limits::new(Size::new(240.0, 40.0), Size::new(240.0, 40.0)),
            );
            let mut messages = Vec::new();
            owner.update(
                &mut owner_tree,
                &iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
                Layout::new(&owner_node),
                mouse::Cursor::Available(Point::new(0.25, 0.25)),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &Rectangle::with_size(Size::new(900.0, 650.0)),
            );
            assert!(messages.is_empty());
            let shared = owner_tree
                .state
                .downcast_ref::<LocalState>()
                .menu_bar_state
                .clone();
            let bounds = Rectangle::new(Point::ORIGIN, Size::new(240.0, 0.5));
            // This is the actual owned Menu Widget used by the popup factory,
            // retaining the SAME Rc state as its parent ContextMenu owner.
            let mut popup = menu::Menu {
                tree: shared,
                menu_roots: Cow::Owned(owner.context_menu.as_ref().unwrap().clone()),
                bounds_expand: 16,
                menu_overlays_parent: true,
                close_condition: CloseCondition {
                    leave: false,
                    click_outside: true,
                    click_inside: true,
                },
                item_width: ItemWidth::Uniform(240),
                item_height: ItemHeight::Dynamic(40),
                bar_bounds: bounds,
                main_offset: 0,
                cross_offset: 0,
                root_bounds_list: vec![bounds],
                path_highlight: Some(PathHighlight::MenuActive),
                style: Cow::Owned(crate::theme::menu_bar::MenuBarStyle::Default),
                position: Point::ORIGIN,
                is_overlay: false,
                window_id: window::Id::unique(),
                depth: 0,
                on_surface_action: None,
            };
            init_root_menu(
                &mut popup,
                &renderer,
                &mut Shell::new(&mut messages),
                Point::new(0.25, 0.25),
                Size::new(900.0, 650.0),
                Vector::ZERO,
                bounds,
                0.0,
            );
            let mut popup_tree = Tree::empty();
            let node = Widget::layout(
                &mut popup,
                &mut popup_tree,
                &renderer,
                &Limits::new(Size::new(240.0, 40.0), Size::new(240.0, 40.0)),
            );
            // An ordinary popup container may move the actual Widget node
            // after layout. AX and dispatch must use that passed layout.
            let shifted = node.move_to(Point::new(57.0, 83.0));
            let layout = Layout::new(&shifted);
            let nodes = Widget::a11y_nodes(&popup, layout, &popup_tree, mouse::Cursor::Unavailable);
            let button = nodes
                .root()
                .iter()
                .find(|node| node.node().label() == Some("Copy branch"))
                .unwrap();
            assert_eq!(button.node().role(), Role::Button);
            assert!(button.node().supports_action(Action::Click));
            assert!(!button.node().supports_action(Action::Focus));
            let expected = layout
                .children()
                .next()
                .unwrap()
                .children()
                .next()
                .unwrap()
                .bounds();
            let actual = button.node().bounds().unwrap();
            assert_eq!(actual.x0, f64::from(expected.x));
            assert_eq!(actual.y0, f64::from(expected.y));
            let target: NodeId = button.id().clone().into();
            let mut kept_owner = Some(owner_tree);
            if retire_owner {
                drop(kept_owner.take());
                assert!(
                    Widget::a11y_nodes(&popup, layout, &popup_tree, mouse::Cursor::Unavailable)
                        .root()
                        .is_empty()
                );
            }
            Widget::update(
                &mut popup,
                &mut popup_tree,
                &iced::Event::A11y(
                    iced_core::widget::Id::from(target.0),
                    ActionRequest {
                        action: Action::Click,
                        target_tree: TreeId::ROOT,
                        target_node: target,
                        data: None,
                    },
                ),
                layout,
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &Rectangle::new(Point::new(57.0, 83.0), Size::new(240.0, 40.0)),
            );
            assert_eq!(messages, if retire_owner { vec![] } else { vec![7] });
            assert!(
                Widget::a11y_nodes(&popup, layout, &popup_tree, mouse::Cursor::Unavailable)
                    .root()
                    .is_empty()
            );
        }
    }
}
