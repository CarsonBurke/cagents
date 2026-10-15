//! Owned selectable rich text: selection uses the same shaped paragraph as prose.
use super::*;
use iced::advanced::text::{Paragraph, Renderer as TextRenderer};
use iced::advanced::Renderer as _;
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
    pub(super) logical: Option<Logical>,
    pub(super) logical_active: bool,
    a11y_id: iced::advanced::widget::Id,
    pub(super) plain: Arc<str>,
    pub(super) target: Option<Target>,
    pub(super) managed: bool,
    pub(super) dirty: bool,
    pub(super) spans: Option<Arc<[Span<'static, String>]>>,
    /// How the spans were last laid out, which is what they are shaped as
    /// when something asks where a point falls among them.
    shape: Option<Shape>,
    /// One shaped paragraph shared by painting, links and selection hits.
    /// Selection/find backgrounds are painted from glyph positions and never
    /// change the paragraph's spans or trigger reshaping.
    paragraph: std::cell::OnceCell<ParagraphBuffer>,
    #[cfg(test)]
    paragraph_builds: std::cell::Cell<usize>,
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
            logical: None,
            logical_active: false,
            plain: Arc::from(""),
            target: None,
            managed: false,
            dirty: false,
            spans: None,
            shape: None,
            paragraph: std::cell::OnceCell::new(),
            #[cfg(test)]
            paragraph_builds: std::cell::Cell::new(0),
            anchor: 0,
            end: 0,
            dragging: false,
            focused: false,
        }
    }
}

/// The room, size and font a block's spans are laid out with.
#[derive(Clone, Copy)]
struct Shape {
    bounds: Size,
    size: iced::Pixels,
    font: iced::Font,
}

impl Shape {
    fn text<Content>(self, content: Content) -> iced::advanced::Text<Content, iced::Font> {
        iced::advanced::Text {
            content,
            bounds: self.bounds,
            size: self.size,
            line_height: Default::default(),
            font: self.font,
            align_x: Default::default(),
            align_y: iced::alignment::Vertical::Top,
            shaping: iced::widget::text::Shaping::Advanced,
            wrapping: Wrapping::WordOrGlyph,
            ellipsize: Default::default(),
        }
    }
}

impl State {
    fn paragraph(&self) -> &ParagraphBuffer {
        self.paragraph
            .get_or_init(|| match (&self.spans, self.shape) {
                (Some(spans), Some(shape)) => {
                    #[cfg(test)]
                    self.paragraph_builds.set(self.paragraph_builds.get() + 1);
                    ParagraphBuffer::with_spans(shape.text(spans.as_ref()))
                }
                _ => ParagraphBuffer::default(),
            })
    }

    /// A mounted glyph cluster containing a logical byte. This inspects the
    /// existing bounded paragraph and never shapes an offscreen source.
    pub(super) fn logical_byte_bounds(&self, identity: u64, byte: usize) -> Option<Rectangle> {
        let logical = self.logical.as_ref()?;
        if logical.identity != identity || byte < logical.start { return None; }
        let byte = byte - logical.start;
        if byte > self.plain.len() { return None; }
        let paragraph = self.paragraph();
        let mut prefixes = Vec::with_capacity(paragraph.buffer().lines.len());
        let mut offset = 0;
        for line in &paragraph.buffer().lines {
            prefixes.push(offset); offset += line.text().len() + line.ending().as_str().len();
        }
        let mut last = None;
        for run in paragraph.buffer().layout_runs() {
            for glyph in run.glyphs {
                let bounds = Rectangle::new(Point::new(glyph.x.min(glyph.x + glyph.w), run.line_top),
                    Size::new(glyph.w.abs(), run.line_height));
                if prefixes[run.line_i] + glyph.start <= byte && byte < prefixes[run.line_i] + glyph.end { return Some(bounds); }
                last = Some(bounds);
            }
        }
        (byte == self.plain.len()).then_some(last).flatten()
    }

    pub(super) fn hit(&self, position: Point) -> usize {
        hit(self.paragraph(), &self.plain, position)
    }

