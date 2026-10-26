//! Keep an explicitly selected or moving focused tab inside its native viewport.
//! Manual scroll changes are not reveal triggers, even while a tab has focus.
use crate::app::Message;
use cosmic::iced::{
    self, Event, Length, Rectangle, Size, Vector,
    advanced::{
        Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
        widget::{Id, Operation, Tree, operation, tree},
    },
    mouse,
};
use cosmic::{Element, Renderer, Theme};

pub(super) fn view<'a>(
    content: impl Into<Element<'a, Message>>,
    scroll: Id,
    selected: Option<Id>,
) -> Element<'a, Message> {
    Element::new(Reveal {
        content: content.into(),
        scroll,
        selected,
    })
}

struct Reveal<'a> {
    content: Element<'a, Message>,
    scroll: Id,
    selected: Option<Id>,
}

#[derive(Default)]
struct Memory {
    selected: Option<Id>,
    focused: Option<(Id, f32, f32)>,
    width: Option<f32>,
}

#[derive(Clone, Copy)]
struct Scroll {
    viewport: Rectangle,
    content: Rectangle,
    offset: f32,
}

struct Measure<'a> {
    scroll_id: &'a Id,
    selected_id: Option<&'a Id>,
    scroll: Option<Scroll>,
    selected: Option<Rectangle>,
    focused: Option<(Id, Rectangle)>,
}
impl Operation for Measure<'_> {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        viewport: Rectangle,
        content: Rectangle,
        translation: Vector,
        _: &mut dyn operation::Scrollable,
    ) {
        if id == Some(self.scroll_id) {
            self.scroll = Some(Scroll {
                viewport,
                content,
                offset: translation.x,
            });
        }
    }
    fn focusable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        if id.is_some() && id == self.selected_id {
            self.selected = Some(bounds);
        }
        if state.is_focused()
            && let Some(id) = id
        {
            self.focused = Some((id.clone(), bounds));
        }
    }
}

struct Offset<'a>(&'a Id, f32);
impl Operation for Offset<'_> {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        _: Rectangle,
        _: Rectangle,
        _: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        if id == Some(self.0) {
            state.scroll_to(operation::scrollable::AbsoluteOffset {
                x: Some(self.1),
                y: None,
            });
        }
    }
}

impl Reveal<'_> {
    fn reconcile(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer) {
        let mut measure = Measure {
            scroll_id: &self.scroll,
            selected_id: self.selected.as_ref(),
            scroll: None,
            selected: None,
            focused: None,
        };
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, &mut measure);
        let Some(scroll) = measure.scroll else {
            return;
        };
        let focused = measure.focused.map(|(id, bounds)| {
            let position = (id, bounds.x - scroll.content.x, bounds.width);
            (position, bounds)
        });
        let memory = tree.state.downcast_mut::<Memory>();
        let target = if self.selected != memory.selected {
            measure.selected
        } else if focused.as_ref().is_some_and(|(position, _)| {
            Some(position) != memory.focused.as_ref() || memory.width != Some(scroll.viewport.width)
        }) {
            focused.as_ref().map(|(_, bounds)| *bounds)
        } else {
            None
        };
        memory.selected.clone_from(&self.selected);
        memory.focused = focused.map(|(position, _)| position);
        memory.width = Some(scroll.viewport.width);
        if let Some(bounds) = target {
            // Focusable bounds use unscrolled content coordinates. Native
            // translation, not text length or tab index, supplies the offset.
            let left = bounds.x - scroll.content.x;
            let right = left + bounds.width;
            let offset = if left < scroll.offset {
                left
            } else if right > scroll.offset + scroll.viewport.width {
                // An oversized tab reveals its start rather than hiding its
                // name just to fit the trailing edge of an impossible width.
                left.min(right - scroll.viewport.width)
            } else {
                scroll.offset
            }
            .clamp(0.0, (scroll.content.width - scroll.viewport.width).max(0.0));
            if (offset - scroll.offset).abs() > f32::EPSILON {
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut Offset(&self.scroll, offset),
                );
            }
        }
    }
}

