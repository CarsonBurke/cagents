//! Owned selectable rich text: selection uses the same shaped paragraph as prose.
use super::*;
use iced::advanced::text::{Paragraph, Renderer as TextRenderer};
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, renderer,
    widget::{Tree, tree},
};
use iced::widget::text::{Span, Wrapping};
use iced::{Event, Point, Rectangle, Size};

type ParagraphBuffer = <cosmic::Renderer as TextRenderer>::Paragraph;

static NEXT_SELECTION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static ACTIVE_SELECTION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) struct State {
    pub(super) identity: u64,
    a11y_id: iced::advanced::widget::Id,
    pub(super) plain: Arc<str>,
    pub(super) target: Option<Target>,
    pub(super) managed: bool,
    pub(super) dirty: bool,
    pub(super) spans: Option<Arc<[Span<'static, String>]>>,
    paragraph: ParagraphBuffer,
    pub(super) anchor: usize,
    pub(super) end: usize,
    dragging: bool,
    focused: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            a11y_id: iced::advanced::widget::Id::unique(),
            identity: NEXT_SELECTION.fetch_add(1, Ordering::Relaxed),
            plain: Arc::from(""),
            target: None,
            managed: false,
            dirty: false,
            spans: None,
            paragraph: ParagraphBuffer::default(),
            anchor: 0,
            end: 0,
            dragging: false,
            focused: false,
        }
    }
}

impl State {
    pub(super) fn hit(&self, position: Point) -> usize {
        hit(&self.paragraph, &self.plain, position)
    }

    pub(super) fn link(&self, position: Point) -> Option<String> {
        self.paragraph
            .hit_span(position)
            .and_then(|index| self.spans.as_ref()?.get(index))
            .and_then(|span| span.link.clone())
    }

    fn validate_selection(&mut self, plain: &str) {
        if !plain.is_char_boundary(self.anchor) || !plain.is_char_boundary(self.end) {
            self.anchor = 0;
            self.end = 0;
            self.dragging = false;
        }
    }
}

pub(super) fn text(
    spans: Arc<[Span<'static, String>]>,
    size: iced::Pixels,
    font: iced::Font,
    target: Target,
    find: Option<&str>,
) -> Element<'static, Message> {
    let plain: Arc<str> = spans
        .iter()
        .map(|span| span.text.as_ref())
        .collect::<String>()
        .into();
    let matches = find
        .filter(|query| !query.is_empty())
        .map_or_else(Vec::new, |query| {
            plain
                .match_indices(query)
                .map(|(start, found)| start..start + found.len())
                .collect()
        });
    Element::new(Selectable {
        spans,
        plain,
        size,
        font,
        target,
        matches,
        child: widget::text("").into(),
    })
}

pub(super) fn plain(
    text: impl Into<String>,
    size: f32,
    font: iced::Font,
    target: Target,
) -> Element<'static, Message> {
    self::text(
        vec![Span::new(text.into())].into(),
        size.into(),
        font,
        target,
        None,
    )
}

struct Selectable {
    matches: Vec<std::ops::Range<usize>>,
    spans: Arc<[Span<'static, String>]>,
    plain: Arc<str>,
    size: iced::Pixels,
    font: iced::Font,
    target: Target,
    child: Element<'static, Message>,
}

impl Selectable {
    fn selected_spans(&self, state: &State) -> Arc<[Span<'static, String>]> {
        let start = state.anchor.min(state.end);
        let end = state.anchor.max(state.end);
        if start == end && self.matches.is_empty() {
            return self.spans.clone();
        }
        let mut offset = 0;
        let mut result = Vec::new();
        for span in self.spans.iter() {
            let next = offset + span.text.len();
            let lo = start.clamp(offset, next) - offset;
            let hi = end.clamp(offset, next) - offset;
            let mut boundaries = vec![0, lo, hi, span.text.len()];
            for found in &self.matches {
                boundaries.push(found.start.clamp(offset, next) - offset);
                boundaries.push(found.end.clamp(offset, next) - offset);
            }
            boundaries.sort_unstable();
            boundaries.dedup();
            for pair in boundaries.windows(2) {
                let range = pair[0]..pair[1];
                let selected = range.start >= lo && range.end <= hi && lo != hi;
                let found = self
                    .matches
                    .iter()
                    .any(|found| found.contains(&(offset + range.start)));
                if range.is_empty() {
                    continue;
                }
                let mut part = span.clone();
                part.text = span.text[range].to_owned().into();
                if selected || found {
                    part.highlight = Some(iced::advanced::text::Highlight {
                        background: if selected {
                            design::accent_color(&cosmic::theme::active())
                        } else {
                            design::accent_wash(&cosmic::theme::active())
                        }
                        .into(),
                        border: iced::Border::default(),
                    });
                }
                result.push(part);
            }
            offset = next;
        }
        result.into()
    }