    pub(super) fn link(&self, position: Point) -> Option<String> {
        // Link activation uses the glyph under the pointer, rather than a
        // caret offset: iced's hit_span rejects line.len() even with Before
        // affinity, losing the right half of the last linked glyph.
        let index = self.paragraph().buffer().layout_runs().find_map(|run| {
            (position.y >= run.line_top && position.y < run.line_top + run.line_height)
                .then(|| {
                    run.glyphs
                        .iter()
                        .find(|glyph| position.x >= glyph.x && position.x < glyph.x + glyph.w)
                        .map(|glyph| glyph.metadata)
                })
                .flatten()
        })?;
        self.spans.as_ref()?.get(index)?.link.clone()
    }

    fn validate_selection(&mut self, plain: &str) {
        if !plain.is_char_boundary(self.anchor) || !plain.is_char_boundary(self.end) {
            self.anchor = 0;
            self.end = 0;
            self.dragging = false;
        }
    }
}

#[derive(Clone)]
pub(super) struct Logical {
    pub identity: u64,
    pub start: usize,
}

pub(super) fn logical_text(spans: Arc<[Span<'static, String>]>, size: iced::Pixels,
    font: iced::Font, target: Target, find: Option<&str>, logical: Logical) -> Element<'static, Message> {
    build_text(spans, size, font, target, find, Some(logical))
}

