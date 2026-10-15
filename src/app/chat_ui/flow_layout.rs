//! Full-context shaping and bounded display projection prototype.
//!
//! Active CachedItem keeps weak geometry descriptors; evicted row owners release
//! full snapshots while measured heights remain in the virtual column. Cold full
//! shaping runs on a bounded worker; it remains a measured cost, not a claim of
//! viewport-only shaping or a fulfilled huge-paragraph performance budget.
use super::*;
use iced::advanced::graphics::text::{self as graphics_text, cosmic_text};
#[cfg(test)]
use iced::advanced::text::Paragraph;
use iced::widget::text::Span;

#[cfg(test)]
type ParagraphBuffer = <cosmic::Renderer as iced::advanced::text::Renderer>::Paragraph;

use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Default)]
pub(super) struct Cache(Rc<RefCell<HashMap<u64, std::rc::Weak<RefCell<Entry>>>>>);
pub(super) type Handle = Rc<RefCell<Entry>>;

pub(super) struct Entry {
    id: u64,
    spans: Arc<[Span<'static, String>]>,
    key: Option<Key>,
    attempt: u64,
    requested: bool,
    deferred: Option<u64>,
    failed: bool,
    ready: Option<Arc<Prepared>>,
    cancelled: Arc<AtomicBool>,
}
impl Drop for Entry {
    fn drop(&mut self) { self.cancelled.store(true, Ordering::Release); }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Key {
    width: f32,
    size: iced::Pixels,
    font: iced::Font,
    version: graphics_text::Version,
}

impl Cache {
    pub(super) fn entry(&self, identity: u64, spans: Arc<[Span<'static, String>]>) -> Handle {
        let mut entries = self.0.borrow_mut();
        if let Some(entry) = entries.get(&identity).and_then(std::rc::Weak::upgrade)
            && Arc::ptr_eq(&entry.borrow().spans, &spans) { return entry; }
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let entry = Rc::new(RefCell::new(Entry { id: NEXT.fetch_add(1, Ordering::Relaxed), spans,
            key: None, attempt: 0, requested: false, deferred: None, failed: false, ready: None, cancelled: Arc::new(AtomicBool::new(false)) }));
        entries.insert(identity, Rc::downgrade(&entry));
        entry
    }
    pub(super) fn find(&self, id: u64) -> Option<Handle> {
        self.0.borrow().values().filter_map(std::rc::Weak::upgrade).find(|entry| entry.borrow().id == id)
    }
    pub(super) fn retained_bytes(&self) -> usize {
        self.0.borrow().values().filter_map(std::rc::Weak::upgrade).filter_map(|entry| entry.borrow().ready.clone())
            .map(|prepared| prepared.snapshot.retained_bytes()).sum()
    }
}
impl Entry {
    pub(super) fn configure(&mut self, width: f32, size: iced::Pixels, font: iced::Font) {
        let key = Key { width, size, font, version: graphics_text::font_system().read().expect("font system").version() };
        if self.key != Some(key) {
            self.key = Some(key);
            self.attempt = self.attempt.wrapping_add(1);
            self.requested = false;
            self.deferred = None;
            self.failed = false;
            self.ready = None;
            self.cancelled.store(true, Ordering::Release);
            self.cancelled = Arc::new(AtomicBool::new(false));
        }
    }
    pub(super) fn failed(&self) -> bool { self.failed }
    pub(super) fn prepared(&self) -> Option<Arc<Prepared>> { self.ready.clone() }
    pub(super) fn bounds(&self, bytes: usize) -> iced::Size {
        self.ready.as_ref().map_or_else(|| {
            let key = self.key.expect("configured layout");
            let lines = (bytes as f32 * key.size.0 * 0.5 / key.width.max(1.0)).ceil().max(1.0);
            iced::Size::new(key.width, lines * key.size.0 * 1.3)
        }, |ready| ready.bounds)
    }
    pub(super) fn request(&mut self) -> Option<Request> {
        let key = self.key?;
        if self.ready.is_some() || self.requested || self.failed || self.deferred == Some(COMPLETED.load(Ordering::Acquire)) || !key.width.is_finite() || key.width <= 0.0 { return None; }
        self.requested = true;
        self.deferred = None;
        Some(Request { id: self.id, attempt: self.attempt, key, spans: self.spans.clone(), cancelled: self.cancelled.clone() })
    }
    pub(super) fn accepts(&self, request: &Request) -> bool {
        self.id == request.id && self.attempt == request.attempt && self.key == Some(request.key)
            && Arc::ptr_eq(&self.spans, &request.spans) && !request.cancelled.load(Ordering::Acquire)
    }
    pub(super) fn defer(&mut self, request: &Request, epoch: u64) {
        if self.accepts(request) { self.requested = false; self.deferred = Some(epoch); }
    }
    pub(super) fn complete(&mut self, request: &Request, ready: Option<Arc<Prepared>>) -> bool {
        if !self.accepts(request) { return false; }
        let version = graphics_text::font_system().read().expect("font system").version();
        if version != request.key.version {
            let old = request.key;
            self.key = Some(Key { version, ..old });
            self.attempt = self.attempt.wrapping_add(1);
            self.cancelled.store(true, Ordering::Release);
            self.cancelled = Arc::new(AtomicBool::new(false));
            self.requested = false;
            self.deferred = None;
            self.ready = None;
            return true;
        }
        self.requested = false;
        self.failed = ready.is_none();
        self.ready = ready;
        true
    }
}

#[derive(Clone)]
pub(super) struct Request {
    pub(super) id: u64,
    attempt: u64,
    key: Key,
    spans: Arc<[Span<'static, String>]>,
    cancelled: Arc<AtomicBool>,
}
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlowRequest").field("id", &self.id).field("attempt", &self.attempt).field("key", &self.key).finish()
    }
}

static COMPLETED: AtomicU64 = AtomicU64::new(0);
struct WorkSlot(Option<tokio::sync::OwnedSemaphorePermit>);
impl Drop for WorkSlot {
    fn drop(&mut self) {
        self.0.take();
        COMPLETED.fetch_add(1, Ordering::AcqRel);
    }
}

pub(super) fn run(target: Target, request: Request) -> Result<Task<Message>, u64> {
    // One shaping job, no waiting worker queue. Deferred live entries become
    // eligible when that job completes; redraw alone cannot create a busy loop.
    static SLOTS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let slots = SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1))).clone();
    let epoch = COMPLETED.load(Ordering::Acquire);
    let permit = WorkSlot(Some(slots.try_acquire_owned().map_err(|_| epoch)?));
    // Copy database metadata under the short UI lock, never shape under it.
    let (db, locale) = {
        let mut fonts = graphics_text::font_system().write().expect("font system");
        (fonts.db().clone(), fonts.raw().locale().to_owned())
    };
    Ok(cosmic::task::future(async move {
        let work = request.clone();
        let ready = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if work.cancelled.load(Ordering::Acquire) { return None; }
            let mut fonts = cosmic_text::FontSystem::new_with_locale_and_db(locale, db);
            let prepared = Prepared::with_fonts(&work.spans, work.key.size, work.key.font, work.key.width,
                iced::widget::text::Wrapping::WordOrGlyph, &mut fonts);
            (!work.cancelled.load(Ordering::Acquire)).then(|| Arc::new(prepared))
        }).await.ok().flatten();
        action(Action::FlowReady(target, request, ready))
    }))
}