impl Widget<Message, Theme, Renderer> for Reveal<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Memory>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(Memory::default())
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
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let node = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        self.reconcile(tree, Layout::new(&node), renderer);
        node
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
        self.reconcile(tree, layout, renderer);
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
        self.reconcile(tree, layout, renderer);
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
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
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
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
        self.content
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor)
    }
    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            layout,
            renderer,
            destinations,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        Cagents,
        chat_ui::{self, Action, Runtime, Target, card_control_id},
    };
    use cosmic::iced::{
        advanced::clipboard,
        keyboard,
        runtime::{UserInterface, user_interface::Cache},
    };
    use std::sync::Arc;

    fn runtime() -> (Runtime, Arc<crate::chat::client::ClientHandle>) {
        let (mut runtime, handle) = chat_ui::view::prepared_runtime_for_test(&[]);
        Arc::make_mut(&mut runtime.snapshot).subagents = (0..1000)
            .map(|index| crate::chat::Subagent {
                id: format!("child-{index}"),
                kind: format!("Agent {index}"),
                description: String::new(),
                status: if index == 999 {
                    crate::chat::ToolStatus::Succeeded
                } else {
                    crate::chat::ToolStatus::Running
                },
                waiting: false,
                model: None,
                detail: String::new(),
                background: true,
                items: Vec::new(),
            })
            .collect();
        (runtime, handle)
    }

    fn measure(
        ui: &mut UserInterface<'_, Message, Theme, Renderer>,
        renderer: &Renderer,
        target: Target,
    ) -> Scroll {
        let mut measure = Measure {
            scroll_id: &card_control_id(target.card, "chat-subagent-tabs"),
            selected_id: None,
            scroll: None,
            selected: None,
            focused: None,
        };
        ui.operate(renderer, &mut measure);
        measure.scroll.expect("actual horizontal viewport")
    }

    fn assert_visible(ui: &UserInterface<'_, Message, Theme, Renderer>, scroll: Scroll, id: &Id) {
        let accessible = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let bounds = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .find(|node| {
                node.id() == &iced_accessibility::A11yId::Widget(id.clone())
                    && node.node().role() == iced_accessibility::accesskit::Role::Button
            })
            .expect("the exact native tab is accessible")
            .node()
            .bounds()
            .expect("native tab bounds");
        assert!(
            bounds.x0 >= f64::from(scroll.viewport.x) - 0.5
                && bounds.x1 <= f64::from(scroll.viewport.x + scroll.viewport.width) + 0.5,
            "native tab {bounds:?} is outside viewport {:?}",
            scroll.viewport
        );
    }

    fn chat(app: &Cagents, target: Target) -> &Runtime {
        match app.cards.get(&target.card).unwrap() {
            crate::app::CardRuntime::Chat(runtime) => runtime,
            _ => panic!("native chat"),
        }
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
    fn chooser_selection_reveals_exact_tail_tab_without_resetting_manual_scroll() {
        let (mut runtime, _handle) = runtime();
        let mut app = crate::app::tests::context_inspector_test_app(true);
        runtime.target.card = app.workspace.focused().unwrap();
        let target = runtime.target;
        app.card_state_mut(target.card).unwrap().kind =
            crate::model::CardKind::Chat(runtime.session.clone());
        app.cards.insert(
            target.card,
            crate::app::CardRuntime::Chat(Box::new(runtime)),
        );
        let mut renderer = crate::app::tests::headless_renderer();
        let viewport = Size::new(360.0, 100.0);
        let mut ui = UserInterface::build(
            chat(&app, target).child_navigation(360.0),
            viewport,
            Cache::new(),
            &mut renderer,
        );
        assert_eq!(measure(&mut ui, &renderer, target).offset, 0.0);
        let cache = ui.into_cache();
        drop(app.handle_chat_ui(Action::SubagentPicker(target, true)));
        drop(app.handle_chat_ui(Action::SubagentQuery(target, "999".into())));
        let mut messages = Vec::new();
        {
            let mut chooser = UserInterface::build(
                chat(&app, target)
                    .child_picker_view()
                    .expect("open chooser"),
                Size::new(360.0, 600.0),
                Cache::new(),
                &mut renderer,
            );
            chooser.operate(
                &renderer,
                &mut operation::focusable::focus::<()>(card_control_id(
                    target.card,
                    "chat-subagent-search",
                )),
            );
            chooser.update(
                &[enter()],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
        }
        assert!(messages.iter().any(|message| matches!(message,
            Message::ChatUi(Action::SelectSubagent(actual, Some(id))) if *actual == target && id == "child-999")));
        for message in messages {
            drop(app.update(message));
        }
        assert!(chat(&app, target).child_picker.is_none());
        assert_eq!(
            chat(&app, target).selected_subagent.as_deref(),
            Some("child-999")
        );
        let mut ui = UserInterface::build(
            chat(&app, target).child_navigation(360.0),
            viewport,
            cache,
            &mut renderer,
        );
        let scroll = measure(&mut ui, &renderer, target);
        assert!(scroll.offset > 0.0);
        assert_visible(
            &ui,
            scroll,
            &card_control_id(target.card, "chat-subagent-tab-child-999"),
        );
        ui.operate(
            &renderer,
            &mut Offset(&card_control_id(target.card, "chat-subagent-tabs"), 0.0),
        );
        assert_eq!(
            measure(&mut ui, &renderer, target).offset,
            0.0,
            "manual browsing must not snap back to the selected tail"
        );
        let mut ui = UserInterface::build(
            chat(&app, target).child_navigation(360.0),
            viewport,
            ui.into_cache(),
            &mut renderer,
        );
        assert_eq!(
            measure(&mut ui, &renderer, target).offset,
            0.0,
            "an unchanged selected identity must preserve manual scroll across rebuild"
        );
    }

    #[test]
    fn focused_status_reorder_reveals_live_tab_and_preserves_exact_activation() {
        let (mut runtime, _handle) = runtime();
        let target = runtime.target;
        let focused = card_control_id(target.card, "chat-subagent-tab-child-20");
        let scroll_id = card_control_id(target.card, "chat-subagent-tabs");
        let mut renderer = crate::app::tests::headless_renderer();
        let viewport = Size::new(360.0, 100.0);
        let mut ui = UserInterface::build(
            runtime.child_navigation(360.0),
            viewport,
            Cache::new(),
            &mut renderer,
        );
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(focused.clone()),
        );
        assert_eq!(
            runtime.child_tab_focus.borrow().as_deref(),
            Some("child-20")
        );
        let before = measure(&mut ui, &renderer, target);
        assert!(before.offset > 0.0);
        assert_visible(&ui, before, &focused);
        ui.operate(&renderer, &mut Offset(&scroll_id, 0.0));
        assert_eq!(
            measure(&mut ui, &renderer, target).offset,
            0.0,
            "manual scrolling alone must not reveal an unchanged focused tab"
        );
        let mut ui = UserInterface::build(
            runtime.child_navigation(360.0),
            viewport,
            ui.into_cache(),
            &mut renderer,
        );
        assert_eq!(
            measure(&mut ui, &renderer, target).offset,
            0.0,
            "manual scroll survives unchanged focused identity and geometry on rebuild"
        );
        let cache = ui.into_cache();
        Arc::make_mut(&mut runtime.snapshot).subagents[20].status =
            crate::chat::ToolStatus::Succeeded;
        let mut ui = UserInterface::build(
            runtime.child_navigation(360.0),
            viewport,
            cache,
            &mut renderer,
        );
        let after = measure(&mut ui, &renderer, target);
        assert!(
            after.offset > before.offset,
            "the focused tab moved past the working prefix"
        );
        assert_visible(&ui, after, &focused);
        assert_eq!(
            runtime.child_tab_focus.borrow().as_deref(),
            Some("child-20")
        );
        let mut native_focus = Measure {
            scroll_id: &scroll_id,
            selected_id: None,
            scroll: None,
            selected: None,
            focused: None,
        };
        ui.operate(&renderer, &mut native_focus);
        assert_eq!(native_focus.focused.map(|(id, _)| id), Some(focused));
        let mut messages = Vec::new();
        ui.update(
            &[enter()],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert!(messages.iter().any(|message| matches!(message,
            Message::ChatUi(Action::SelectSubagent(actual, Some(id))) if *actual == target && id == "child-20")));
        assert!(
            runtime.child_cache.is_empty(),
            "revealing tabs must not prepare hidden transcripts"
        );
    }
}