pub(super) fn text(
    spans: Arc<[Span<'static, String>]>,
    size: iced::Pixels,
    font: iced::Font,
    target: Target,
    find: Option<&str>,
) -> Element<'static, Message> {
    build_text(spans, size, font, target, find, None)
}
fn build_text(spans: Arc<[Span<'static, String>]>, size: iced::Pixels,
    font: iced::Font, target: Target, find: Option<&str>, logical: Option<Logical>) -> Element<'static, Message> {
    let plain: Arc<str> = spans
        .iter()
        .map(|span| span.text.as_ref())
        .collect::<String>()
        .into();
    let highlights = super::find::highlights(&plain, find.unwrap_or_default());
    let truncated = highlights.truncated;
    let body = Element::new(Selectable {
        spans,
        plain,
        size,
        font,
        target,
        logical,
        matches: highlights.ranges,
    });
    if truncated {
        widget::column::with_children(vec![
            design::caption("Find highlights limited to the first 4096 matches in this block.")
                .into(),
            body,
        ])
        .width(iced::Length::Fill)
        .into()
    } else {
        body
    }
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
    logical: Option<Logical>,
    matches: Vec<std::ops::Range<usize>>,
    spans: Arc<[Span<'static, String>]>,
    plain: Arc<str>,
    size: iced::Pixels,
    font: iced::Font,
    target: Target,
}

impl Selectable {
    #[cfg(test)]
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
            let first = self.matches.partition_point(|found| found.end <= offset);
            let last = self.matches.partition_point(|found| found.start < next);
            let matching = &self.matches[first..last];
            for found in matching {
                boundaries.push(found.start.clamp(offset, next) - offset);
                boundaries.push(found.end.clamp(offset, next) - offset);
            }
            boundaries.sort_unstable();
            boundaries.dedup();
            for pair in boundaries.windows(2) {
                let range = pair[0]..pair[1];
                let selected = range.start >= lo && range.end <= hi && lo != hi;
                let position = offset + range.start;
                let candidate = matching.partition_point(|found| found.end <= position);
                let found = matching
                    .get(candidate)
                    .is_some_and(|found| found.contains(&position));
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
        hit(state.paragraph(), &self.plain, position)
    }
}

/// Paint byte-range backgrounds against the existing shaped glyph runs. This
/// is independent of syntax span count and never reshapes selected text.
fn paint_ranges(renderer: &mut cosmic::Renderer, paragraph: &ParagraphBuffer,
    ranges: &[std::ops::Range<usize>], translation: iced::Vector, color: iced::Color) {
    visit_range_bounds(paragraph, ranges, |bounds| {
        renderer.fill_quad(renderer::Quad { bounds: Rectangle { x: bounds.x + translation.x,
            y: bounds.y + translation.y, ..bounds }, ..Default::default() }, color);
    });
}

fn visit_range_bounds(paragraph: &ParagraphBuffer, ranges: &[std::ops::Range<usize>],
    mut paint: impl FnMut(Rectangle)) {
    if ranges.is_empty() { return; }
    let mut starts = Vec::with_capacity(paragraph.buffer().lines.len());
    let mut offset = 0;
    for line in &paragraph.buffer().lines {
        starts.push(offset);
        offset += line.text().len() + line.ending().as_str().len();
    }
    for run in paragraph.buffer().layout_runs() {
        let prefix = starts[run.line_i];
        let mut bounds: Option<(f32, f32)> = None;
        let mut flush = |bounds: &mut Option<(f32, f32)>| {
            if let Some((left, right)) = bounds.take() {
                paint(Rectangle::new(Point::new(left, run.line_top),
                    Size::new(right - left, run.line_height)));
            }
        };
        for glyph in run.glyphs {
            let start = prefix + glyph.start;
            let end = prefix + glyph.end;
            let found = ranges.partition_point(|range| range.end <= start);
            if ranges.get(found).is_some_and(|range| range.start < end) {
                let left = glyph.x.min(glyph.x + glyph.w);
                let right = glyph.x.max(glyph.x + glyph.w);
                if let Some((lo, hi)) = bounds.as_mut() { *lo = lo.min(left); *hi = hi.max(right); }
                else { bounds = Some((left, right)); }
            } else { flush(&mut bounds); }
        }
        flush(&mut bounds);
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
    fn children(&self) -> Vec<Tree> { vec![] }
    fn diff(&mut self, _tree: &mut Tree) {}
    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        _renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<State>();
        state.dirty = false;
        state.target = Some(self.target);
        state.logical = self.logical.clone();
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
        let shape = Shape {
            bounds: limits.max(),
            size: self.size,
            font: self.font,
        };
        if state
            .spans
            .as_ref()
            .is_none_or(|previous| !Arc::ptr_eq(previous, &self.spans))
        {
            state.paragraph.take();
            state.spans = Some(self.spans.clone());
        } else if let Some(paragraph) = state.paragraph.get_mut() {
            match paragraph.compare(shape.text(())) {
                iced::advanced::text::Difference::None => {}
                iced::advanced::text::Difference::Bounds => paragraph.resize(shape.bounds),
                iced::advanced::text::Difference::Shape => {
                    state.paragraph.take();
                }
            }
        }
        state.shape = Some(shape);
        layout::Node::new(limits.resolve(Length::Shrink, Length::Shrink, state.paragraph().min_bounds()))
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
        if layout.bounds().intersection(viewport).is_none() { return; }
        let state = tree.state.downcast_ref::<State>();
        let paragraph = state.paragraph();
        let translation = layout.position() - Point::ORIGIN;
        // Preserve inline code surfaces, emphasis, links and strike-throughs
        // without allocating a second rich-text widget or shaped buffer.
        for (index, span) in self.spans.iter().enumerate() {
            if span.highlight.is_none() && !span.underline && !span.strikethrough { continue; }
            for bounds in paragraph.span_bounds(index) {
                if let Some(highlight) = span.highlight {
                    renderer.fill_quad(renderer::Quad {
                        bounds: Rectangle::new(bounds.position() - iced::Vector::new(span.padding.left, span.padding.top),
                            bounds.size() + Size::new(span.padding.x(), span.padding.y())) + translation,
                        border: highlight.border, ..Default::default()
                    }, highlight.background);
                }
                let size = span.size.unwrap_or(self.size);
                let line_height = span.line_height.unwrap_or_default().to_absolute(size).0;
                let baseline = size.0 + (line_height - size.0) / 2.0;
                for (draw, shift) in [(span.underline, size.0 * 0.08), (span.strikethrough, size.0 / 2.0)] {
                    if draw {
                        renderer.fill_quad(renderer::Quad {
                            bounds: Rectangle::new(bounds.position() + translation + iced::Vector::new(0.0, baseline - shift),
                                Size::new(bounds.width, 1.0)), ..Default::default()
                        }, span.color.unwrap_or(defaults.text_color));
                    }
                }
            }
        }
        paint_ranges(renderer, paragraph, &self.matches, translation, design::accent_wash(theme));
        let selection = state.anchor.min(state.end)..state.anchor.max(state.end);
        if !selection.is_empty() {
            paint_ranges(renderer, paragraph, &[selection], translation, design::accent_color(theme));
        }
        renderer.fill_paragraph(paragraph, layout.position(), defaults.text_color, *viewport);
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
                    && let Some(span) = state.paragraph().hit_span(position)
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
            if state.link(position).is_some()
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
        let scrolled = Layout::with_offset(iced::Vector::new(12.0, 50.0), &node)
            .with_virtual_offset(iced::Vector::new(0.0, 37.0));
        let first = element
            .as_widget()
            .a11y_nodes(scrolled, &tree, mouse::Cursor::Unavailable);
        let second = element
            .as_widget()
            .a11y_nodes(scrolled, &tree, mouse::Cursor::Unavailable);
        assert_eq!(first.root()[0].id(), second.root()[0].id());
        let bounds = first.root()[0].node().bounds().unwrap();
        assert_eq!(
            (bounds.x0, bounds.y0, bounds.x1, bounds.y1),
            (12.0, 13.0, 92.0, 53.0)
        );
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
            logical: None,
            plain: "α first\nβ second".into(),
            spans: spans.clone(),
            matches: vec![],
            size: design::TEXT_PROSE.into(),
            font: iced::Font::default(),
            target: Target {
                card: CardId::new(),
                generation: 0,
            },
            };
        let state = State {
            spans: Some(spans),
            shape: Some(Shape {
                bounds: Size::new(300.0, 200.0),
                size: selectable.size,
                font: selectable.font,
            }),
            ..Default::default()
        };
        let second = state
            .paragraph()
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
    fn direct_paint_and_logical_navigation_use_bounded_unicode_glyph_geometry() {
        let renderer = crate::app::tests::headless_renderer();
        let source = "Latin fi e\u{301} αβ العربية text";
        let identity = 0x1234;
        let mut element = logical_text(vec![Span::new(source.to_owned())].into(),
            design::TEXT_PROSE.into(), iced::Font::default(),
            Target { card: CardId::new(), generation: 0 }, None, Logical { identity, start: 100 });
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(&mut tree, &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(150.0, f32::INFINITY)));
        let state = tree.state.downcast_ref::<State>();
        for (byte, ch) in source.char_indices().filter(|(_, ch)| !ch.is_whitespace()) {
            let mut painted = Vec::new();
            visit_range_bounds(state.paragraph(), &[byte..byte + ch.len_utf8()], |bounds| painted.push(bounds));
            assert!(!painted.is_empty(), "every selected grapheme paints its existing cluster");
            for bounds in painted {
                assert!(bounds.x >= -0.5 && bounds.x + bounds.width <=150.5);
                assert!(bounds.y >= -0.5 && bounds.y + bounds.height <= node.size().height +0.5);
            }
            assert!(state.logical_byte_bounds(identity, 100 + byte).is_some());
        }
        assert!(state.logical_byte_bounds(identity, 99).is_none());
        assert!(state.logical_byte_bounds(identity +1, 100).is_none());
        assert_eq!(state.paragraph_builds.get(), 1, "paint/navigation reuse the same paragraph");
    }

    #[test]
    fn selection_reuses_the_paragraph_used_for_drawing_and_hit_testing() {
        let renderer = crate::app::tests::headless_renderer();
        let mut element = text(vec![Span::new("linked α prose ".repeat(100)).link("source.rs".to_owned())].into(),
            design::TEXT_PROSE.into(), iced::Font::default(),
            Target { card: CardId::new(), generation: 0 }, Some("prose"));
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, f32::INFINITY));
        element.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let state = tree.state.downcast_ref::<State>();
        let buffer = state.paragraph().buffer() as *const _ as usize;
        assert_eq!(state.paragraph_builds.get(), 1);
        assert!(tree.children.is_empty(), "no second rich-text paragraph tree");
        for end in [1, 20, 100, 300] {
            let state = tree.state.downcast_mut::<State>();
            state.anchor = 0;
            state.end = end;
            element.as_widget_mut().layout(&mut tree, &renderer, &limits);
            let state = tree.state.downcast_ref::<State>();
            assert_eq!(state.paragraph_builds.get(), 1, "selection does not construct another paragraph");
            assert_eq!(buffer, state.paragraph().buffer() as *const _ as usize,
                "selection changes paint bounds instead of reshaping spans");
        }
    }

    #[test]
    fn long_tokens_wrap_with_accessible_glyphs_and_matching_hit_geometry() {
        let renderer = crate::app::tests::headless_renderer();
        let source = "w".repeat(3000);
        let spans: Arc<[Span<'static, String>]> =
            vec![Span::new(source.clone()).link("https://example.com".to_owned())].into();
        let mut selectable = Selectable {
            plain: source.clone().into(),
            spans,
            matches: vec![],
            size: design::TEXT_PROSE.into(),
            font: iced::Font::default(),
            target: Target {
                card: CardId::new(),
                generation: 0,
            },
            logical: None,
        };
        let mut tree =
            Tree::new(&selectable as &dyn Widget<Message, cosmic::Theme, cosmic::Renderer>);
        let node = selectable.layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(240.0, f32::INFINITY)),
        );
        let state = tree.state.downcast_ref::<State>();
        let runs: Vec<_> = state.paragraph().buffer().layout_runs().collect();
        assert!(
            runs.len() > 20,
            "a long token must wrap to many visible lines"
        );
        for run in &runs {
            for glyph in run.glyphs {
                assert!(
                    glyph.x >= -0.01 && glyph.x + glyph.w <= 240.01,
                    "a token glyph is outside the reading column"
                );
            }
        }
        let last = runs.last().unwrap();
        let last_glyph = last.glyphs.last().unwrap();
        let position = Point::new(last_glyph.x + last_glyph.w * 0.5, last.line_top + 2.0);
        assert!(
            state.hit(position) > source.len() - 32,
            "the tail remains hit-testable"
        );
        for fraction in [0.25, 0.75] {
            let point = Point::new(last_glyph.x + last_glyph.w * fraction, position.y);
            assert_eq!(state.link(point).as_deref(), Some("https://example.com"));
        }
        assert_eq!(
            state.link(Point::new(last_glyph.x + last_glyph.w + 2.0, position.y)),
            None
        );
        assert!(
            (node.size().height - state.paragraph().min_bounds().height).abs() < 0.1,
            "draw and hit-test paragraphs must have the same wrapped height"
        );
        assert_eq!(state.plain.as_ref(), source);
    }

    #[test]
    fn link_hit_uses_glyph_metadata_across_wrapping_and_bidi_boundaries() {
        let spans: Arc<[Span<'static, String>]> = vec![
            Span::new("linked ".repeat(12)).link("first".to_owned()),
            Span::new(" plain "),
            Span::new("שלום".to_owned()).link("rtl".to_owned()),
        ]
        .into();
        let state = State {
            plain: spans
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<String>()
                .into(),
            spans: Some(spans.clone()),
            shape: Some(Shape {
                bounds: Size::new(110.0, f32::INFINITY),
                size: design::TEXT_PROSE.into(),
                font: iced::Font::default(),
            }),
            ..Default::default()
        };
        let mut checked = [false; 3];
        let mut bottom = 0.0_f32;
        for run in state.paragraph().buffer().layout_runs() {
            bottom = bottom.max(run.line_top + run.line_height);
            for glyph in run.glyphs {
                checked[glyph.metadata] = true;
                for fraction in [0.25, 0.75] {
                    let point = Point::new(
                        glyph.x + glyph.w * fraction,
                        run.line_top + run.line_height * 0.5,
                    );
                    assert_eq!(state.link(point), spans[glyph.metadata].link);
                }
            }
            assert_eq!(state.link(Point::new(-2.0, run.line_top + 2.0)), None);
            assert_eq!(state.link(Point::new(112.0, run.line_top + 2.0)), None);
        }
        assert_eq!(checked, [true; 3]);
        assert_eq!(state.link(Point::new(10.0, bottom + 2.0)), None);
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
            logical: None,
            plain: "before αβ linked".into(),
            spans,
            matches: vec![],
            size: design::TEXT_PROSE.into(),
            font: iced::Font::default(),
            target: Target {
                card: CardId::new(),
                generation: 0,
            },
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
    fn unicode_find_highlights_original_text_across_syntax_spans() {
        let selectable = Selectable {
            logical: None,
            plain: "Straße CAFÉ".into(),
            spans: vec![Span::new("Stra"), Span::new("ße"), Span::new(" CAFÉ")].into(),
            matches: super::super::find::ranges("Straße CAFÉ", "STRASSE"),
            size: design::TEXT_PROSE.into(),
            font: iced::Font::default(),
            target: Target {
                card: CardId::new(),
                generation: 0,
            },
            };
        assert_eq!(
            selectable
                .selected_spans(&State::default())
                .iter()
                .filter(|span| span.highlight.is_some())
                .map(|span| span.text.as_ref())
                .collect::<String>(),
            "Straße"
        );
    }

    #[test]
    fn find_marks_only_the_matching_text() {
        let selectable = Selectable {
            logical: None,
            plain: "one needle two".into(),
            spans: vec![Span::new("one needle two")].into(),
            matches: vec![4..10],
            size: design::TEXT_PROSE.into(),
            font: iced::Font::default(),
            target: Target {
                card: CardId::new(),
                generation: 0,
            },
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
