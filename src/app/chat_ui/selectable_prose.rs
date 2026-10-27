//! Owned selectable rich text: selection uses the same shaped paragraph as prose.
use super::*;
use iced::advanced::Renderer as _;
use iced::advanced::text::{Paragraph, Renderer as TextRenderer};
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, renderer,
    widget::{Tree, tree},
};
use iced::widget::text::{Span, Wrapping};
use iced::{Event, Point, Rectangle, Size};
use unicode_segmentation::UnicodeSegmentation;

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
    pub(super) spans: Option<Arc<[Span<'static, markdown::Uri>]>>,
    /// How the spans were last laid out, which is what they are shaped as
    /// when something asks where a point falls among them.
    shape: Option<Shape>,
    /// One shaped paragraph shared by painting, links and selection hits.
    /// Selection/find backgrounds are painted from glyph positions and never
    /// change the paragraph's spans or trigger reshaping.
    paragraph: std::cell::OnceCell<ParagraphBuffer>,
    flow: Option<flow_layout::Handle>,
    flow_ready: Option<Arc<flow_layout::Prepared>>,
    flow_projection: std::cell::RefCell<
        Option<(
            std::sync::Weak<flow_layout::Prepared>,
            [u32; 4],
            flow_layout::Projection,
        )>,
    >,
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
            flow: None,
            flow_ready: None,
            flow_projection: std::cell::RefCell::new(None),
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
    #[cfg(test)]
    pub(super) fn flow_readiness(&self) -> Option<bool> {
        self.flow.as_ref().map(|_| self.flow_ready.is_some())
    }
    fn paragraph(&self) -> &ParagraphBuffer {
        debug_assert!(
            self.flow.is_none(),
            "flow text must never shape synchronously"
        );
        self.paragraph
            .get_or_init(|| match (&self.spans, self.shape) {
                (Some(spans), Some(shape)) => {
                    let _stage = chat_stage::scope(Stage::Shape);
                    #[cfg(test)]
                    self.paragraph_builds.set(self.paragraph_builds.get() + 1);
                    ParagraphBuffer::with_spans(shape.text(spans.as_ref()))
                }
                _ => ParagraphBuffer::default(),
            })
    }

    pub(super) fn empty_line_caret(&self) -> bool {
        self.logical.is_some()
            && self.flow.is_none()
            && matches!(self.plain.as_ref(), "" | "\n" | "\r\n")
    }

    /// Only the matching mounted source may make Find wait for a real Flow
    /// completion; unrelated paragraphs and offscreen estimates cannot do so.
    pub(super) fn logical_geometry_pending(
        &self,
        target: Target,
        identity: u64,
        byte: usize,
    ) -> Option<flow_layout::PendingGeometry> {
        let logical = self.logical.as_ref()?;
        if self.target != Some(target) || logical.identity != identity || byte < logical.start {
            return None;
        }
        self.flow
            .as_ref()?
            .borrow()
            .geometry_pending(byte - logical.start)
    }

    /// A mounted glyph cluster containing a logical byte. This inspects the
    /// existing bounded paragraph and never shapes an offscreen source.
    pub(super) fn logical_byte_bounds(&self, identity: u64, byte: usize) -> Option<Rectangle> {
        let logical = self.logical.as_ref()?;
        if logical.identity != identity || byte < logical.start {
            return None;
        }
        let byte = byte - logical.start;
        if byte > self.plain.len() {
            return None;
        }
        if self.flow.is_some() {
            return self
                .flow_ready
                .as_ref()?
                .byte_bounds(byte, self.plain.len());
        }
        let paragraph = self.paragraph();
        if self.empty_line_caret() {
            return Some(Rectangle::with_size(Size::new(
                1.0,
                paragraph.min_bounds().height.max(
                    iced::widget::text::LineHeight::default()
                        .to_absolute(self.shape?.size)
                        .0,
                ),
            )));
        }
        let mut prefixes = Vec::with_capacity(paragraph.buffer().lines.len());
        let mut offset = 0;
        for line in &paragraph.buffer().lines {
            prefixes.push(offset);
            offset += line.text().len() + line.ending().as_str().len();
        }
        let mut last = None;
        for run in paragraph.buffer().layout_runs() {
            for glyph in run.glyphs {
                let bounds = Rectangle::new(
                    Point::new(glyph.x.min(glyph.x + glyph.w), run.line_top),
                    Size::new(glyph.w.abs(), run.line_height),
                );
                if prefixes[run.line_i] + glyph.start <= byte
                    && byte < prefixes[run.line_i] + glyph.end
                {
                    return Some(bounds);
                }
                last = Some(bounds);
            }
        }
        (byte == self.plain.len()).then_some(last).flatten()
    }

    pub(super) fn hit(&self, position: Point) -> usize {
        if self.flow.is_some() {
            return self
                .flow_ready
                .as_ref()
                .map_or(0, |ready| ready.hit(position, self.plain.len()));
        }
        hit(self.paragraph(), &self.plain, position)
    }

    pub(super) fn link(&self, position: Point) -> Option<markdown::Uri> {
        if self.flow.is_some() {
            let metadata = self.flow_ready.as_ref()?.link(position)?;
            return self.spans.as_ref()?.get(metadata)?.link.clone();
        }
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

pub(super) fn logical_text(
    spans: Arc<[Span<'static, markdown::Uri>]>,
    size: iced::Pixels,
    font: iced::Font,
    target: Target,
    find: Option<&str>,
    logical: Logical,
) -> Element<'static, Message> {
    build_text(spans, size, font, target, find, Some(logical), None)
}

pub(super) fn text(
    spans: Arc<[Span<'static, markdown::Uri>]>,
    size: iced::Pixels,
    font: iced::Font,
    target: Target,
    find: Option<&str>,
) -> Element<'static, Message> {
    build_text(spans, size, font, target, find, None, None)
}
pub(super) fn flow_source(
    size: iced::Pixels,
    font: iced::Font,
    target: Target,
    find: Option<&str>,
    logical: Logical,
    flow: flow_layout::Handle,
) -> Element<'static, Message> {
    flow_source_optional(size, font, target, find, Some(logical), flow)
}
pub(super) fn flow_source_optional(
    size: iced::Pixels,
    font: iced::Font,
    target: Target,
    find: Option<&str>,
    logical: Option<Logical>,
    flow: flow_layout::Handle,
) -> Element<'static, Message> {
    let ready = flow.borrow().prepared();
    let spans = ready
        .as_ref()
        .map_or_else(|| Arc::from([]), |ready| ready.spans.clone());
    let plain: Arc<str> = ready
        .as_ref()
        .map_or_else(|| Arc::from(""), |ready| ready.plain.clone());
    let highlights = if find.is_some() {
        flow.borrow_mut().find_highlights(target)
    } else {
        super::find_navigation::Marks::default()
    };
    let body = Element::new(Selectable {
        find_enabled: find.is_some(),
        logical,
        flow: Some(flow),
        flow_ready: ready,
        matches: highlights,
        spans,
        plain,
        size,
        font,
        target,
    });
    if find.is_some() {
        Element::new(FlowFindNotice {
            body,
            caption: design::caption(
                "Find highlights limited to the first 4096 matches in this block.",
            )
            .into(),
        })
    } else {
        body
    }
}

