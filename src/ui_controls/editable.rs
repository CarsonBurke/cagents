//! Editable leaf semantics around the existing COSMIC input. Normal input,
//! shaping, IME, clipboard and context menus remain owned by that widget.
mod editor;
pub use editor::TextEditor;

use std::{borrow::Cow, cell::Cell, rc::Rc};

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Id, Operation, Tree, tree},
};
use cosmic::iced::{Event, Length, Padding, Pixels, Rectangle, Size, Vector};
use cosmic::{Element, Renderer, Theme, widget};
use iced_accessibility::{
    A11yId, A11yNode, A11yTree, IdEq,
    accesskit::{Action, ActionData, Node, NodeId, Rect, Role, TextPosition, TextSelection},
};
use unicode_segmentation::UnicodeSegmentation;

pub struct TextInput<'a, Message> {
    inner: widget::TextInput<'a, Message>,
    value: Cow<'a, str>,
    name: Cow<'a, str>,
    id: Option<Id>,
    secret: bool,
    private: bool,
    on_input: Option<Rc<dyn Fn(String) -> Message + 'a>>,
    on_focus: Option<Message>,
}

struct State {
    id: Id,
    explicit_id: Option<Id>,
    name: String,
    run_id: Id,
    last_focused: Cell<bool>,
}
impl State {
    fn new(id: Option<Id>, name: &str) -> Self {
        Self {
            id: id.clone().unwrap_or_else(Id::unique),
            explicit_id: id,
            name: name.into(),
            run_id: Id::unique(),
            last_focused: Cell::new(false),
        }
    }
    fn bind(&mut self, id: Option<Id>, name: &str) {
        let same_id = match (&self.explicit_id, &id) {
            (Some(a), Some(b)) => IdEq::eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same_id || (id.is_none() && self.name != name) {
            *self = Self::new(id, name);
        } else {
            self.name = name.into();
        }
    }
    fn run_node_id(&self) -> NodeId {
        A11yId::from(self.run_id.clone()).into()
    }
    fn owns(&self, id: &Id) -> bool {
        IdEq::eq(id, &self.id) || IdEq::eq(id, &self.run_id)
    }
}

impl<'a, Message: Clone + 'static> TextInput<'a, Message> {
    pub fn new(name: impl Into<Cow<'a, str>>, value: impl Into<Cow<'a, str>>) -> Self {
        let name = name.into();
        let value = value.into();
        Self {
            inner: widget::text_input(name.clone(), value.clone()),
            value,
            name,
            id: None,
            secret: false,
            private: false,
            on_input: None,
            on_focus: None,
        }
    }
    pub fn name(mut self, name: impl Into<Cow<'a, str>>) -> Self {
        self.name = name.into();
        self
    }
    pub fn id(mut self, id: Id) -> Self {
        self.inner = self.inner.id(id.clone());
        self.id = Some(id);
        self
    }
    /// A sensitive value is omitted entirely, including length and offsets.
    pub fn private(mut self) -> Self {
        self.private = true;
        self
    }
    pub fn secure(mut self, secure: bool) -> Self {
        self.secret = secure;
        if secure {
            self.inner = self.inner.password();
        }
        self
    }
    pub fn on_input(mut self, callback: impl Fn(String) -> Message + 'a) -> Self {
        let callback: Rc<dyn Fn(String) -> Message + 'a> = Rc::new(callback);
        let input = callback.clone();
        self.inner = self.inner.on_input(move |value| input(value));
        self.on_input = Some(callback);
        self
    }
    pub fn on_input_maybe(self, callback: Option<impl Fn(String) -> Message + 'a>) -> Self {
        if let Some(callback) = callback {
            self.on_input(callback)
        } else {
            self
        }
    }
    pub fn on_submit(mut self, callback: impl Fn(String) -> Message + 'a) -> Self {
        self.inner = self.inner.on_submit(callback);
        self
    }
    pub fn on_submit_maybe(self, callback: Option<impl Fn(String) -> Message + 'a>) -> Self {
        if let Some(callback) = callback {
            self.on_submit(callback)
        } else {
            self
        }
    }
    pub fn on_focus(mut self, message: Message) -> Self {
        self.inner = self.inner.on_focus(message.clone());
        self.on_focus = Some(message);
        self
    }
    pub fn on_unfocus(mut self, message: Message) -> Self {
        self.inner = self.inner.on_unfocus(message);
        self
    }
    pub fn error(mut self, error: impl Into<Cow<'a, str>>) -> Self {
        self.inner = self.inner.error(error);
        self
    }
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.inner = self.inner.width(width);
        self
    }
    pub fn padding(mut self, padding: impl Into<Padding>) -> Self {
        self.inner = self.inner.padding(padding);
        self
    }
    pub fn size(mut self, size: impl Into<Pixels>) -> Self {
        self.inner = self.inner.size(size);
        self
    }
    pub fn line_height(
        mut self,
        line_height: impl Into<cosmic::iced::widget::text::LineHeight>,
    ) -> Self {
        self.inner = self.inner.line_height(line_height);
        self
    }
    pub fn style(mut self, style: impl Into<cosmic::theme::TextInput>) -> Self {
        self.inner = self.inner.style(style);
        self
    }
    pub fn leading_icon(mut self, icon: Element<'a, Message>) -> Self {
        self.inner = self.inner.leading_icon(icon);
        self
    }
    pub fn trailing_icon(mut self, icon: Element<'a, Message>) -> Self {
        self.inner = self.inner.trailing_icon(icon);
        self
    }
}

