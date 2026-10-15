//! Full-context shaping and bounded display projection prototype.
//!
//! The active CachedItem should own this cache, outside evictable widget Trees.
//! Cold shaping is deliberately explicit. This module does not yet replace the
//! production prose widget or satisfy the huge-paragraph performance contract.
use super::*;
use iced::advanced::graphics::text::{self as graphics_text, cosmic_text};
use iced::advanced::text::Paragraph;
use iced::widget::text::Span;

type ParagraphBuffer = <cosmic::Renderer as iced::advanced::text::Renderer>::Paragraph;

/// Immutable source/width/font-specific geometry. Retaining this is more
/// expensive than a scroll bookmark; release it with active transcript caches.
struct Prepared {
    snapshot: cosmic_text::LayoutSnapshot,
    bounds: iced::Size,
}

impl Prepared {
    /// Use the same rich-text shaping attributes as iced's Paragraph. Font
    /// changes, width, source/span changes and theme colors invalidate this.
    fn new(spans: &[Span<'static, String>], size: iced::Pixels, font: iced::Font, width: f32, wrapping: iced::widget::text::Wrapping) -> Self {
        let line_height = iced::widget::text::LineHeight::default();
        let mut fonts = graphics_text::font_system().write().expect("font system");
        let mut buffer = cosmic_text::Buffer::new(fonts.raw(), cosmic_text::Metrics::new(size.0, line_height.to_absolute(size).0));
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
        buffer.shape_until_scroll(fonts.raw(), false);
        let bounds = graphics_text::align(&mut buffer, fonts.raw(), Default::default());
        let snapshot = buffer.into_layout_snapshot().expect("complete paragraph layout");
        Self { snapshot, bounds }
    }
}

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