/// The text element is retained while asynchronous Find completes. The notice
/// therefore follows the exact ready prefix held by its native tree too.
struct FlowFindNotice {
    body: Element<'static, Message>,
    caption: Element<'static, Message>,
}
impl FlowFindNotice {
    fn truncated(&self, tree: &Tree) -> bool {
        let state = tree.children[0].state.downcast_ref::<State>();
        state
            .flow
            .as_ref()
            .zip(state.flow_ready.as_ref())
            .is_some_and(|(flow, ready)| {
                state.target.is_some_and(|target| {
                    flow.borrow_mut()
                        .find_highlights_for(target, &ready.plain)
                        .truncated()
                })
            })
    }
}
impl Widget<Message, cosmic::Theme, cosmic::Renderer> for FlowFindNotice {
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        let mut children = layout.children();
        let body = self.body.as_widget().a11y_nodes(
            children
                .next()
                .unwrap()
                .with_virtual_offset(layout.virtual_offset()),
            &tree.children[0],
            cursor,
        );
        if *tree.state.downcast_ref::<bool>() && self.truncated(tree) {
            iced_accessibility::A11yTree::join(
                [
                    body,
                    self.caption.as_widget().a11y_nodes(
                        children
                            .next()
                            .unwrap()
                            .with_virtual_offset(layout.virtual_offset()),
                        &tree.children[1],
                        cursor,
                    ),
                ]
                .into_iter(),
            )
        } else {
            body
        }
    }
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<bool>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(false)
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.body), Tree::new(&self.caption)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.children[0].diff(&mut self.body);
        tree.children[1].diff(&mut self.caption);
    }
    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let body = self
            .body
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let shown = self.truncated(tree);
        *tree.state.downcast_mut::<bool>() = shown;
        let caption = if shown {
            self.caption.as_widget_mut().layout(
                &mut tree.children[1],
                renderer,
                &layout::Limits::new(
                    Size::ZERO,
                    Size::new(
                        limits.max().width,
                        (limits.max().height - body.size().height).max(0.0),
                    ),
                ),
            )
        } else {
            layout::Node::new(Size::ZERO)
        };
        let height = caption.size().height;
        let body_height = body.size().height;
        let size = limits.resolve(
            Length::Shrink,
            Length::Shrink,
            Size::new(
                body.size().width.max(caption.size().width),
                body_height + height,
            ),
        );
        layout::Node::with_children(
            size,
            vec![body, caption.move_to(Point::new(0.0, body_height))],
        )
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
        let mut children = layout.children();
        self.body.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            defaults,
            children
                .next()
                .unwrap()
                .with_virtual_offset(layout.virtual_offset()),
            cursor,
            viewport,
        );
        if *tree.state.downcast_ref::<bool>() && self.truncated(tree) {
            self.caption.as_widget().draw(
                &tree.children[1],
                renderer,
                theme,
                defaults,
                children
                    .next()
                    .unwrap()
                    .with_virtual_offset(layout.virtual_offset()),
                cursor,
                viewport,
            );
        }
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.body.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout
                .children()
                .next()
                .unwrap()
                .with_virtual_offset(layout.virtual_offset()),
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if self.truncated(tree) != *tree.state.downcast_ref::<bool>() {
            shell.invalidate_layout();
        }
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn iced::advanced::widget::Operation,
    ) {
        self.body.as_widget_mut().operate(
            &mut tree.children[0],
            layout
                .children()
                .next()
                .unwrap()
                .with_virtual_offset(layout.virtual_offset()),
            renderer,
            operation,
        );
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.body.as_widget().mouse_interaction(
            &tree.children[0],
            layout
                .children()
                .next()
                .unwrap()
                .with_virtual_offset(layout.virtual_offset()),
            cursor,
            viewport,
            renderer,
        )
    }
}