/// AccessKit indices count the declared characters, not bytes. Keep grapheme
/// boundaries together, including combining sequences and emoji. An unusually
/// long grapheme is split at UTF-8 boundaries because lengths are u8.
fn character_lengths(text: &str) -> Vec<u8> {
    let mut lengths = Vec::new();
    for grapheme in text.graphemes(true) {
        if let Ok(length) = u8::try_from(grapheme.len()) {
            lengths.push(length);
        } else {
            lengths.extend(grapheme.chars().map(|ch| ch.len_utf8() as u8));
        }
    }
    lengths
}
fn position_to_byte(text: &str, index: usize) -> Option<usize> {
    let lengths = character_lengths(text);
    (index <= lengths.len()).then(|| lengths[..index].iter().map(|n| usize::from(*n)).sum())
}
fn grapheme_to_character(text: &str, grapheme: usize) -> usize {
    let end = text
        .grapheme_indices(true)
        .nth(grapheme)
        .map_or(text.len(), |(offset, _)| offset);
    character_lengths(&text[..end]).len()
}
fn character_to_grapheme(text: &str, index: usize) -> Option<usize> {
    let offset = position_to_byte(text, index)?;
    if offset == text.len() {
        return Some(text.graphemes(true).count());
    }
    text.grapheme_indices(true)
        .position(|(byte, _)| byte == offset)
}
fn bounds(rect: Rectangle) -> Rect {
    Rect::new(
        rect.x.into(),
        rect.y.into(),
        (rect.x + rect.width).into(),
        (rect.y + rect.height).into(),
    )
}

