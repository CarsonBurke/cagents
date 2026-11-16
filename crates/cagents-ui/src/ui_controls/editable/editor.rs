//! Form editors use the same native editor for ordinary interaction. Absolute
//! assistive selections go through an explicit app callback, never synthesized
//! key or pointer events.
use super::*;
use cosmic::iced::{advanced::text::highlighter::PlainText, widget::text_editor};
use std::cell::RefCell;

pub struct TextEditor<'a, Message> {
    inner: cosmic::iced::widget::TextEditor<'a, PlainText, Message, Theme, Renderer>,
    content: &'a text_editor::Content<Renderer>,
    name: Cow<'a, str>,
    id: Option<Id>,
    on_action: Option<Rc<dyn Fn(text_editor::Action) -> Message + 'a>>,
    on_cursor: Option<Rc<dyn Fn(text_editor::Cursor) -> Message + 'a>>,
}
struct EditorState {
    identity: State,
    lines: RefCell<Vec<Id>>,
}
impl<'a, Message: Clone + 'static> TextEditor<'a, Message> {
    pub fn new(name: impl Into<Cow<'a, str>>, content: &'a text_editor::Content<Renderer>) -> Self {
        Self {
            inner: cosmic::iced::widget::text_editor(content),
            content,
            name: name.into(),
            id: None,
            on_action: None,
            on_cursor: None,
        }
    }
    pub fn id(mut self, id: Id) -> Self {
        self.inner = self.inner.id(id.clone());
        self.id = Some(id);
        self
    }
    pub fn on_action(mut self, callback: impl Fn(text_editor::Action) -> Message + 'a) -> Self {
        let callback: Rc<dyn Fn(text_editor::Action) -> Message + 'a> = Rc::new(callback);
        let action = callback.clone();
        self.inner = self.inner.on_action(move |value| action(value));
        self.on_action = Some(callback);
        self
    }
    pub fn on_cursor(mut self, callback: impl Fn(text_editor::Cursor) -> Message + 'a) -> Self {
        self.on_cursor = Some(Rc::new(callback));
        self
    }
    pub fn placeholder(
        mut self,
        placeholder: impl cosmic::iced::advanced::text::IntoFragment<'a>,
    ) -> Self {
        self.inner = self.inner.placeholder(placeholder);
        self
    }
    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.inner = self.inner.height(height);
        self
    }
    pub fn max_height(mut self, height: impl Into<Pixels>) -> Self {
        self.inner = self.inner.max_height(height);
        self
    }
    pub fn font(mut self, font: cosmic::iced::Font) -> Self {
        self.inner = self.inner.font(font);
        self
    }
    pub fn size(mut self, size: impl Into<Pixels>) -> Self {
        self.inner = self.inner.size(size);
        self
    }
    pub fn padding(mut self, padding: impl Into<Padding>) -> Self {
        self.inner = self.inner.padding(padding);
        self
    }
    pub fn wrapping(mut self, wrapping: cosmic::iced::widget::text::Wrapping) -> Self {
        self.inner = self.inner.wrapping(wrapping);
        self
    }
    pub fn key_binding(
        mut self,
        binding: impl Fn(text_editor::KeyPress) -> Option<text_editor::Binding<Message>> + 'a,
    ) -> Self {
        self.inner = self.inner.key_binding(binding);
        self
    }
    pub fn style(
        mut self,
        style: impl Fn(&Theme, text_editor::Status) -> text_editor::Style + 'a,
    ) -> Self {
        self.inner = self.inner.style(style);
        self
    }
    pub fn class(mut self, class: impl Into<<Theme as text_editor::Catalog>::Class<'a>>) -> Self {
        self.inner = self.inner.class(class);
        self
    }
}
fn byte_to_character(text: &str, byte: usize) -> usize {
    let mut offset = 0;
    character_lengths(text)
        .into_iter()
        .take_while(|length| {
            offset += usize::from(*length);
            offset <= byte
        })
        .count()
}
impl<Message: Clone + 'static> Widget<Message, Theme, Renderer> for TextEditor<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<EditorState>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(EditorState {
            identity: State::new(self.id.clone(), &self.name),
            lines: RefCell::default(),
        })
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(
            &self.inner as &dyn Widget<Message, Theme, Renderer>,
        )]
    }
    fn diff(&mut self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<EditorState>();
        let old = state.identity.id.clone();
        state.identity.bind(self.id.clone(), &self.name);
        if old != state.identity.id {
            state.lines.get_mut().clear();
        }
        self.inner.set_id(state.identity.id.clone());
        tree.children[0].diff(&mut self.inner as &mut dyn Widget<Message, Theme, Renderer>);
    }
    fn id(&self) -> Option<Id> {
        // Keep the native editor as the sole owner of its named runtime state.
        None
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
            .set_id(tree.state.downcast_ref::<EditorState>().identity.id.clone());
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
        self.inner.draw(
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
        let state = tree.state.downcast_ref::<EditorState>();
        let mut ids = state.lines.borrow_mut();
        ids.resize_with(self.content.line_count(), Id::unique);
        let mut node = Node::new(Role::MultilineTextInput);
        node.set_label(self.name.as_ref());
        node.set_bounds(bounds(layout.bounds() - layout.virtual_offset()));
        if self.on_action.is_none() {
            node.set_read_only();
            node.set_disabled();
        } else {
            node.add_action(Action::Focus);
            node.add_action(Action::SetValue);
            node.add_action(Action::ReplaceSelectedText);
        }
        if self.on_action.is_some() && self.on_cursor.is_some() {
            node.add_action(Action::SetTextSelection);
        }
        let cursor = self.content.cursor();
        let convert = |position: text_editor::Position| -> Option<TextPosition> {
            let line = self.content.line(position.line)?;
            Some(TextPosition {
                node: A11yId::from(ids.get(position.line)?.clone()).into(),
                character_index: byte_to_character(&line.text, position.column),
            })
        };
        if let (Some(anchor), Some(focus)) = (
            convert(cursor.selection.unwrap_or(cursor.position)),
            convert(cursor.position),
        ) {
            node.set_text_selection(TextSelection { anchor, focus });
        }
        let children = self
            .content
            .lines()
            .enumerate()
            .map(|(index, line)| {
                let mut run = Node::new(Role::TextRun);
                let text = format!("{}{}", line.text, line.ending.as_str());
                run.set_character_lengths(character_lengths(&text));
                run.set_value(text);
                run.set_bounds(bounds(layout.bounds() - layout.virtual_offset()));
                A11yTree::leaf(run, ids[index].clone())
            })
            .collect::<Vec<_>>();
        A11yTree::node_with_child_tree(
            A11yNode::new(node, state.identity.id.clone()),
            A11yTree::join(children.into_iter()),
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
            let state = tree.state.downcast_ref::<EditorState>();
            let ids = state.lines.borrow();
            if IdEq::eq(id, &state.identity.id) || ids.iter().any(|own| IdEq::eq(id, own)) {
                if let Some(action) = &self.on_action {
                    match (&request.action, &request.data) {
                        (Action::Focus, _) => {
                            use cosmic::iced::advanced::widget::operation::Focusable;
                            tree.children[0]
                                .state
                                .downcast_mut::<text_editor::State<PlainText>>()
                                .focus();
                        }
                        (
                            Action::SetTextSelection,
                            Some(ActionData::SetTextSelection(selection)),
                        ) => {
                            if let Some(callback) = &self.on_cursor {
                                let convert =
                                    |position: TextPosition| -> Option<text_editor::Position> {
                                        let row = ids.iter().position(|id| {
                                            NodeId::from(A11yId::from(id.clone())) == position.node
                                        })?;
                                        let line = self.content.line(row)?;
                                        let text = format!("{}{}", line.text, line.ending.as_str());
                                        let column =
                                            position_to_byte(&text, position.character_index)?;
                                        if column <= line.text.len() {
                                            Some(text_editor::Position { line: row, column })
                                        } else if row + 1 < ids.len() {
                                            Some(text_editor::Position {
                                                line: row + 1,
                                                column: 0,
                                            })
                                        } else {
                                            None
                                        }
                                    };
                                if let (Some(anchor), Some(focus)) =
                                    (convert(selection.anchor), convert(selection.focus))
                                {
                                    shell.publish(callback(text_editor::Cursor {
                                        position: focus,
                                        selection: (anchor != focus).then_some(anchor),
                                    }));
                                }
                            }
                        }
                        (Action::SetValue, Some(ActionData::Value(value))) => {
                            shell.publish(action(text_editor::Action::SelectAll));
                            shell.publish(action(text_editor::Action::Edit(
                                text_editor::Edit::Paste(std::sync::Arc::new(value.to_string())),
                            )));
                            shell.invalidate_layout();
                        }
                        (Action::ReplaceSelectedText, Some(ActionData::Value(value))) => {
                            shell.publish(action(text_editor::Action::Edit(
                                text_editor::Edit::Paste(std::sync::Arc::new(value.to_string())),
                            )));
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
    }
}
impl<'a, Message: Clone + 'static> From<TextEditor<'a, Message>> for Element<'a, Message> {
    fn from(editor: TextEditor<'a, Message>) -> Self {
        Element::new(editor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::advanced::clipboard;
    use iced_accessibility::accesskit::{ActionRequest, TreeId};
    #[derive(Clone, Debug)]
    enum Message {
        Edit(text_editor::Action),
        Cursor(text_editor::Cursor),
    }
    #[test]
    fn multiline_selection_maps_unicode_bytes_and_replacement_uses_existing_edit_action() {
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut content = text_editor::Content::with_text("éclair\n👩‍💻 end");
        let id = Id::new("test.form.editor");
        let mut tree;
        let mut messages = Vec::new();
        {
            let mut editor = TextEditor::new("Instructions", &content)
                .id(id.clone())
                .on_action(Message::Edit)
                .on_cursor(Message::Cursor);
            tree = Tree::new(&editor as &dyn Widget<Message, Theme, Renderer>);
            assert!(
                tree.id.is_none(),
                "decorator must not duplicate native state ownership"
            );
            assert!(IdEq::eq(tree.children[0].id.as_ref().unwrap(), &id));
            let node = editor.layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(400.0, 180.0)),
            );
            let layout = Layout::new(&node);
            let accessible = editor.a11y_nodes(layout, &tree, mouse::Cursor::Unavailable);
            let shifted = editor.a11y_nodes(
                layout.with_virtual_offset(cosmic::iced::Vector::new(11.0, 37.0)),
                &tree,
                mouse::Cursor::Unavailable,
            );
            for (normal, shifted) in accessible
                .root()
                .iter()
                .chain(accessible.children())
                .zip(shifted.root().iter().chain(shifted.children()))
            {
                assert_eq!(normal.id(), shifted.id());
                if let Some(bounds) = normal.node().bounds() {
                    let moved = shifted.node().bounds().unwrap();
                    assert_eq!(moved.x0, bounds.x0 - 11.0);
                    assert_eq!(moved.x1, bounds.x1 - 11.0);
                    assert_eq!(moved.y0, bounds.y0 - 37.0);
                    assert_eq!(moved.y1, bounds.y1 - 37.0);
                }
            }

            assert!(IdEq::eq(
                accessible.root()[0].id(),
                &A11yId::from(id.clone())
            ));
            assert_eq!(accessible.root()[0].node().role(), Role::MultilineTextInput);
            assert_eq!(accessible.children()[0].node().value(), Some("éclair\n"));
            assert_eq!(accessible.children()[1].node().value(), Some("👩‍💻 end"));
            let selection = TextSelection {
                anchor: TextPosition {
                    node: accessible.children()[0].id().clone().into(),
                    character_index: 1,
                },
                focus: TextPosition {
                    node: accessible.children()[1].id().clone().into(),
                    character_index: 1,
                },
            };
            editor.update(
                &mut tree,
                &Event::A11y(
                    id.clone(),
                    ActionRequest {
                        action: Action::SetTextSelection,
                        target_tree: TreeId::ROOT,
                        target_node: A11yId::from(id.clone()).into(),
                        data: Some(ActionData::SetTextSelection(selection)),
                    },
                ),
                layout,
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &layout.bounds(),
            );
        }
        assert!(
            matches!(messages.as_slice(), [Message::Cursor(cursor)] if cursor.position == text_editor::Position { line: 1, column: 11 } && cursor.selection == Some(text_editor::Position { line: 0, column: 2 }))
        );
        for message in messages.drain(..) {
            if let Message::Cursor(cursor) = message {
                content.move_to(cursor);
            }
        }
        assert_eq!(content.selection().as_deref(), Some("clair\n👩‍💻"));
        {
            let mut editor = TextEditor::new("Instructions", &content)
                .id(id.clone())
                .on_action(Message::Edit)
                .on_cursor(Message::Cursor);
            editor.diff(&mut tree);
            let node = editor.layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(400.0, 180.0)),
            );
            let layout = Layout::new(&node);
            editor.update(
                &mut tree,
                &Event::A11y(
                    id.clone(),
                    ActionRequest {
                        action: Action::ReplaceSelectedText,
                        target_tree: TreeId::ROOT,
                        target_node: A11yId::from(id).into(),
                        data: Some(ActionData::Value("X".into())),
                    },
                ),
                layout,
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &layout.bounds(),
            );
        }
        for message in messages {
            if let Message::Edit(action) = message {
                content.perform(action);
            }
        }
        assert_eq!(content.text(), "éX end");
    }
}