#[cfg(test)]
pub(super) fn flow_text(
    spans: Arc<[Span<'static, markdown::Uri>]>,
    size: iced::Pixels,
    font: iced::Font,
    target: Target,
    find: Option<&str>,
    logical: Logical,
    flow: flow_layout::Handle,
) -> Element<'static, Message> {
    build_text(spans, size, font, target, find, Some(logical), Some(flow))
}

fn build_text(
    spans: Arc<[Span<'static, markdown::Uri>]>,
    size: iced::Pixels,
    font: iced::Font,
    target: Target,
    find: Option<&str>,
    logical: Option<Logical>,
    flow: Option<flow_layout::Handle>,
) -> Element<'static, Message> {
    let plain: Arc<str> = spans
        .iter()
        .map(|span| span.text.as_ref())
        .collect::<String>()
        .into();
    let highlights = if find.is_some() {
        super::find_navigation::inline_highlights(target, &plain)
    } else {
        super::find_navigation::Marks::default()
    };
    let truncated = highlights.truncated();
    let flow_ready = flow.as_ref().and_then(|flow| flow.borrow().prepared());
    let body = Element::new(Selectable {
        find_enabled: find.is_some(),
        flow_ready,
        spans,
        plain,
        size,
        font,
        target,
        logical,
        flow,
        matches: highlights,
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
    find_enabled: bool,
    flow: Option<flow_layout::Handle>,
    flow_ready: Option<Arc<flow_layout::Prepared>>,
    logical: Option<Logical>,
    matches: super::find_navigation::Marks,
    spans: Arc<[Span<'static, markdown::Uri>]>,
    plain: Arc<str>,
    size: iced::Pixels,
    font: iced::Font,
    target: Target,
}

impl Selectable {
    #[cfg(test)]
    fn selected_spans(&self, state: &State) -> Arc<[Span<'static, markdown::Uri>]> {
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
        state.hit(position)
    }
}

/// Paint byte-range backgrounds against the existing shaped glyph runs. This
/// is independent of syntax span count and never reshapes selected text.
fn paint_ranges(
    renderer: &mut cosmic::Renderer,
    paragraph: &ParagraphBuffer,
    ranges: &[std::ops::Range<usize>],
    translation: iced::Vector,
    color: iced::Color,
) {
    visit_range_bounds(paragraph, ranges, |bounds| {
        renderer.fill_quad(
            renderer::Quad {
                bounds: Rectangle {
                    x: bounds.x + translation.x,
                    y: bounds.y + translation.y,
                    ..bounds
                },
                ..Default::default()
            },
            color,
        );
    });
}

fn visit_range_bounds(
    paragraph: &ParagraphBuffer,
    ranges: &[std::ops::Range<usize>],
    mut paint: impl FnMut(Rectangle),
) {
    if ranges.is_empty() {
        return;
    }
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
                paint(Rectangle::new(
                    Point::new(left, run.line_top),
                    Size::new(right - left, run.line_height),
                ));
            }
        };
        for glyph in run.glyphs {
            // Buffer::hit divides a shaped cluster into equal-width grapheme
            // regions. Paint those same regions so a partial ligature copies
            // and highlights the same text, without reshaping the paragraph.
            let cluster = &run.text[glyph.start..glyph.end];
            let width = glyph.w / cluster.graphemes(true).count().max(1) as f32;
            for (index, (byte, grapheme)) in cluster.grapheme_indices(true).enumerate() {
                let start = prefix + glyph.start + byte;
                let end = start + grapheme.len();
                let found = ranges.partition_point(|range| range.end <= start);
                if ranges.get(found).is_some_and(|range| range.start < end) {
                    let left = glyph.x + index as f32 * width;
                    let right = left + width;
                    if let Some((lo, hi)) = bounds.as_mut() {
                        *lo = lo.min(left);
                        *hi = hi.max(right);
                    } else {
                        bounds = Some((left, right));
                    }
                } else {
                    flush(&mut bounds);
                }
            }
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
        node.set_value(tree.state.downcast_ref::<State>().plain.to_string());
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
        vec![]
    }
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
        let same_target = state.target == Some(self.target);
        let same_logical = state
            .logical
            .as_ref()
            .map(|logical| (logical.identity, logical.start))
            == self
                .logical
                .as_ref()
                .map(|logical| (logical.identity, logical.start));
        let mut flow = self.flow.clone();
        if same_target
            && same_logical
            && let (Some(previous), Some(current)) = (&state.flow, &self.flow)
            && !std::rc::Rc::ptr_eq(previous, current)
            && previous.borrow_mut().adopt(&current.borrow())
        {
            flow = Some(previous.clone());
        }
        let same_flow = state
            .flow
            .as_ref()
            .zip(flow.as_ref())
            .is_some_and(|(old, current)| std::rc::Rc::ptr_eq(old, current));
        state.target = Some(self.target);
        state.logical = self.logical.clone();
        if !same_flow {
            state.flow_ready = None;
            state.flow_projection.borrow_mut().take();
        }
        state.flow = flow;
        if state
            .spans
            .as_ref()
            .is_none_or(|previous| !Arc::ptr_eq(previous, &self.spans))
        {
            if if self.flow.is_some() {
                !same_flow
            } else {
                !self.plain.starts_with(state.plain.as_ref())
            } {
                state.identity = NEXT_SELECTION.fetch_add(1, Ordering::Relaxed);
                state.anchor = 0;
                state.end = 0;
                state.dragging = false;
            }
            if state.flow.is_none() || !same_flow {
                state.plain = self.plain.clone();
            }
        }
        // Selection bytes cannot outlive a streamed paragraph's replacement.
        if state.flow.is_none() {
            state.validate_selection(&self.plain);
        }
        let shape = Shape {
            bounds: limits.max(),
            size: self.size,
            font: self.font,
        };
        if let Some(flow) = state.flow.clone() {
            state.paragraph.take();
            state.spans = Some(self.spans.clone());
            state.shape = Some(shape);
            let mut flow = flow.borrow_mut();
            flow.configure(shape.bounds.width, self.size, self.font);
            flow.bind_target(self.target);
            state.flow_ready = flow.prepared();
            if let Some(ready) = &state.flow_ready {
                state.plain = ready.plain.clone();
                state.spans = Some(ready.spans.clone());
            }
            state.validate_selection(&state.plain.clone());
            return layout::Node::new(limits.resolve(
                Length::Shrink,
                Length::Shrink,
                flow.bounds(state.plain.len()),
            ));
        }
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
        let mut natural = state.paragraph().min_bounds();
        if state.empty_line_caret() {
            natural.height = natural.height.max(
                iced::widget::text::LineHeight::default()
                    .to_absolute(self.size)
                    .0,
            );
        }
        layout::Node::new(limits.resolve(Length::Shrink, Length::Shrink, natural))
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
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        if layout.bounds().intersection(viewport).is_none() {
            return;
        }
        let state = tree.state.downcast_ref::<State>();
        if let Some(flow) = &state.flow {
            if let Some(ready) = &state.flow_ready {
                let top = (viewport.y - layout.bounds().y).max(0.0);
                // LayoutGlyph exposes placed advances rather than exact ink
                // outlines. Keep four ems of raster overscan on either edge;
                // native font/edge fixtures qualify the supported fonts.
                let overscan = self.size.0 * 4.0;
                let left = viewport.x - layout.bounds().x - overscan;
                let right = viewport.x + viewport.width - layout.bounds().x + overscan;
                let bottom = top + viewport.height + self.size.0 * 1.3;
                let key = (
                    Arc::downgrade(ready),
                    [
                        top.to_bits(),
                        bottom.to_bits(),
                        left.to_bits(),
                        right.to_bits(),
                    ],
                );
                let mut cached = state.flow_projection.borrow_mut();
                if cached
                    .as_ref()
                    .is_none_or(|(owner, bounds, _)| !owner.ptr_eq(&key.0) || *bounds != key.1)
                {
                    *cached = ready
                        .projection_rect(top, bottom, left, right)
                        .map(|projection| (key.0, key.1, projection));
                }
                if let Some((_, _, projection)) = cached.as_ref() {
                    flow_layout::paint(
                        renderer,
                        projection,
                        &ready.spans,
                        &if self.find_enabled {
                            flow.borrow_mut()
                                .find_highlights_for(self.target, &ready.plain)
                        } else {
                            super::find_navigation::Marks::default()
                        },
                        state.anchor.min(state.end)..state.anchor.max(state.end),
                        self.size,
                        layout.position(),
                        theme,
                        defaults,
                        viewport,
                    );
                }
            } else if let Some(shape) = state.shape {
                let label = if flow.borrow().failed() {
                    "Text could not be displayed"
                } else {
                    "Loading…"
                };
                renderer.fill_text(
                    shape.text(label.to_owned()),
                    layout.position(),
                    defaults.text_color,
                    *viewport,
                );
            }
            return;
        }
        let paragraph = state.paragraph();
        let translation = layout.position() - Point::ORIGIN;
        // Preserve inline code surfaces, emphasis, links and strike-throughs
        // without allocating a second rich-text widget or shaped buffer.
        for (index, span) in self.spans.iter().enumerate() {
            if span.highlight.is_none() {
                continue;
            }
            for bounds in paragraph.span_bounds(index) {
                if let Some(highlight) = span.highlight {
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds: Rectangle::new(
                                bounds.position()
                                    - iced::Vector::new(span.padding.left, span.padding.top),
                                bounds.size() + Size::new(span.padding.x(), span.padding.y()),
                            ) + translation,
                            border: highlight.border,
                            ..Default::default()
                        },
                        highlight.background,
                    );
                }
            }
        }
        paint_ranges(
            renderer,
            paragraph,
            &self.matches,
            translation,
            design::accent_wash(theme),
        );
        let selection = state.anchor.min(state.end)..state.anchor.max(state.end);
        if !selection.is_empty() {
            paint_ranges(
                renderer,
                paragraph,
                &[selection],
                translation,
                design::accent_color(theme),
            );
        }
        // Decorations remain visible over opaque selection/Find backgrounds.
        for (index, span) in self.spans.iter().enumerate() {
            if !span.underline && !span.strikethrough {
                continue;
            }
            for bounds in paragraph.span_bounds(index) {
                let size = span.size.unwrap_or(self.size);
                let line_height = span.line_height.unwrap_or_default().to_absolute(size).0;
                let baseline = size.0 + (line_height - size.0) / 2.0;
                for (draw, shift) in [
                    (span.underline, size.0 * 0.08),
                    (span.strikethrough, size.0 / 2.0),
                ] {
                    if draw {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: Rectangle::new(
                                    bounds.position()
                                        + translation
                                        + iced::Vector::new(0.0, baseline - shift),
                                    Size::new(bounds.width, 1.0),
                                ),
                                ..Default::default()
                            },
                            span.color.unwrap_or(defaults.text_color),
                        );
                    }
                }
            }
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
            shell.request_redraw();
        }
        if matches!(
            event,
            Event::Window(iced::window::Event::RedrawRequested(_))
        ) && layout.bounds().intersection(viewport).is_some()
            && let Some(flow) = &state.flow
            && let Some(request) = flow.borrow_mut().request()
        {
            shell.publish(action(Action::FlowRequest(self.target, request)));
        }
        if matches!(
            event,
            Event::Window(iced::window::Event::RedrawRequested(_))
        ) && let Some(flow) = &state.flow
        {
            flow.borrow_mut().set_find_visible(
                self.find_enabled && layout.bounds().intersection(viewport).is_some(),
            );
        }
        if self.find_enabled
            && matches!(
                event,
                Event::Window(iced::window::Event::RedrawRequested(_))
            )
            && layout.bounds().intersection(viewport).is_some()
            && let Some(flow) = &state.flow
            && let Some(id) = flow.borrow_mut().find_request_id(self.target)
        {
            shell.publish(action(Action::FindHighlight(self.target, id)));
        }
        if state.managed {
            return;
        }
        if matches!(event, Event::Window(iced::window::Event::Unfocused)) {
            state.dragging = false;
        }
        let previous = state.anchor.min(state.end)..state.anchor.max(state.end);
        let current_plain = state.plain.clone();
        state.validate_selection(&current_plain);
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
                    && let Some(link) = state.link(position)
                {
                    shell.publish(action(Action::OpenSharedDocument(self.target, link)));
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
                    super::text_copy::invalidate();
                    clipboard.write(
                        iced::advanced::clipboard::Kind::Standard,
                        state.plain[state.anchor.min(state.end)..state.anchor.max(state.end)]
                            .to_owned(),
                    );
                    shell.capture_event();
                } else if key.eq_ignore_ascii_case("a") {
                    state.anchor = 0;
                    state.end = state.plain.len();
                    shell.capture_event();
                    shell.invalidate_layout();
                }
            }
            _ => {}
        }
        if previous != (state.anchor.min(state.end)..state.anchor.max(state.end)) {
            shell.request_redraw();
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
            if state.link(position).is_some() {
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
    fn selection_and_find_preserve_markdown_strikes_and_span_underlines() {
        use iced::advanced::renderer::{Headless, Renderer};

        let cached = CachedItem::new(
            &ItemContent::Text {
                text: "~~x       x~~".to_owned(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            crate::settings::EditorSyntaxTheme::Auto,
        );
        let markdown::Item::Paragraph(text) = &cached.markdown.items()[0] else {
            panic!("fixture must render as an ordinary Markdown paragraph");
        };
        let authored = text.spans(markdown::Style::from(iced::Theme::Light));
        assert!(authored.iter().all(|span| span.strikethrough));
        for underline in [false, true] {
            // Integer decoration positions isolate an opaque foreground pixel
            // inside whitespace, where text glyphs cannot hide a lost stroke.
            let (size, line_height, stroke_y) = if underline {
                (25.0_f32, 31.0_f32, 26)
            } else {
                (20.0_f32, 24.0_f32, 12)
            };
            for mode in 0..3 {
                let mut renderer = crate::app::tests::headless_renderer();
                let spans: Arc<[_]> = authored
                    .iter()
                    .cloned()
                    .map(|mut span| {
                        span.color = Some(iced::Color::BLACK);
                        span.underline = underline;
                        span.strikethrough = !underline;
                        span.line_height =
                            Some(iced::widget::text::LineHeight::Absolute(line_height.into()));
                        span
                    })
                    .collect::<Vec<_>>()
                    .into();
                let plain: Arc<str> = spans
                    .iter()
                    .map(|span| span.text.as_ref())
                    .collect::<String>()
                    .into();
                let mut selectable = Selectable {
                    find_enabled: true,
                    plain: plain.clone(),
                    spans,
                    matches: vec![].into(),
                    size: size.into(),
                    font: iced::Font::default(),
                    target: Target {
                        card: CardId::new(),
                        generation: 0,
                    },
                    logical: None,
                    flow: None,
                    flow_ready: None,
                };
                let mut tree =
                    Tree::new(&selectable as &dyn Widget<Message, cosmic::Theme, cosmic::Renderer>);
                let node = selectable.layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(400.0, 100.0)),
                );
                let state = tree.state.downcast_mut::<State>();
                let x = state
                    .paragraph()
                    .buffer()
                    .layout_runs()
                    .find_map(|run| {
                        run.glyphs
                            .iter()
                            .find(|glyph| run.text[glyph.start..glyph.end].trim().is_empty())
                            .map(|glyph| (glyph.x + glyph.w * 0.5) as usize)
                    })
                    .expect("fixture must contain a shaped whitespace region");
                match mode {
                    1 => {
                        state.anchor = 0;
                        state.end = plain.len();
                    }
                    2 => selectable.matches = vec![0..plain.len()].into(),
                    _ => {}
                }
                let viewport = Rectangle::with_size(Size::new(400.0, 100.0));
                renderer.reset(viewport);
                selectable.draw(
                    &tree,
                    &mut renderer,
                    &cosmic::theme::active(),
                    &renderer::Style::default(),
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &viewport,
                );
                let pixels = renderer.screenshot(Size::new(400, 100), 1.0, iced::Color::WHITE);
                assert_eq!(
                    &pixels[(stroke_y * 400 + x) * 4..][..4],
                    &[0, 0, 0, 255],
                    "decoration disappears over highlight: underline={underline}, mode={mode}"
                );
            }
        }
    }

    #[test]
    fn selection_paints_every_selected_glyph_across_bidirectional_visual_runs() {
        assert_bidirectional_paint(false);
    }

    #[test]
    fn find_paints_every_matching_glyph_across_bidirectional_visual_runs() {
        assert_bidirectional_paint(true);
    }

    #[test]
    fn selection_and_find_paint_only_the_hit_testable_part_of_a_ligature() {
        use iced::advanced::renderer::{Headless, Renderer};

        for (source, expected, rtl) in [("office affinity", "f", false), ("لا", "ل", true)] {
            for find in [false, true] {
                let mut renderer = crate::app::tests::headless_renderer();
                let mut selectable = Selectable {
                    find_enabled: true,
                    plain: source.into(),
                    spans: vec![Span::new(source)].into(),
                    matches: vec![].into(),
                    size: 32.0.into(),
                    font: iced::Font::with_name("DejaVu Sans"),
                    target: Target {
                        card: CardId::new(),
                        generation: 0,
                    },
                    logical: None,
                    flow: None,
                    flow_ready: None,
                };
                let mut tree =
                    Tree::new(&selectable as &dyn Widget<Message, cosmic::Theme, cosmic::Renderer>);
                let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 100.0));
                let node = selectable.layout(&mut tree, &renderer, &limits);
                let state = tree.state.downcast_mut::<State>();
                let (start, end, x, y, width, count) = state
                    .paragraph()
                    .buffer()
                    .layout_runs()
                    .find_map(|run| {
                        run.glyphs.iter().find_map(|glyph| {
                            let cluster = &run.text[glyph.start..glyph.end];
                            let count = cluster.graphemes(true).count();
                            (count > 1).then(|| {
                                (
                                    glyph.start,
                                    glyph.start + cluster.graphemes(true).next().unwrap().len(),
                                    glyph.x,
                                    run.line_top,
                                    glyph.w / count as f32,
                                    count,
                                )
                            })
                        })
                    })
                    .expect("fixture font must shape a real multi-grapheme ligature");
                assert_eq!(
                    state.hit(Point::new(
                        x + width * if rtl { 0.25 } else { 0.75 },
                        y + 1.0
                    )),
                    end,
                    "the first ligature component must expose its own caret endpoint"
                );
                assert_eq!(&source[start..end], expected);
                if find {
                    selectable.matches = vec![start..end].into();
                } else {
                    state.anchor = start;
                    state.end = end;
                }
                let viewport = Rectangle::with_size(Size::new(400.0, 100.0));
                renderer.reset(viewport);
                selectable.draw(
                    &tree,
                    &mut renderer,
                    &cosmic::theme::active(),
                    &renderer::Style::default(),
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &viewport,
                );
                let pixels = renderer.screenshot(Size::new(400, 100), 1.0, iced::Color::WHITE);
                let color = if find {
                    design::accent_wash(&cosmic::theme::active())
                } else {
                    design::accent_color(&cosmic::theme::active())
                }
                .into_rgba8();
                for part in 0..count {
                    let px = (x + width * (part as f32 + 0.5)) as usize;
                    let py = (y + 1.0) as usize;
                    let painted = pixels[(py * 400 + px) * 4..][..4]
                        .iter()
                        .zip(color)
                        .all(|(actual, expected)| actual.abs_diff(expected) <= 1);
                    assert_eq!(painted, part == 0, "ligature component {part}, find={find}");
                }
            }
        }
    }

    fn assert_bidirectional_paint(find: bool) {
        use iced::advanced::renderer::{Headless, Renderer};

        let mut renderer = crate::app::tests::headless_renderer();
        let source = "hello שלום world";
        let mut selectable = Selectable {
            find_enabled: true,
            plain: source.into(),
            spans: vec![Span::new(source)].into(),
            matches: vec![].into(),
            size: design::TEXT_PROSE.into(),
            font: iced::Font::default(),
            target: Target {
                card: CardId::new(),
                generation: 0,
            },
            logical: None,
            flow: None,
            flow_ready: None,
        };
        let mut tree =
            Tree::new(&selectable as &dyn Widget<Message, cosmic::Theme, cosmic::Renderer>);
        let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 100.0));
        selectable.layout(&mut tree, &renderer, &limits);
        // A logical selection cuts the RTL word and continues into LTR text.
        // Its selected glyphs are separated by an unselected visual glyph.
        let start = "hello ש".len();
        let state = tree.state.downcast_mut::<State>();
        if find {
            selectable.matches = vec![start..source.len()].into();
        } else {
            state.anchor = start;
            state.end = source.len();
        }
        let node = selectable.layout(&mut tree, &renderer, &limits);
        let state = tree.state.downcast_ref::<State>();
        let probes: Vec<_> = state
            .paragraph()
            .buffer()
            .layout_runs()
            .flat_map(|run| {
                run.glyphs.iter().map(move |glyph| {
                    (
                        glyph.start,
                        (glyph.x + glyph.w * 0.5) as usize,
                        (run.line_top + 1.0) as usize,
                    )
                })
            })
            .collect();
        let viewport = Rectangle::with_size(Size::new(400.0, 100.0));
        renderer.reset(viewport);
        selectable.draw(
            &tree,
            &mut renderer,
            &cosmic::theme::active(),
            &renderer::Style::default(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &viewport,
        );
        let pixels = renderer.screenshot(Size::new(400, 100), 1.0, iced::Color::WHITE);
        let selected = if find {
            design::accent_wash(&cosmic::theme::active())
        } else {
            design::accent_color(&cosmic::theme::active())
        }
        .into_rgba8();
        for (byte, x, y) in probes {
            let pixel = &pixels[(y * 400 + x) * 4..][..4];
            let painted = pixel
                .iter()
                .zip(selected)
                .all(|(actual, expected)| actual.abs_diff(expected) <= 1);
            assert_eq!(
                painted,
                byte >= start,
                "selection paint disagrees at source byte {byte}, pixel ({x}, {y}): {pixel:?}"
            );
        }
    }

    #[test]
    fn prose_exposes_full_text_and_stable_accessible_identity() {
        let mut element = text(
            vec![Span::new("α first\nβ second")].into(),
            design::TEXT_PROSE.into(),
            iced::Font::default(),
            Target {
                card: CardId::new(),
                generation: 0,
            },
            None,
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &crate::app::tests::headless_renderer(),
            &layout::Limits::new(Size::ZERO, Size::new(80.0, 40.0)),
        );
        assert!(node.size().width > 0.0 && node.size().width <= 80.0);
        assert!(node.size().height > 0.0 && node.size().height <= 40.0);
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
            (
                12.0,
                13.0,
                12.0 + node.size().width as f64,
                13.0 + node.size().height as f64
            )
        );
        assert_eq!(first.root()[0].node().value(), Some("α first\nβ second"));
        assert_eq!(
            first.root()[0].node().role(),
            iced_accessibility::accesskit::Role::Label
        );
    }

    #[test]
    fn shaped_hit_testing_keeps_multiline_offsets_and_outside_drag_direction() {
        let spans: Arc<[Span<'static, markdown::Uri>]> =
            vec![Span::new("α first\nβ second")].into();
        let selectable = Selectable {
            find_enabled: true,
            logical: None,
            flow: None,
            flow_ready: None,
            plain: "α first\nβ second".into(),
            spans: spans.clone(),
            matches: vec![].into(),
            size: design::TEXT_PROSE.into(),
            font: iced::Font::default(),
            target: Target {
                card: CardId::new(),
                generation: 0,
            },
        };
        let state = State {
            plain: selectable.plain.clone(),
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
        let mut element = logical_text(
            vec![Span::new(source.to_owned())].into(),
            design::TEXT_PROSE.into(),
            iced::Font::default(),
            Target {
                card: CardId::new(),
                generation: 0,
            },
            None,
            Logical {
                identity,
                start: 100,
            },
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(150.0, f32::INFINITY)),
        );
        let state = tree.state.downcast_ref::<State>();
        for (byte, ch) in source.char_indices().filter(|(_, ch)| !ch.is_whitespace()) {
            let mut painted = Vec::new();
            visit_range_bounds(state.paragraph(), &[byte..byte + ch.len_utf8()], |bounds| {
                painted.push(bounds)
            });
            assert!(
                !painted.is_empty(),
                "every selected grapheme paints its existing cluster"
            );
            for bounds in painted {
                assert!(bounds.x >= -0.5 && bounds.x + bounds.width <= 150.5);
                assert!(bounds.y >= -0.5 && bounds.y + bounds.height <= node.size().height + 0.5);
            }
            assert!(state.logical_byte_bounds(identity, 100 + byte).is_some());
        }
        assert!(state.logical_byte_bounds(identity, 99).is_none());
        assert!(state.logical_byte_bounds(identity + 1, 100).is_none());
        assert_eq!(
            state.paragraph_builds.get(),
            1,
            "paint/navigation reuse the same paragraph"
        );
    }

    #[test]
    fn selection_reuses_the_paragraph_used_for_drawing_and_hit_testing() {
        let renderer = crate::app::tests::headless_renderer();
        let mut element = text(
            vec![Span::new("linked α prose ".repeat(100)).link(markdown::Uri::from("source.rs"))]
                .into(),
            design::TEXT_PROSE.into(),
            iced::Font::default(),
            Target {
                card: CardId::new(),
                generation: 0,
            },
            Some("prose"),
        );
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, f32::INFINITY));
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let state = tree.state.downcast_ref::<State>();
        let buffer = state.paragraph().buffer() as *const _ as usize;
        assert_eq!(state.paragraph_builds.get(), 1);
        assert!(
            tree.children.is_empty(),
            "no second rich-text paragraph tree"
        );
        for end in [1, 20, 100, 300] {
            let state = tree.state.downcast_mut::<State>();
            state.anchor = 0;
            state.end = end;
            element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let state = tree.state.downcast_ref::<State>();
            assert_eq!(
                state.paragraph_builds.get(),
                1,
                "selection does not construct another paragraph"
            );
            assert_eq!(
                buffer,
                state.paragraph().buffer() as *const _ as usize,
                "selection changes paint bounds instead of reshaping spans"
            );
        }
    }

    #[test]
    fn long_tokens_wrap_with_accessible_glyphs_and_matching_hit_geometry() {
        let renderer = crate::app::tests::headless_renderer();
        let source = "w".repeat(3000);
        let spans: Arc<[Span<'static, markdown::Uri>]> =
            vec![Span::new(source.clone()).link(markdown::Uri::from("https://example.com"))].into();
        let mut selectable = Selectable {
            find_enabled: true,
            plain: source.clone().into(),
            spans,
            matches: vec![].into(),
            size: design::TEXT_PROSE.into(),
            font: iced::Font::default(),
            target: Target {
                card: CardId::new(),
                generation: 0,
            },
            logical: None,
            flow: None,
            flow_ready: None,
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
        let spans: Arc<[Span<'static, markdown::Uri>]> = vec![
            Span::new("linked ".repeat(12)).link(markdown::Uri::from("first")),
            Span::new(" plain "),
            Span::new("שלום".to_owned()).link(markdown::Uri::from("rtl")),
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
        let spans: Arc<[Span<'static, markdown::Uri>]> = vec![
            Span::new("before α").font(iced::Font::MONOSPACE),
            Span::new("β linked").link(markdown::Uri::from("src/app.rs:42")),
        ]
        .into();
        let selectable = Selectable {
            find_enabled: true,
            logical: None,
            flow: None,
            flow_ready: None,
            plain: "before αβ linked".into(),
            spans,
            matches: vec![].into(),
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
            find_enabled: true,
            logical: None,
            flow: None,
            flow_ready: None,
            plain: "Straße CAFÉ".into(),
            spans: vec![Span::new("Stra"), Span::new("ße"), Span::new(" CAFÉ")].into(),
            matches: super::super::find::ranges("Straße CAFÉ", "STRASSE").into(),
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
            find_enabled: true,
            logical: None,
            flow: None,
            flow_ready: None,
            plain: "one needle two".into(),
            spans: vec![Span::new("one needle two")].into(),
            matches: vec![4..10].into(),
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

#[cfg(test)]
mod flow_tests {
    use super::*;
    #[test]
    fn huge_flow_layout_defers_shaping_and_requests_only_when_native_visible() {
        let renderer = crate::app::tests::headless_renderer();
        let spans: Arc<[Span<'static, markdown::Uri>]> =
            vec![Span::new("word ".repeat(210_000))].into();
        let cache = flow_layout::Cache::default();
        let flow = cache.entry(99, spans.clone());
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let mut element = flow_text(
            spans,
            iced::Pixels(16.0),
            iced::Font::DEFAULT,
            target,
            None,
            Logical {
                identity: 99,
                start: 0,
            },
            flow,
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(720.0, f32::INFINITY)),
        );
        assert_eq!(tree.state.downcast_ref::<State>().paragraph_builds.get(), 0);
        assert!(tree.children.is_empty());
        let mut messages = Vec::new();
        let event = Event::Window(iced::window::Event::RedrawRequested(
            iced::time::Instant::now(),
        ));
        let hidden = Rectangle::new(Point::new(1000.0, 0.0), Size::new(100.0, 400.0));
        element.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &hidden,
        );
        assert!(messages.is_empty());
        let visible = Rectangle::with_size(Size::new(720.0, 400.0));
        for _ in 0..2 {
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &visible,
            );
        }
        assert_eq!(
            messages.len(),
            1,
            "one pending request per source/font/width generation"
        );
        assert!(
            matches!(messages[0], Message::ChatUi(Action::FlowRequest(found, _)) if found == target)
        );
        assert_eq!(tree.state.downcast_ref::<State>().paragraph_builds.get(), 0);
    }
}
