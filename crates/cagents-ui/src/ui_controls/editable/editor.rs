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
    read_only: bool,
    padding: Padding,
    padding_fade: Option<PaddingFade<'a>>,
}
/// Text scrolled into the editor's vertical padding, and the surface it fades
/// into there. See [`TextEditor::fade_into_padding`].
struct PaddingFade<'a> {
    above: f32,
    below: f32,
    fill: Box<dyn Fn(&Theme) -> cosmic::iced::Color + 'a>,
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
            read_only: false,
            // The native editor's own default.
            padding: Padding::new(5.0),
            padding_fade: None,
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
    /// Text that can be scrolled, selected and copied but not changed. The
    /// caller's `on_action` still receives the editor's actions, and must
    /// perform only those that are not edits; assistive technology is told
    /// the text is read-only and offered no way to replace it.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
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
        self.padding = padding.into();
        self.inner = self.inner.padding(self.padding);
        self
    }
    /// Lets text scrolled past the top or the bottom of the text area carry
    /// on up to `above` and `below` into the vertical padding, fading into
    /// `fill` there, rather than being cut off at the padding's inner edge.
    ///
    /// `fill` is the surface the editor is drawn on. The fade is as deep as
    /// the text reaches into the padding, so it is absent while that end of
    /// the text is in view and grows as it scrolls away. The editor still
    /// scrolls, and keeps its caret, within the text area.
    pub fn fade_into_padding(
        mut self,
        above: f32,
        below: f32,
        fill: impl Fn(&Theme) -> cosmic::iced::Color + 'a,
    ) -> Self {
        self.inner = self.inner.overscan(above, below);
        self.padding_fade = Some(PaddingFade {
            above,
            below,
            fill: Box::new(fill),
        });
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
/// Where an editor with `padding` and `overscan` draws its text, and how deep
/// the fade at each end of that area is when the drawn text reaches
/// `overflow` past the text area. The area spans the editor's whole width,
/// and its bands are the native editor's own (`overscan_bounds`), so the
/// fade covers exactly what the editor drew there. Each fade is as deep as
/// the text reaches, but no deeper than its band.
fn padding_fade_geometry(
    bounds: Rectangle,
    padding: Padding,
    overscan: (f32, f32),
    overflow: (f32, f32),
    scale_factor: f32,
) -> (Rectangle, (f32, f32)) {
    let text = bounds.shrink(padding);
    let drawn = text_editor::overscan_bounds(text, padding, overscan, scale_factor);
    let band = (
        text.y - drawn.y,
        (drawn.y + drawn.height) - (text.y + text.height),
    );
    (
        Rectangle {
            y: drawn.y,
            height: drawn.height,
            ..bounds
        },
        (overflow.0.clamp(0.0, band.0), overflow.1.clamp(0.0, band.1)),
    )
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
        if let Some(fade) = &self.padding_fade {
            let overflow = self.content.overflow();
            if overflow.0 > 0.0 || overflow.1 > 0.0 {
                let bounds = layout.bounds();
                let (drawn, depth) = padding_fade_geometry(
                    bounds,
                    self.padding,
                    (fade.above, fade.below),
                    overflow,
                    style.scale_factor as f32,
                );
                let color = (fade.fill)(theme);
                cosmic::iced::advanced::Renderer::with_layer(renderer, drawn, |renderer| {
                    crate::interaction::fade_vertical_edges(
                        renderer, drawn, depth.0, depth.1, color,
                    );
                });
            }
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
        } else if self.read_only {
            node.set_read_only();
            node.add_action(Action::Focus);
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
                        (Action::SetValue | Action::ReplaceSelectedText, _) if self.read_only => {}
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
    /// The composer's text, fade and selection bands stop on a whole device
    /// pixel inside its 1 px border, whatever fractional position and scale
    /// it lands on, so the renderer's rounded clip never takes in a row the
    /// antialiased border paints (#2824). Each fade is as deep as the text
    /// reaches, within its band, and an overscan is limited to the padding.
    #[test]
    fn padding_fade_stops_on_a_device_pixel_inside_the_border() {
        // The composer's geometry: a 19.6 px line, a 9.2 px inset and a 1 px
        // border, with the overscan reaching to the border.
        let border = 1.0;
        let inset = 9.2;
        let reach = inset - border;
        let padding = Padding::from([inset, 8.0]);
        let device = |logical: f32, scale: f32| logical * scale;
        for scale in [0.8, 1.0, 1.25, 1.5, 1.75, 2.0, 3.0] {
            for top in [0.0, 573.73, 580.4, 580.5, 611.15, 777.95] {
                let bounds = Rectangle {
                    x: 326.3,
                    y: top,
                    width: 720.0,
                    height: 12.0 * 19.6 + 2.0 * inset,
                };
                let text = bounds.shrink(padding);
                let (drawn, depth) =
                    padding_fade_geometry(bounds, padding, (reach, reach), (reach, reach), scale);
                let context = format!("scale {scale}, top {top}: {drawn:?}");
                let drawn_top = device(drawn.y, scale);
                let drawn_bottom = device(drawn.y + drawn.height, scale);
                assert!((drawn_top - drawn_top.round()).abs() < 1e-3, "{context}");
                assert!(
                    (drawn_bottom - drawn_bottom.round()).abs() < 1e-3,
                    "{context}"
                );
                // Clear of every device row the border touches.
                let border_top = device(bounds.y + border, scale).ceil();
                let border_bottom = device(bounds.y + bounds.height - border, scale).floor();
                assert!(drawn_top.round() >= border_top, "{context}");
                assert!(drawn_bottom.round() <= border_bottom, "{context}");
                // And within a device pixel of it, so the band is not lost.
                assert!(drawn_top.round() - border_top <= 1.0, "{context}");
                assert!(border_bottom - drawn_bottom.round() <= 1.0, "{context}");
                assert!(drawn.y <= text.y && drawn.y + drawn.height >= text.y + text.height);
                assert_eq!((drawn.x, drawn.width), (bounds.x, bounds.width));
                // The fades fill their bands exactly when the text reaches
                // across them.
                assert!((depth.0 - (text.y - drawn.y)).abs() < 1e-4, "{context}");
                assert!(
                    (depth.1 - (drawn.y + drawn.height - text.y - text.height)).abs() < 1e-4,
                    "{context}"
                );
                // Text reaching only part of the way fades only that far,
                // and none at an end with nothing past it.
                let (_, depth) =
                    padding_fade_geometry(bounds, padding, (reach, reach), (3.0, 0.0), scale);
                assert_eq!(depth, (3.0, 0.0), "{context}");
                // An overscan is limited to the padding.
                let (drawn, _) =
                    padding_fade_geometry(bounds, padding, (50.0, 50.0), (50.0, 50.0), scale);
                assert!(drawn.y >= bounds.y - 1e-4, "{context}");
                assert!(
                    drawn.y + drawn.height <= bounds.y + bounds.height + 1e-4,
                    "{context}"
                );
                // Without an overscan the text area is drawn as upstream.
                let (drawn, depth) =
                    padding_fade_geometry(bounds, padding, (0.0, 0.0), (0.0, 0.0), scale);
                assert_eq!((drawn.y, drawn.height), (text.y, text.height), "{context}");
                assert_eq!(depth, (0.0, 0.0));
            }
        }
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

    #[test]
    fn read_only_text_is_announced_read_only_and_refuses_replacement() {
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let content = text_editor::Content::with_text("kept draft");
        let id = Id::new("test.form.read-only");
        let mut messages = Vec::new();
        let mut editor = TextEditor::new("Message", &content)
            .id(id.clone())
            .read_only()
            .on_action(Message::Edit);
        let mut tree = Tree::new(&editor as &dyn Widget<Message, Theme, Renderer>);
        let node = editor.layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(400.0, 180.0)),
        );
        let layout = Layout::new(&node);
        let accessible = editor.a11y_nodes(layout, &tree, mouse::Cursor::Unavailable);
        let field = accessible.root()[0].node();
        assert!(field.is_read_only());
        assert!(!field.is_disabled(), "it can still be read and selected");
        assert!(field.supports_action(Action::Focus));
        assert!(!field.supports_action(Action::SetValue));
        assert!(!field.supports_action(Action::ReplaceSelectedText));
        for action in [Action::SetValue, Action::ReplaceSelectedText] {
            editor.update(
                &mut tree,
                &Event::A11y(
                    id.clone(),
                    ActionRequest {
                        action,
                        target_tree: TreeId::ROOT,
                        target_node: A11yId::from(id.clone()).into(),
                        data: Some(ActionData::Value("replaced".into())),
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
        assert!(messages.is_empty(), "{messages:?}");
    }
}