impl<Message: Clone + 'static> Widget<Message, Theme, Renderer> for TextInput<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::new(self.id.clone(), &self.name))
    }
    fn children(&self) -> Vec<Tree> {
        let mut child = Tree::new(&self.inner as &dyn Widget<Message, Theme, Renderer>);
        child
            .state
            .downcast_mut::<widget::text_input::State>()
            .tracked_value = widget::text_input::value::Value::new(&self.value);
        vec![child]
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.state
            .downcast_mut::<State>()
            .bind(self.id.clone(), &self.name);
        let id = tree.state.downcast_ref::<State>().id.clone();
        self.inner.set_id(id);
        tree.children[0].diff(&mut self.inner as &mut dyn Widget<Message, Theme, Renderer>);
        if !tree.children[0]
            .state
            .downcast_ref::<widget::text_input::State>()
            .is_focused()
        {
            tree.state.downcast_ref::<State>().last_focused.set(false);
        }
    }
    fn id(&self) -> Option<Id> {
        self.id.clone()
    }
    fn set_id(&mut self, id: Id) {
        self.inner.set_id(id.clone());
        self.id = Some(id);
    }
    fn size(&self) -> Size<Length> {
        Widget::size(&self.inner)
    }
    fn size_hint(&self) -> Size<Length> {
        Widget::size_hint(&self.inner)
    }
    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        rectangles: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.inner
            .drag_destinations(&tree.children[0], layout, renderer, rectangles);
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.inner
            .set_id(tree.state.downcast_ref::<State>().id.clone());
        self.inner.layout(&mut tree.children[0], renderer, limits)
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
        Widget::draw(
            &self.inner,
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
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.inner
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.inner
            .operate(&mut tree.children[0], layout, renderer, operation);
        // Winit applies the focus operation before delivering the A11y event.
        // Retain the pending transition until that event, but retire a blur
        // immediately so re-entry can notify the app again.
        if !tree.children[0]
            .state
            .downcast_ref::<widget::text_input::State>()
            .is_focused()
        {
            tree.state.downcast_ref::<State>().last_focused.set(false);
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
        self.inner.overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
    fn a11y_nodes(&self, layout: Layout<'_>, tree: &Tree, _cursor: mouse::Cursor) -> A11yTree {
        use widget::text_input::cursor;
        let own = tree.state.downcast_ref::<State>();
        let state = tree.children[0]
            .state
            .downcast_ref::<widget::text_input::State>();
        let mut node = Node::new(if self.secret {
            Role::PasswordInput
        } else {
            Role::TextInput
        });
        node.set_label(self.name.as_ref());
        node.set_bounds(bounds(layout.bounds()));
        if self.on_input.is_none() {
            node.set_disabled();
            node.set_read_only();
        } else {
            node.add_action(Action::Focus);
            node.add_action(Action::SetValue);
            if !self.private {
                node.add_action(Action::ReplaceSelectedText);
                node.add_action(Action::SetTextSelection);
            }
        }
        if self.private {
            return A11yTree::leaf(node, own.id.clone());
        }
        let text = state.tracked_value.to_string();
        let (anchor, focus) = match state.cursor().state(&state.tracked_value) {
            cursor::State::Index(index) => (index, index),
            cursor::State::Selection { start, end } => (start, end),
        };
        // A secret never reaches any accessibility property or child, even
        // when the caller opts to reveal its visual representation elsewhere.
        let exposed = if self.secret {
            "•".repeat(text.graphemes(true).count())
        } else {
            text.clone()
        };
        let anchor = if self.secret {
            anchor
        } else {
            grapheme_to_character(&text, anchor)
        };
        let focus = if self.secret {
            focus
        } else {
            grapheme_to_character(&text, focus)
        };
        node.set_value(exposed.clone());
        node.set_text_selection(TextSelection {
            anchor: TextPosition {
                node: own.run_node_id(),
                character_index: anchor,
            },
            focus: TextPosition {
                node: own.run_node_id(),
                character_index: focus,
            },
        });
        let mut run = Node::new(Role::TextRun);
        run.set_value(exposed.clone());
        run.set_character_lengths(character_lengths(&exposed));
        run.set_bounds(bounds(layout.bounds()));
        A11yTree::node_with_child_tree(
            A11yNode::new(node, own.id.clone()),
            A11yTree::leaf(run, own.run_id.clone()),
        )
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
        if let Event::A11y(id, request) = event {
            let own = tree.state.downcast_ref::<State>();
            if own.owns(id) {
                if let Some(callback) = &self.on_input {
                    let state = tree.children[0]
                        .state
                        .downcast_mut::<widget::text_input::State>();
                    match (&request.action, &request.data) {
                        (Action::Focus, _) => {
                            state.focus();
                            if !own.last_focused.replace(true) {
                                if let Some(message) = &self.on_focus {
                                    shell.publish(message.clone());
                                }
                            }
                        }
                        (
                            Action::SetTextSelection,
                            Some(ActionData::SetTextSelection(selection)),
                        ) if !self.private
                            && selection.anchor.node == own.run_node_id()
                            && selection.focus.node == own.run_node_id() =>
                        {
                            let text = state.tracked_value.to_string();
                            let convert = |index| {
                                if self.secret {
                                    (index <= text.graphemes(true).count()).then_some(index)
                                } else {
                                    character_to_grapheme(&text, index)
                                }
                            };
                            if let (Some(start), Some(end)) = (
                                convert(selection.anchor.character_index),
                                convert(selection.focus.character_index),
                            ) {
                                state.select_range(start, end);
                            }
                        }
                        (Action::SetValue, Some(ActionData::Value(value))) => {
                            state.select_all();
                            let result = state.paste_text(&single_line(value));
                            shell.publish(callback(result));
                            shell.invalidate_layout();
                        }
                        (Action::ReplaceSelectedText, Some(ActionData::Value(value)))
                            if !self.private =>
                        {
                            let result = state.paste_text(&single_line(value));
                            shell.publish(callback(result));
                            shell.invalidate_layout();
                        }
                        _ => {}
                    }
                }
                shell.capture_event();
                shell.request_redraw();
                return;
            }
        }
        self.inner.update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        tree.state.downcast_ref::<State>().last_focused.set(
            tree.children[0]
                .state
                .downcast_ref::<widget::text_input::State>()
                .is_focused(),
        );
    }
}
fn single_line(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_control()).collect()
}
impl<'a, Message: Clone + 'static> From<TextInput<'a, Message>> for Element<'a, Message> {
    fn from(input: TextInput<'a, Message>) -> Self {
        Element::new(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::advanced::clipboard;
    use iced_accessibility::accesskit::{ActionRequest, TreeId};

    fn dispatch(
        input: &mut TextInput<'_, String>,
        tree: &mut Tree,
        renderer: &Renderer,
        layout: Layout<'_>,
        action: Action,
        data: Option<ActionData>,
    ) -> Vec<String> {
        let id = tree.state.downcast_ref::<State>().id.clone();
        let mut messages = Vec::new();
        input.update(
            tree,
            &Event::A11y(
                id.clone(),
                ActionRequest {
                    action,
                    target_tree: TreeId::ROOT,
                    target_node: A11yId::from(id).into(),
                    data,
                },
            ),
            layout,
            mouse::Cursor::Unavailable,
            renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &layout.bounds(),
        );
        messages
    }
    #[test]
    fn mounted_field_exposes_unicode_value_and_routes_selection_and_edit_to_callback() {
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut input = TextInput::new("Search settings", "aé👩‍💻z").on_input(|value| value);
        let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Renderer>);
        let node = input.layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(400.0, 60.0)),
        );
        let layout = Layout::new(&node);
        let accessible = input.a11y_nodes(layout, &tree, mouse::Cursor::Unavailable);
        assert_eq!(accessible.root()[0].node().role(), Role::TextInput);
        assert_eq!(accessible.root()[0].node().label(), Some("Search settings"));
        assert_eq!(accessible.root()[0].node().value(), Some("aé👩‍💻z"));
        assert_eq!(
            accessible.children()[0].node().character_lengths(),
            &[1, 2, 11, 1]
        );
        assert!(
            dispatch(
                &mut input,
                &mut tree,
                &renderer,
                layout,
                Action::Focus,
                None
            )
            .is_empty()
        );
        assert!(
            tree.children[0]
                .state
                .downcast_ref::<widget::text_input::State>()
                .is_focused()
        );
        let run = tree.state.downcast_ref::<State>().run_node_id();
        let selection = TextSelection {
            anchor: TextPosition {
                node: run,
                character_index: 1,
            },
            focus: TextPosition {
                node: run,
                character_index: 3,
            },
        };
        dispatch(
            &mut input,
            &mut tree,
            &renderer,
            layout,
            Action::SetTextSelection,
            Some(ActionData::SetTextSelection(selection)),
        );
        assert_eq!(
            dispatch(
                &mut input,
                &mut tree,
                &renderer,
                layout,
                Action::ReplaceSelectedText,
                Some(ActionData::Value("X".into()))
            ),
            vec!["aXz"]
        );
        assert_eq!(
            dispatch(
                &mut input,
                &mut tree,
                &renderer,
                layout,
                Action::SetValue,
                Some(ActionData::Value("new\nvalue".into()))
            ),
            vec!["newvalue"]
        );
        // An action from a stale/foreign run must not alter this field.
        dispatch(
            &mut input,
            &mut tree,
            &renderer,
            layout,
            Action::SetTextSelection,
            Some(ActionData::SetTextSelection(TextSelection {
                anchor: TextPosition {
                    node: NodeId(0),
                    character_index: 0,
                },
                focus: TextPosition {
                    node: NodeId(0),
                    character_index: 3,
                },
            })),
        );
        assert_eq!(
            dispatch(
                &mut input,
                &mut tree,
                &renderer,
                layout,
                Action::ReplaceSelectedText,
                Some(ActionData::Value("!".into()))
            ),
            vec!["newvalue!"]
        );
    }
    #[test]
    fn secret_and_private_fields_never_publish_their_actual_value() {
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let secret = "évery-secret-value";
        for private in [false, true] {
            let mut input = TextInput::new("Credential", secret).on_input(|value| value);
            input = if private {
                input.private()
            } else {
                input.secure(true)
            };
            let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Renderer>);
            let node = input.layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(400.0, 60.0)),
            );
            let accessible =
                input.a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
            for node in accessible.root().iter().chain(accessible.children()) {
                assert!(
                    !node
                        .node()
                        .value()
                        .is_some_and(|value| value.contains(secret))
                );
                assert!(
                    !node
                        .node()
                        .description()
                        .is_some_and(|value| value.contains(secret))
                );
            }
            if private {
                assert_eq!(accessible.root()[0].node().value(), None);
                assert_eq!(accessible.root()[0].node().text_selection(), None);
                assert!(accessible.children().is_empty());
            } else {
                assert_eq!(accessible.root()[0].node().role(), Role::PasswordInput);
                assert_eq!(
                    accessible.root()[0].node().value(),
                    Some("••••••••••••••••••")
                );
            }
        }
    }
    #[test]
    fn assistive_focus_notifies_once_after_runtime_focus_operation_and_again_after_blur() {
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut input = TextInput::new("URL", "https://example.com")
            .private()
            .on_input(|value| value)
            .on_focus("focused".to_owned());
        let mut tree = Tree::new(&input as &dyn Widget<String, Theme, Renderer>);
        let node = input.layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(400.0, 60.0)),
        );
        let layout = Layout::new(&node);
        let id = tree.state.downcast_ref::<State>().id.clone();
        // This is the production Winit order: operate focus, then A11y event.
        input.operate(
            &mut tree,
            layout,
            &renderer,
            &mut cosmic::iced::advanced::widget::operation::focusable::focus::<()>(id),
        );
        assert_eq!(
            dispatch(
                &mut input,
                &mut tree,
                &renderer,
                layout,
                Action::Focus,
                None
            ),
            vec!["focused"]
        );
        assert!(
            dispatch(
                &mut input,
                &mut tree,
                &renderer,
                layout,
                Action::Focus,
                None
            )
            .is_empty()
        );
        input.operate(
            &mut tree,
            layout,
            &renderer,
            &mut cosmic::iced::advanced::widget::operation::focusable::focus::<()>(Id::unique()),
        );
        assert_eq!(
            dispatch(
                &mut input,
                &mut tree,
                &renderer,
                layout,
                Action::Focus,
                None
            ),
            vec!["focused"]
        );
    }
    #[test]
    fn unicode_cursor_roundtrips_only_grapheme_boundaries() {
        let text = "é e\u{301} 👩‍💻";
        for index in 0..=text.graphemes(true).count() {
            assert_eq!(
                character_to_grapheme(text, grapheme_to_character(text, index)),
                Some(index)
            );
        }
        assert_eq!(character_to_grapheme(text, 999), None);
    }
    #[test]
    fn retained_identity_rebinds_for_a_different_field() {
        let mut state = State::new(None, "Search settings");
        let old = state.run_id.clone();
        state.bind(None, "Search settings");
        assert_eq!(state.run_id, old);
        state.bind(None, "Device name");
        assert_ne!(state.run_id, old);
    }
}