    fn hit(&self, state: &State, position: Point) -> usize {
        hit(&state.paragraph, &self.plain, position)
    }
}

fn hit(paragraph: &ParagraphBuffer, plain: &str, position: Point) -> usize {
    if position.y < 0.0 {
        return 0;
    }
    if position.y >= paragraph.min_bounds().height {
        return plain.len();
    }
    let buffer = paragraph.buffer();
    let Some(cursor) = buffer.hit(position.x, position.y) else {
        return if position.x <= 0.0 { 0 } else { plain.len() };
    };
    // Iced's paragraph Hit omits the line number. Code blocks and hard
    // breaks need the cursor's full byte offset in the original source.
    let prefix: usize = buffer
        .lines
        .iter()
        .take(cursor.line)
        .map(|line| line.text().len() + line.ending().as_str().len())
        .sum();
    (prefix + cursor.index).min(plain.len())
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Selectable {
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        _cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        use iced_accessibility::accesskit::{Node, Rect, Role};
        let mut bounds = layout.bounds();
        bounds.x -= layout.virtual_offset().x;
        bounds.y -= layout.virtual_offset().y;
        let mut node = Node::new(Role::Label);
        node.set_value(self.plain.to_string());
        node.set_bounds(Rect::new(
            bounds.x as f64,
            bounds.y as f64,
            (bounds.x + bounds.width) as f64,
            (bounds.y + bounds.height) as f64,
        ));
        iced_accessibility::A11yTree::leaf(node, tree.state.downcast_ref::<State>().a11y_id.clone())
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.child)]
    }
    fn diff(&mut self, _tree: &mut Tree) {}
    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<State>();
        state.dirty = false;
        state.target = Some(self.target);
        if state
            .spans
            .as_ref()
            .is_none_or(|previous| !Arc::ptr_eq(previous, &self.spans))
        {
            if !self.plain.starts_with(state.plain.as_ref()) {
                state.identity = NEXT_SELECTION.fetch_add(1, Ordering::Relaxed);
                state.anchor = 0;
                state.end = 0;
                state.dragging = false;
            }
            state.plain = self.plain.clone();
        }
        // Selection bytes cannot outlive a streamed paragraph's replacement.
        state.validate_selection(&self.plain);
        let description = iced::advanced::Text {
            content: self.spans.as_ref(),
            bounds: limits.max(),
            size: self.size,
            line_height: Default::default(),
            font: self.font,
            align_x: Default::default(),
            align_y: iced::alignment::Vertical::Top,
            shaping: iced::widget::text::Shaping::Advanced,
            wrapping: Wrapping::default(),
            ellipsize: Default::default(),
        };
        if state
            .spans
            .as_ref()
            .is_none_or(|previous| !Arc::ptr_eq(previous, &self.spans))
        {
            state.paragraph = ParagraphBuffer::with_spans(description);
            state.spans = Some(self.spans.clone());
        } else {
            match state.paragraph.compare(description.with_content(())) {
                iced::advanced::text::Difference::None => {}
                iced::advanced::text::Difference::Bounds => {
                    state.paragraph.resize(description.bounds)
                }
                iced::advanced::text::Difference::Shape => {
                    state.paragraph = ParagraphBuffer::with_spans(description)
                }
            }
        }
        self.child = iced::widget::rich_text(self.selected_spans(state))
            .size(self.size)
            .font(self.font)
            .into();
        tree.children[0].diff(&mut self.child);
        self.child
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &cosmic::Renderer,
        operation: &mut dyn iced::advanced::widget::Operation,
    ) {
        operation.custom(None, layout.bounds(), tree.state.downcast_mut::<State>());
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        defaults: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.child.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            defaults,
            layout,
            cursor,
            viewport,
        );
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        if std::mem::take(&mut state.dirty) {
            shell.invalidate_layout();
        }
        if state.managed {
            return;
        }
        if matches!(event, Event::Window(iced::window::Event::Unfocused)) {
            state.dragging = false;
        }
        state.validate_selection(&self.plain);
        let position = cursor.position_in(layout.bounds());
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                state.focused = position.is_some() && cursor.is_over(*viewport);
                if state.focused {
                    ACTIVE_SELECTION.store(state.identity, Ordering::Relaxed);
                    state.anchor = self.hit(state, position.unwrap());
                    state.end = state.anchor;
                    state.dragging = true;
                    shell.capture_event();
                    shell.invalidate_layout();
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.dragging => {
                if let Some(position) = cursor.position() {
                    state.end = self.hit(
                        state,
                        position - iced::Vector::new(layout.bounds().x, layout.bounds().y),
                    );
                    shell.capture_event();
                    shell.invalidate_layout();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.dragging => {
                state.dragging = false;
                if let Some(position) = cursor.position() {
                    state.end = self.hit(
                        state,
                        position - iced::Vector::new(layout.bounds().x, layout.bounds().y),
                    );
                }
                if state.anchor == state.end
                    && let Some(position) = position
                    && let Some(span) = state.paragraph.hit_span(position)
                    && let Some(link) = self.spans.get(span).and_then(|span| span.link.clone())
                {
                    shell.publish(action(Action::OpenDocument(self.target, link)));
                }
                shell.capture_event();
                shell.invalidate_layout();
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(key),
                modifiers,
                ..
            }) if state.focused
                && ACTIVE_SELECTION.load(Ordering::Relaxed) == state.identity
                && modifiers.command() =>
            {
                if key.eq_ignore_ascii_case("c") && state.anchor != state.end {
                    clipboard.write(
                        iced::advanced::clipboard::Kind::Standard,
                        self.plain[state.anchor.min(state.end)..state.anchor.max(state.end)]
                            .to_owned(),
                    );
                    shell.capture_event();
                } else if key.eq_ignore_ascii_case("a") {
                    state.anchor = 0;
                    state.end = self.plain.len();
                    shell.capture_event();
                    shell.invalidate_layout();
                }
            }
            _ => {}
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        _renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(*viewport)
            && let Some(position) = cursor.position_in(layout.bounds())
        {
            let state = tree.state.downcast_ref::<State>();
            if state
                .paragraph
                .hit_span(position)
                .and_then(|index| self.spans.get(index))
                .is_some_and(|span| span.link.is_some())
            {
                mouse::Interaction::Pointer
            } else {
                mouse::Interaction::Text
            }
        } else {
            mouse::Interaction::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prose_exposes_full_text_and_stable_accessible_identity() {
        let element = text(
            vec![Span::new("α first\nβ second")].into(),
            design::TEXT_PROSE.into(),
            iced::Font::default(),
            Target {
                card: CardId::new(),
                generation: 0,
            },
            None,
        );
        let tree = Tree::new(&element);
        let node = layout::Node::new(Size::new(80.0, 40.0));
        let first =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let second =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        assert_eq!(first.root()[0].id(), second.root()[0].id());
        assert_eq!(first.root()[0].node().value(), Some("α first\nβ second"));
        assert_eq!(
            first.root()[0].node().role(),
            iced_accessibility::accesskit::Role::Label
        );
    }

    #[test]
    fn shaped_hit_testing_keeps_multiline_offsets_and_outside_drag_direction() {
        let spans: Arc<[Span<'static, String>]> = vec![Span::new("α first\nβ second")].into();
        let selectable = Selectable {
            plain: "α first\nβ second".into(),
            spans: spans.clone(),
            matches: vec![],
            size: design::TEXT_PROSE.into(),
            font: iced::Font::default(),
            target: Target {
                card: CardId::new(),
                generation: 0,
            },
            child: widget::text("").into(),
        };
        let state = State {
            paragraph: ParagraphBuffer::with_spans(iced::advanced::Text {
                content: spans.as_ref(),
                bounds: Size::new(300.0, 200.0),
                size: selectable.size,
                line_height: Default::default(),
                font: selectable.font,
                align_x: Default::default(),
                align_y: iced::alignment::Vertical::Top,
                shaping: iced::widget::text::Shaping::Advanced,
                wrapping: Wrapping::default(),
                ellipsize: Default::default(),
            }),
            ..Default::default()
        };
        let second = state
            .paragraph
            .buffer()
            .layout_runs()
            .find(|run| run.line_i == 1)
            .unwrap();
        assert_eq!(
            selectable.hit(&state, Point::new(-20.0, second.line_top + 2.0)),
            "α first\n".len()
        );
        assert_eq!(selectable.hit(&state, Point::new(80.0, -20.0)), 0);
        assert_eq!(
            selectable.hit(&state, Point::new(0.0, 400.0)),
            selectable.plain.len()
        );
    }

    #[test]
    fn a_streamed_shrink_invalidates_selection_before_keyboard_copy() {
        let mut state = State {
            anchor: 3,
            end: 40,
            dragging: true,
            ..Default::default()
        };
        state.validate_selection("new");
        assert_eq!((state.anchor, state.end, state.dragging), (0, 0, false));
    }

    #[test]
    fn selection_preserves_links_fonts_and_unicode_across_span_boundaries() {
        let spans: Arc<[Span<'static, String>]> = vec![
            Span::new("before α").font(iced::Font::MONOSPACE),
            Span::new("β linked").link("src/app.rs:42"),
        ]
        .into();
        let selectable = Selectable {
            plain: "before αβ linked".into(),
            spans,
            matches: vec![],
            size: design::TEXT_PROSE.into(),
            font: iced::Font::default(),
            target: Target {
                card: CardId::new(),
                generation: 0,
            },
            child: widget::text("").into(),
        };
        let state = State {
            anchor: "before ".len(),
            end: "before αβ".len(),
            ..State::default()
        };
        let selected = selectable.selected_spans(&state);
        assert_eq!(
            selected
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<String>(),
            selectable.plain.as_ref()
        );
        assert_eq!(
            selected
                .iter()
                .filter(|span| span.highlight.is_some())
                .map(|span| span.text.as_ref())
                .collect::<String>(),
            "αβ"
        );
        assert!(
            selected
                .iter()
                .any(|span| span.link.as_deref() == Some("src/app.rs:42"))
        );
        assert_eq!(selected[0].font, Some(iced::Font::MONOSPACE));
    }

    #[test]
    fn find_marks_only_the_matching_text() {
        let selectable = Selectable {
            plain: "one needle two".into(),
            spans: vec![Span::new("one needle two")].into(),
            matches: vec![4..10],
            size: design::TEXT_PROSE.into(),
            font: iced::Font::default(),
            target: Target {
                card: CardId::new(),
                generation: 0,
            },
            child: widget::text("").into(),
        };
        let spans = selectable.selected_spans(&State::default());
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.highlight.is_some())
                .map(|span| span.text.as_ref())
                .collect::<String>(),
            "needle"
        );
    }
}
