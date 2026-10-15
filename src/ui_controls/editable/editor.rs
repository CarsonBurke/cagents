//! Form editors use the same native editor for ordinary interaction. Absolute
//! assistive selections go through an explicit app callback, never synthesized
//! key or pointer events.
use super::*;
use cosmic::iced::{advanced::text::highlighter::PlainText, widget::text_editor};
use std::{
    cell::{Cell, RefCell},
    sync::{Arc, Mutex},
};

/// A form draft owns its identity independently of its address or widget ID.
/// Replacing or cloning a draft cannot admit callbacks from the previous one.
#[derive(Debug)]
pub struct EditorContent {
    native: text_editor::Content<Renderer>,
    owner: Arc<()>,
}
impl EditorContent {
    pub fn new() -> Self {
        Self::with_text("")
    }
    pub fn with_text(text: &str) -> Self {
        Self {
            native: text_editor::Content::with_text(text),
            owner: Arc::new(()),
        }
    }
    /// Normalize the just-accepted edit without replacing this draft identity.
    pub(crate) fn normalize_text(&mut self, text: &str) {
        self.native = text_editor::Content::with_text(text);
    }
}
impl Default for EditorContent {
    fn default() -> Self {
        Self::new()
    }
}
impl Clone for EditorContent {
    fn clone(&self) -> Self {
        Self::with_text(&self.native.text())
    }
}
impl std::ops::Deref for EditorContent {
    type Target = text_editor::Content<Renderer>;
    fn deref(&self) -> &Self::Target {
        &self.native
    }
}
impl std::ops::DerefMut for EditorContent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.native
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryIntent {
    Observe,
    Undo,
    Redo,
}

/// Dispatched after the native edit, then applied by the consumer reducer.
/// Native UI.update batches events before reducers run; reading the view
/// Content while interpreting Undo would observe the previous batch instead.
#[derive(Clone)]
pub struct EditorHistoryCommand {
    owner: Arc<()>,
    history: Arc<Mutex<EditHistory>>,
    intent: HistoryIntent,
}
impl std::fmt::Debug for EditorHistoryCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorHistoryCommand")
            .field("intent", &self.intent)
            .finish_non_exhaustive()
    }
}
impl PartialEq for EditorHistoryCommand {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
            && Arc::ptr_eq(&self.history, &other.history)
            && self.intent == other.intent
    }
}
impl Eq for EditorHistoryCommand {}
impl EditorHistoryCommand {
    pub fn changes_text(&self) -> bool {
        !matches!(self.intent, HistoryIntent::Observe)
    }
    /// Returns true only when an admitted Undo/Redo changed this same draft.
    pub fn apply(&self, content: &mut EditorContent) -> bool {
        if !Arc::ptr_eq(&self.owner, &content.owner) {
            return false;
        }
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        history.observe(content.text(), content.cursor());
        let redo = match self.intent {
            HistoryIntent::Observe => return false,
            HistoryIntent::Undo => false,
            HistoryIntent::Redo => true,
        };
        let Some(snapshot) = history.restore(redo) else {
            return false;
        };
        content.perform(text_editor::Action::SelectAll);
        content.perform(text_editor::Action::Edit(text_editor::Edit::Paste(
            Arc::new(snapshot.text.clone()),
        )));
        content.move_to(snapshot.cursor);
        true
    }
}

/// Bounded per-control history supplies the undo/redo operations absent from
/// iced's form editor. Record text only on focused keyboard input, never on
/// redraw, so an idle retained form does no cloning or history work.
#[derive(Debug)]
struct EditSnapshot {
    text: String,
    cursor: text_editor::Cursor,
}
#[derive(Debug)]
struct EditHistory {
    entries: Vec<EditSnapshot>,
    current: usize,
    bytes: usize,
}
impl EditHistory {
    fn new(text: String, cursor: text_editor::Cursor) -> Self {
        let bytes = text.len();
        Self {
            entries: vec![EditSnapshot { text, cursor }],
            current: 0,
            bytes,
        }
    }
    fn observe(&mut self, text: String, cursor: text_editor::Cursor) {
        if text == self.entries[self.current].text {
            self.entries[self.current].cursor = cursor;
            return;
        }
        for entry in self.entries.drain(self.current + 1..) {
            self.bytes -= entry.text.len();
        }
        self.bytes += text.len();
        self.entries.push(EditSnapshot { text, cursor });
        while self.entries.len() > 128 || self.bytes > 1024 * 1024 && self.entries.len() > 1 {
            self.bytes -= self.entries.remove(0).text.len();
        }
        self.current = self.entries.len() - 1;
    }
    fn restore(&mut self, redo: bool) -> Option<&EditSnapshot> {
        let target = if redo {
            self.current
                .checked_add(1)
                .filter(|target| *target < self.entries.len())?
        } else {
            self.current.checked_sub(1)?
        };
        self.current = target;
        self.entries.get(target)
    }
}
struct EditorFocus(bool);
impl Operation for EditorFocus {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }
    fn focusable(
        &mut self,
        _id: Option<&Id>,
        _bounds: Rectangle,
        state: &mut dyn cosmic::iced::advanced::widget::operation::Focusable,
    ) {
        self.0 |= state.is_focused();
    }
}