/// Immutable source/width/font-specific geometry. Retaining this is more
/// expensive than a scroll bookmark; release it with active transcript caches.
#[derive(Debug)]
pub(super) struct Prepared {
    pub(super) snapshot: cosmic_text::LayoutSnapshot,
    pub(super) bounds: iced::Size,
}

impl Prepared {
    /// Use the same rich-text shaping attributes as iced's Paragraph. Font
    /// changes, width, source/span changes and theme colors invalidate this.
    #[cfg(test)]
    fn new(spans: &[Span<'static, String>], size: iced::Pixels, font: iced::Font, width: f32, wrapping: iced::widget::text::Wrapping) -> Self {
        let (db, locale) = {
            let mut fonts = graphics_text::font_system().write().expect("font system");
            (fonts.db().clone(), fonts.raw().locale().to_owned())
        };
        let mut fonts = cosmic_text::FontSystem::new_with_locale_and_db(locale, db);
        Self::with_fonts(spans, size, font, width, wrapping, &mut fonts)
    }

    fn with_fonts(spans: &[Span<'static, String>], size: iced::Pixels, font: iced::Font, width: f32,
        wrapping: iced::widget::text::Wrapping, fonts: &mut cosmic_text::FontSystem) -> Self {
        let line_height = iced::widget::text::LineHeight::default();
        let mut buffer = cosmic_text::Buffer::new(fonts, cosmic_text::Metrics::new(size.0, line_height.to_absolute(size).0));
        buffer.set_size(Some(width), None);
        buffer.set_wrap(graphics_text::to_wrap(wrapping));
        buffer.set_rich_text(spans.iter().enumerate().map(|(index, span)| {
            let mut attrs = graphics_text::to_attributes(span.font.unwrap_or(font));
            if span.size.is_some() || span.line_height.is_some() {
                let span_size = span.size.unwrap_or(size);
                attrs = attrs.metrics(cosmic_text::Metrics::new(span_size.0, span.line_height.unwrap_or(line_height).to_absolute(span_size).0));
            }
            if let Some(color) = span.color { attrs = attrs.color(graphics_text::to_color(color)); }
            (span.text.as_ref(), attrs.metadata(index))
        }), &graphics_text::to_attributes(font), graphics_text::to_shaping_ascii(iced::widget::text::Shaping::Advanced, false), None);
        buffer.shape_until_scroll(fonts, false);
        let bounds = graphics_text::align(&mut buffer, fonts, Default::default());
        let snapshot = buffer.into_layout_snapshot().expect("complete paragraph layout");
        Self { snapshot, bounds }
    }
}

#[cfg(test)]
fn reference(spans: &[Span<'static, String>], size: iced::Pixels, font: iced::Font, width: f32, wrapping: iced::widget::text::Wrapping) -> ParagraphBuffer {
    ParagraphBuffer::with_spans(iced::advanced::Text {
        content: spans,
        bounds: iced::Size::new(width, f32::INFINITY),
        size,
        line_height: Default::default(),
        font,
        align_x: Default::default(),
        align_y: iced::alignment::Vertical::Top,
        shaping: iced::widget::text::Shaping::Advanced,
        wrapping,
        ellipsize: Default::default(),
    })
}

#[test]
fn visual_projection_matches_actual_paragraph_across_wrap_ligature_and_bidi_context() {
    let width = 720.0;
    let size = iced::Pixels(16.0);
    let sources = [
        "word ".repeat(3000),
        format!("{}ffi office efficient {}", "x".repeat(2047), " ligature ffi ".repeat(600)),
        "العربية نص عربي يجمع اتجاهين English עברית שלום 123 ".repeat(500),
        "e\u{301} 👩‍🔬 👨‍👩‍👧‍👦 combining mixed نص ".repeat(500),
    ];
    for source in sources {
        let spans = vec![Span::new(source)];
        let full = reference(&spans, size, iced::Font::DEFAULT, width, iced::widget::text::Wrapping::WordOrGlyph);
        let prepared = Prepared::new(&spans, size, iced::Font::DEFAULT, width, iced::widget::text::Wrapping::WordOrGlyph);
        assert_eq!(prepared.bounds, full.min_bounds());
        assert_eq!(prepared.snapshot.height(), full.min_height());
        let original = full.buffer().layout_runs().collect::<Vec<_>>();
        assert_eq!(prepared.snapshot.visual_line_count(), original.len());
        for top in [0.0, full.min_height() * 0.5, (full.min_height() - 400.0).max(0.0)] {
            let range = prepared.snapshot.visible_lines(top, top + 400.0);
            let projected = prepared.snapshot.project(range.clone()).expect("visible range");
            let actual = projected.buffer().layout_runs().collect::<Vec<_>>();
            assert_eq!(actual.len(), range.len());
            assert!(actual.len() < 32, "viewport glyph owners remain bounded");
            for (local, visual) in range.enumerate() {
                let before = &original[visual];
                let after = &actual[local];
                assert_eq!(before.rtl, after.rtl);
                assert_eq!(before.line_w.to_bits(), after.line_w.to_bits());
                assert_eq!(before.decorations, after.decorations);
                assert!((before.line_y - (after.line_y + projected.top())).abs() <= 0.02);
                assert!((before.line_top - (after.line_top + projected.top())).abs() <= 0.02, "translation differs beyond f32 accumulation tolerance");
                assert_eq!(before.glyphs.len(), after.glyphs.len());
                for (expected, actual) in before.glyphs.iter().zip(after.glyphs) {
                    assert_eq!((expected.font_id, expected.glyph_id, expected.level, expected.metadata), (actual.font_id, actual.glyph_id, actual.level, actual.metadata));
                    assert_eq!((expected.x.to_bits(), expected.y.to_bits(), expected.w.to_bits(), expected.x_offset.to_bits(), expected.y_offset.to_bits()), (actual.x.to_bits(), actual.y.to_bits(), actual.w.to_bits(), actual.x_offset.to_bits(), actual.y_offset.to_bits()));
                    assert_eq!(projected.source_byte(local, actual.start), Some(expected.start));
                    assert_eq!(projected.source_byte(local, actual.end), Some(expected.end));
                }
            }
        }
    }
}

#[test]
fn rich_font_span_metadata_and_unwrapped_horizontal_geometry_are_preserved() {
    let spans = vec![
        Span::new("office ffi ".repeat(500)).font(iced::Font::MONOSPACE),
        Span::new("نص عربي mixed ".repeat(500)).color(iced::Color::from_rgb(0.3, 0.5, 0.7)),
        Span::new("end".to_owned()),
    ];
    for wrapping in [iced::widget::text::Wrapping::WordOrGlyph, iced::widget::text::Wrapping::None] {
        let full = reference(&spans, iced::Pixels(16.0), iced::Font::DEFAULT, 720.0, wrapping);
        let prepared = Prepared::new(&spans, iced::Pixels(16.0), iced::Font::DEFAULT, 720.0, wrapping);
        assert_eq!(prepared.bounds, full.min_bounds());
        let projection = prepared.snapshot.project(0..prepared.snapshot.visual_line_count()).expect("all visual lines");
        for (before, after) in full.buffer().layout_runs().zip(projection.buffer().layout_runs()) {
            assert_eq!(before.line_w.to_bits(), after.line_w.to_bits());
            for (before, after) in before.glyphs.iter().zip(after.glyphs) {
                assert_eq!((before.x.to_bits(), before.w.to_bits(), before.metadata, before.color_opt), (after.x.to_bits(), after.w.to_bits(), after.metadata, after.color_opt));
            }
        }
    }
}

pub(super) struct Projection {
    pub(super) buffer: Arc<cosmic_text::Buffer>,
    pub(super) source_starts: Vec<usize>,
    pub(super) top: f32,
}
impl Prepared {
    pub(super) fn projection(&self, top: f32, bottom: f32) -> Option<Projection> {
        let projection = self.snapshot.project(self.snapshot.visible_lines(top, bottom))?;
        let source_starts = projection.source_starts().to_vec();
        let top = projection.top();
        Some(Projection { buffer: Arc::new(projection.into_buffer()), source_starts, top })
    }
    pub(super) fn hit(&self, position: iced::Point, length: usize) -> usize {
        if position.y < 0.0 { return 0; }
        if position.y >= self.bounds.height { return length; }
        let Some(projection) = self.projection(position.y, position.y + 0.1) else { return 0; };
        let Some(cursor) = projection.buffer.hit(position.x, position.y - projection.top) else { return 0; };
        projection.source_starts.get(cursor.line).copied().unwrap_or(0).saturating_add(cursor.index).min(length)
    }
    pub(super) fn link(&self, position: iced::Point) -> Option<usize> {
        let projection = self.projection(position.y, position.y + 0.1)?;
        projection.buffer.layout_runs().find_map(|run| run.glyphs.iter().find(|glyph|
            position.x >= glyph.x && position.x < glyph.x + glyph.w).map(|glyph| glyph.metadata))
    }
    pub(super) fn byte_bounds(&self, byte: usize, length: usize) -> Option<iced::Rectangle> {
        let query = if byte == length { byte.saturating_sub(1) } else { byte };
        for visual in self.snapshot.source_visual_lines(query) {
            for (prefix, glyph) in self.snapshot.glyphs(visual)? {
                if prefix + glyph.start <= query && query < prefix + glyph.end {
                    return Some(iced::Rectangle::new(iced::Point::new(glyph.x.min(glyph.x + glyph.w), self.snapshot.line_top(visual)?),
                        iced::Size::new(glyph.w.abs(), self.snapshot.line_height(visual)?)));
                }
            }
        }
        None
    }
}

pub(super) fn paint(
    renderer: &mut cosmic::Renderer, projection: &Projection,
    spans: &[Span<'static, String>], matches: &[std::ops::Range<usize>], selection: std::ops::Range<usize>,
    size: iced::Pixels, position: iced::Point, theme: &cosmic::Theme,
    defaults: &iced::advanced::renderer::Style, viewport: &iced::Rectangle,
) {
    use iced::advanced::Renderer as _;
    use iced::advanced::text::Renderer as _;
    let translation = iced::Vector::new(position.x, position.y + projection.top);
    for run in projection.buffer.layout_runs() {
        let Some(prefix) = projection.source_starts.get(run.line_i).copied() else { continue; };
        let mut start = 0;
        while let Some(first) = run.glyphs.get(start) {
            let end = start + run.glyphs[start..].iter().position(|glyph| glyph.metadata != first.metadata).unwrap_or(run.glyphs.len() - start);
            let end = end.max(start + 1);
            let group = &run.glyphs[start..end];
            if let Some(span) = spans.get(first.metadata) {
                let left = group.iter().map(|glyph| glyph.x.min(glyph.x + glyph.w)).fold(f32::INFINITY, f32::min);
                let right = group.iter().map(|glyph| glyph.x.max(glyph.x + glyph.w)).fold(f32::NEG_INFINITY, f32::max);
                let bounds = iced::Rectangle::new(iced::Point::new(left, run.line_top), iced::Size::new(right - left, run.line_height));
                if let Some(highlight) = span.highlight {
                    renderer.fill_quad(iced::advanced::renderer::Quad {
                        bounds: iced::Rectangle::new(bounds.position() + translation - iced::Vector::new(span.padding.left, span.padding.top),
                            bounds.size() + iced::Size::new(span.padding.x(), span.padding.y())),
                        border: highlight.border, ..Default::default()
                    }, highlight.background);
                }
                let font_size = span.size.unwrap_or(size);
                let height = span.line_height.unwrap_or_default().to_absolute(font_size).0;
                let baseline = font_size.0 + (height - font_size.0) / 2.0;
                for (draw, shift) in [(span.underline, font_size.0 * 0.08), (span.strikethrough, font_size.0 / 2.0)] {
                    if draw {
                        renderer.fill_quad(iced::advanced::renderer::Quad {
                            bounds: iced::Rectangle::new(bounds.position() + translation + iced::Vector::new(0.0, baseline - shift),
                                iced::Size::new(bounds.width, 1.0)), ..Default::default()
                        }, span.color.unwrap_or(defaults.text_color));
                    }
                }
            }
            start = end;
        }
        for (ranges, color) in [(matches, design::accent_wash(theme)), (std::slice::from_ref(&selection), design::accent_color(theme))] {
            let mut active: Option<(f32, f32)> = None;
            let flush = |active: &mut Option<(f32, f32)>, renderer: &mut cosmic::Renderer| {
                if let Some((left, right)) = active.take() {
                    renderer.fill_quad(iced::advanced::renderer::Quad {
                        bounds: iced::Rectangle::new(iced::Point::new(left, run.line_top) + translation,
                            iced::Size::new(right - left, run.line_height)), ..Default::default()
                    }, color);
                }
            };
            for glyph in run.glyphs {
                let begin = prefix + glyph.start;
                let finish = prefix + glyph.end;
                let index = ranges.partition_point(|range| range.end <= begin);
                if ranges.get(index).is_some_and(|range| range.start < finish && !range.is_empty()) {
                    let left = glyph.x.min(glyph.x + glyph.w);
                    let right = glyph.x.max(glyph.x + glyph.w);
                    active = Some(active.map_or((left, right), |(lo, hi)| (lo.min(left), hi.max(right))));
                } else { flush(&mut active, renderer); }
            }
            flush(&mut active, renderer);
        }
    }
    renderer.fill_raw(graphics_text::Raw { buffer: Arc::downgrade(&projection.buffer), position: position + iced::Vector::new(0.0, projection.top),
        color: defaults.text_color, clip_bounds: *viewport });
}

#[test]
fn cache_rejects_stale_source_and_width_and_releases_owned_layout() {
    let spans: Arc<[Span<'static, String>]> = vec![Span::new("word ".repeat(1000))].into();
    let cache = Cache::default();
    let entry = cache.entry(7, spans.clone());
    entry.borrow_mut().configure(720.0, iced::Pixels(16.0), iced::Font::DEFAULT);
    let request = entry.borrow_mut().request().expect("first request");
    let prepared = Arc::new(Prepared::new(&spans, request.key.size, request.key.font, request.key.width, iced::widget::text::Wrapping::WordOrGlyph));
    let weak = Arc::downgrade(&prepared);
    assert!(entry.borrow_mut().complete(&request, Some(prepared)));
    assert!(entry.borrow().prepared().is_some());
    entry.borrow_mut().configure(300.0, iced::Pixels(16.0), iced::Font::DEFAULT);
    assert!(weak.upgrade().is_none(), "width replacement releases old glyph owner");
    assert!(!entry.borrow_mut().complete(&request, None));
    let current = entry.borrow_mut().request().expect("new width request");
    let replacement: Arc<[Span<'static, String>]> = vec![Span::new("replacement".to_owned())].into();
    let _new = cache.entry(7, replacement);
    assert!(cache.find(current.id).is_none(), "old source cannot receive completion");
    drop(entry);
    assert!(current.cancelled.load(Ordering::Acquire));
}

#[test]
fn flow_hits_links_and_find_bounds_use_the_original_full_context_source() {
    let text = format!("{} office ffi العربية עברית {}", "word ".repeat(1000), "suffix ".repeat(500));
    let spans: Arc<[Span<'static, String>]> = vec![Span::new(text.clone()).link("https://example.com".to_owned())].into();
    let prepared = Prepared::new(&spans, iced::Pixels(16.0), iced::Font::DEFAULT, 720.0, iced::widget::text::Wrapping::WordOrGlyph);
    for visual in [0, prepared.snapshot.visual_line_count() / 2, prepared.snapshot.visual_line_count() - 1] {
        for (prefix, glyph) in prepared.snapshot.glyphs(visual).unwrap().take(3) {
            let position = iced::Point::new(glyph.x + glyph.w * 0.25, prepared.snapshot.line_top(visual).unwrap() + 2.0);
            assert_eq!(prepared.link(position), Some(glyph.metadata));
            let hit = prepared.hit(position, text.len());
            assert!(text.is_char_boundary(hit));
            let bounds = prepared.byte_bounds(prefix + glyph.start, text.len()).expect("source glyph bound");
            assert_eq!(bounds.y, prepared.snapshot.line_top(visual).unwrap());
            assert_eq!(bounds.x.to_bits(), glyph.x.min(glyph.x + glyph.w).to_bits());
        }
    }
}

#[test]
fn virtual_row_eviction_releases_flow_glyphs_without_hiding_the_chat() {
    use iced::advanced::{layout, widget::Tree};
    use std::cell::Cell;
    let renderer = crate::app::tests::headless_renderer();
    let cache = Cache::default();
    let owners = Rc::new(RefCell::new(Vec::new()));
    let spans: Arc<[Span<'static, String>]> = vec![Span::new("word ".repeat(800))].into();
    let window = Rc::new(Cell::new(Some(virtualized::Viewport { top: 0.0, height: 200.0, bottom: false })));
    let rows = (0..200).map(|identity| {
        let cache = cache.clone(); let owners = owners.clone(); let spans = spans.clone();
        virtualized::Row::new(identity, 0, 1000.0, None, move || {
            let flow = cache.entry(identity, spans.clone());
            flow.borrow_mut().configure(720.0, iced::Pixels(16.0), iced::Font::DEFAULT);
            let request = flow.borrow_mut().request().unwrap();
            let ready = Arc::new(Prepared::new(&spans, request.key.size, request.key.font, request.key.width, iced::widget::text::Wrapping::WordOrGlyph));
            owners.borrow_mut().push(Arc::downgrade(&ready));
            assert!(flow.borrow_mut().complete(&request, Some(ready)));
            selectable_prose::flow_text(spans.clone(), iced::Pixels(16.0), iced::Font::DEFAULT,
                Target { card: CardId::new(), generation: 0 }, None,
                selectable_prose::Logical { identity, start: 0 }, flow)
        })
    }).collect();
    let mut element = virtualized::column(rows, window.clone());
    let mut tree = Tree::new(&element);
    let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(720.0, f32::INFINITY));
    element.as_widget_mut().layout(&mut tree, &renderer, &limits);
    let first = owners.borrow().clone();
    assert!(!first.is_empty());
    assert!(first.iter().all(|owner| owner.upgrade().is_some()));
    window.set(Some(virtualized::Viewport { top: 100000.0, height: 200.0, bottom: false }));
    element.as_widget_mut().layout(&mut tree, &renderer, &limits);
    assert!(first.iter().all(|owner| owner.upgrade().is_none()), "cached item descriptors must not retain evicted glyph snapshots");
    assert!(cache.retained_bytes() > 0, "the same active chat still owns visible row geometry");
}