pub struct TextEditor<'a, Message> {
    inner: cosmic::iced::widget::TextEditor<'a, PlainText, Message, Theme, Renderer>,
    content: &'a EditorContent,
    name: Cow<'a, str>,
    id: Option<Id>,
    on_action: Option<Rc<dyn Fn(text_editor::Action) -> Message + 'a>>,
    on_cursor: Option<Rc<dyn Fn(text_editor::Cursor) -> Message + 'a>>,
    on_history: Option<Rc<dyn Fn(EditorHistoryCommand) -> Message + 'a>>,
    emitted: Rc<Cell<bool>>,
}
struct EditorState {
    identity: State,
    lines: RefCell<Vec<Id>>,
    history: Arc<Mutex<EditHistory>>,
    owner: Arc<()>,
}
impl<'a, Message: Clone + 'static> TextEditor<'a, Message> {
    pub fn new(name: impl Into<Cow<'a, str>>, content: &'a EditorContent) -> Self {
        Self {
            inner: cosmic::iced::widget::text_editor(&content.native),
            content,
            name: name.into(),
            id: None,
            on_action: None,
            on_cursor: None,
            on_history: None,
            emitted: Rc::new(Cell::new(false)),
        }
    }
    pub fn id(mut self, id: Id) -> Self {
        self.inner = self.inner.id(id.clone());
        self.id = Some(id);
        self
    }
    pub fn on_action(mut self, callback: impl Fn(text_editor::Action) -> Message + 'a) -> Self {
        let emitted = self.emitted.clone();
        let callback: Rc<dyn Fn(text_editor::Action) -> Message + 'a> = Rc::new(move |action| {
            emitted.set(true);
            callback(action)
        });
        let action = callback.clone();
        self.inner = self.inner.on_action(move |value| action(value));
        self.on_action = Some(callback);
        self
    }
    pub fn on_history(mut self, callback: impl Fn(EditorHistoryCommand) -> Message + 'a) -> Self {
        self.on_history = Some(Rc::new(callback));
        self
    }
    fn publish_history(&self, tree: &Tree, intent: HistoryIntent, shell: &mut Shell<'_, Message>) {
        if let Some(callback) = &self.on_history {
            let state = tree.state.downcast_ref::<EditorState>();
            shell.publish(callback(EditorHistoryCommand {
                owner: self.content.owner.clone(),
                history: state.history.clone(),
                intent,
            }));
        }
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
            history: Arc::new(Mutex::new(EditHistory::new(
                self.content.text(),
                self.content.cursor(),
            ))),
            owner: self.content.owner.clone(),
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
        if old != state.identity.id || !Arc::ptr_eq(&state.owner, &self.content.owner) {
            state.lines.get_mut().clear();
            state.history = Arc::new(Mutex::new(EditHistory::new(
                self.content.text(),
                self.content.cursor(),
            )));
            state.owner = self.content.owner.clone();
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
        let own = tree.state.downcast_ref::<EditorState>();
        operation.custom(
            Some(&own.identity.id),
            layout.bounds(),
            &mut crate::input_trace::TextEditing,
        );
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
        self.emitted.set(false);
        let mut observe_key = false;
        if let cosmic::iced::Event::Keyboard(cosmic::iced::keyboard::Event::KeyPressed {
            key,
            physical_key,
            modifiers,
            ..
        }) = event
            && self.on_action.is_some()
            && self.on_history.is_some()
        {
            let mut focused = EditorFocus(false);
            self.inner
                .operate(&mut tree.children[0], layout, renderer, &mut focused);
            if focused.0 {
                observe_key = true;
                let command = if modifiers.command() && !modifiers.alt() {
                    match key
                        .to_latin(*physical_key)
                        .map(|key| key.to_ascii_lowercase())
                    {
                        Some('z') => Some(modifiers.shift()),
                        Some('y') if !modifiers.shift() => Some(true),
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(redo) = command {
                    self.publish_history(
                        tree,
                        if redo {
                            HistoryIntent::Redo
                        } else {
                            HistoryIntent::Undo
                        },
                        shell,
                    );
                    shell.invalidate_layout();
                    shell.request_redraw();
                    shell.capture_event();
                    return;
                }
            }
        }
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
                if self.emitted.replace(false) {
                    self.publish_history(tree, HistoryIntent::Observe, shell);
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
        if self.emitted.replace(false) || observe_key {
            self.publish_history(tree, HistoryIntent::Observe, shell);
        }
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
        History(EditorHistoryCommand),
        Keyboard(
            cosmic::iced::keyboard::Event,
            bool,
            crate::input_trace::KeyboardRegion,
        ),
    }
    #[test]
    fn native_composer_keys_undo_and_redo_without_a_workspace_action() {
        use crate::{
            input_trace,
            workspace_widget::{self, FocusedPane},
        };
        use cosmic::iced::keyboard::{
            self,
            key::{Code, Physical},
        };
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut content = EditorContent::new();
        let id = Id::new("test.chat-composer");
        let trace = input_trace::InputTrace::default();
        let owning_card = crate::model::CardId::new();
        let mut tree = Tree::empty();
        for (key, code, modifiers, text, expected) in [
            (
                "a",
                Code::KeyA,
                keyboard::Modifiers::empty(),
                Some("a"),
                "a",
            ),
            ("z", Code::KeyZ, keyboard::Modifiers::CTRL, None, ""),
            (
                "Z",
                Code::KeyZ,
                keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
                None,
                "a",
            ),
        ] {
            let event = keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(key.into()),
                modified_key: keyboard::Key::Character(key.into()),
                physical_key: Physical::Code(code),
                modifiers,
                text: text.map(Into::into),
                repeat: false,
                location: keyboard::Location::Standard,
            };
            let mut messages = Vec::new();
            {
                let editor = TextEditor::new("Message", &content)
                    .id(id.clone())
                    .on_action(Message::Edit)
                    .on_history(Message::History)
                    .on_cursor(Message::Cursor);
                let mut element = input_trace::observe_keyboard_events(
                    editor,
                    &trace,
                    vec![(
                        id.clone(),
                        input_trace::KeyboardRegion {
                            pane: FocusedPane::Chat,
                            card: Some(owning_card),
                        },
                    )],
                    |event, captured, region, _widget| Message::Keyboard(event, captured, region),
                );
                tree.diff(element.as_widget_mut());
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(400., 180.)),
                );
                let layout = Layout::new(&node);
                element.as_widget_mut().operate(
                    &mut tree,
                    layout,
                    &renderer,
                    &mut cosmic::iced::advanced::widget::operation::focusable::focus::<()>(
                        id.clone(),
                    ),
                );
                element.as_widget_mut().update(
                    &mut tree,
                    &Event::Keyboard(event.clone()),
                    layout,
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &layout.bounds(),
                );
            }
            for message in messages {
                match message {
                    Message::Edit(action) => content.perform(action),
                    Message::Cursor(cursor) => content.move_to(cursor),
                    Message::History(command) => {
                        command.apply(&mut content);
                    }
                    Message::Keyboard(delivered, captured, region) => {
                        assert_eq!(delivered, event);
                        assert!(captured, "the text widget handles each tested edit chord");
                        if modifiers.control() {
                            assert_eq!(region.card, Some(owning_card));
                            assert_eq!(region.pane, FocusedPane::Chat);
                        }
                    }
                }
            }
            assert_eq!(content.text(), expected);
            if modifiers.control() {
                assert_eq!(trace.keyboard_focus(), Some(FocusedPane::Chat));
                assert_eq!(trace.keyboard_card(), Some(owning_card));
                assert_eq!(
                    workspace_widget::global_shortcut(
                        &event,
                        &crate::settings::KeyboardShortcuts::default(),
                        trace.keyboard_focus().unwrap()
                    ),
                    None
                );
            }
        }
    }

    fn batch_key(letter: char, control: bool) -> Event {
        use cosmic::iced::keyboard::{
            self,
            key::{Code, Physical},
        };
        let code = match letter {
            'a' => Code::KeyA,
            'b' => Code::KeyB,
            'c' => Code::KeyC,
            'z' => Code::KeyZ,
            'y' => Code::KeyY,
            _ => unreachable!(),
        };
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Character(letter.to_string().into()),
            modified_key: keyboard::Key::Character(letter.to_string().into()),
            physical_key: Physical::Code(code),
            modifiers: if control {
                keyboard::Modifiers::CTRL
            } else {
                keyboard::Modifiers::empty()
            },
            text: (!control).then(|| letter.to_string().into()),
            repeat: false,
            location: keyboard::Location::Standard,
        })
    }

    fn native_batch(content: &EditorContent, events: &[Event]) -> Vec<Message> {
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let id = Id::new("test.batch-composer");
        let editor = TextEditor::new("Message", content)
            .id(id.clone())
            .on_action(Message::Edit)
            .on_cursor(Message::Cursor)
            .on_history(Message::History);
        let mut ui =
            UserInterface::build(editor, Size::new(400.0, 180.0), Cache::new(), &mut renderer);
        ui.operate(
            &renderer,
            &mut cosmic::iced::advanced::widget::operation::focusable::focus::<()>(id),
        );
        let mut messages = Vec::new();
        // This is deliberately ONE native UI update, followed by the reducer.
        // Rebuilding/reducing after each event hides the regression.
        let _ = ui.update(
            events,
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        messages
    }

    #[test]
    fn one_native_event_batch_preserves_type_undo_redo_and_new_edit_order() {
        for (keys, expected) in [
            (vec![('a', false), ('z', true)], ""),
            (
                vec![
                    ('a', false),
                    ('b', false),
                    ('z', true),
                    ('z', true),
                    ('y', true),
                ],
                "a",
            ),
            (
                vec![
                    ('a', false),
                    ('b', false),
                    ('z', true),
                    ('z', true),
                    ('y', true),
                    ('c', false),
                    ('y', true),
                ],
                "ac",
            ),
        ] {
            let mut content = EditorContent::new();
            let events = keys
                .into_iter()
                .map(|(key, control)| batch_key(key, control))
                .collect::<Vec<_>>();
            let messages = native_batch(&content, &events);
            assert_eq!(
                content.text(),
                "",
                "widgets must not mutate the consumer's draft"
            );
            for message in messages {
                match message {
                    Message::Edit(action) => content.perform(action),
                    Message::Cursor(cursor) => content.move_to(cursor),
                    Message::History(command) => {
                        command.apply(&mut content);
                    }
                    Message::Keyboard(..) => unreachable!(),
                }
            }
            assert_eq!(content.text(), expected);
        }
    }

    #[test]
    fn batched_history_observes_accepted_normalized_or_refused_edits_only() {
        for refuse in [false, true] {
            let mut content = EditorContent::new();
            let messages = native_batch(
                &content,
                &[
                    batch_key('a', false),
                    batch_key('z', true),
                    batch_key('y', true),
                ],
            );
            for message in messages {
                match message {
                    Message::Edit(text_editor::Action::Edit(text_editor::Edit::Insert(
                        character,
                    ))) if !refuse => {
                        content.perform(text_editor::Action::Edit(text_editor::Edit::Insert(
                            character,
                        )));
                        // Like a single-line commit subject, normalization is
                        // accepted in this same draft, not a new draft owner.
                        content.normalize_text(&content.text().to_ascii_uppercase());
                    }
                    Message::Edit(_) => {}
                    Message::History(command) => {
                        command.apply(&mut content);
                    }
                    Message::Cursor(cursor) => content.move_to(cursor),
                    Message::Keyboard(..) => unreachable!(),
                }
            }
            assert_eq!(content.text(), if refuse { "" } else { "A" });
        }
    }

    #[test]
    fn stale_history_callback_cannot_observe_or_restore_a_successor_draft() {
        let mut original = EditorContent::new();
        let messages = native_batch(
            &original,
            &[
                batch_key('a', false),
                batch_key('z', true),
                batch_key('y', true),
            ],
        );
        let mut successor = EditorContent::with_text("keep successor");
        for message in messages {
            match message {
                Message::Edit(action) => original.perform(action),
                Message::History(command) => {
                    assert!(!command.apply(&mut successor));
                    assert_eq!(successor.text(), "keep successor");
                    // Refusal must precede history mutation: the same callback
                    // still works for its actual original draft afterwards.
                    command.apply(&mut original);
                }
                Message::Cursor(cursor) => original.move_to(cursor),
                Message::Keyboard(..) => unreachable!(),
            }
        }
        assert_eq!(original.text(), "a");
        assert_eq!(successor.text(), "keep successor");
    }

    #[test]
    fn editor_history_discards_redo_after_new_text_and_bounds_retention() {
        let cursor = text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 0 },
            selection: None,
        };
        let mut history = EditHistory::new(String::new(), cursor);
        history.observe("a".into(), cursor);
        assert_eq!(history.restore(false).unwrap().text, "");
        history.observe("".into(), cursor);
        history.observe("b".into(), cursor);
        assert!(history.restore(true).is_none());
        for value in 0..200 {
            history.observe(format!("{value}{}", "x".repeat(20_000)), cursor);
        }
        assert!(history.bytes <= 1024 * 1024);
        assert!(history.entries.len() <= 128);
    }

    #[test]
    fn multiline_selection_maps_unicode_bytes_and_replacement_uses_existing_edit_action() {
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut content = EditorContent::with_text("éclair\n👩‍💻 end");
        let id = Id::new("test.form.editor");
        let mut tree;
        let mut messages = Vec::new();
        {
            let mut editor = TextEditor::new("Instructions", &content)
                .id(id.clone())
                .on_action(Message::Edit)
                .on_history(Message::History)
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
                .on_history(Message::History)
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
