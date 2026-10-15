// SPDX-License-Identifier: MIT OR Apache-2.0

//! Immutable results of full-context shaping with bounded visual projections.
//!
//! This is a display cache, not an incremental shaper. Constructing it still
//! requires shaping each original physical paragraph. Projection never calls
//! the font system, inserts source breaks, or changes glyph positions.
use alloc::{string::String, vec::Vec};
use core::ops::Range;
use crate::{Buffer, BufferLine, LayoutGlyph, LayoutLine, LineEnding, Metrics};

#[derive(Debug)]
pub(crate) struct SnapshotLine {
    pub text: String,
    pub ending: LineEnding,
    pub rtl: bool,
    pub layout: Vec<LayoutLine>,
}

#[derive(Debug)]
struct VisualLine {
    physical: usize,
    layout: usize,
    source_start: usize,
    top: f32,
    height: f32,
}

/// Immutable prelaid glyphs, with the redundant shaping cache released.
///
/// Created by [`Buffer::into_layout_snapshot`]. Width, fonts, attributes and
/// source revisions are part of its owner's cache key; a changed input needs
/// a new snapshot. The snapshot owns all laid-out glyphs. It is not a promise
/// of viewport-only initial shaping or bounded total glyph storage.
#[derive(Debug)]
pub struct LayoutSnapshot {
    lines: Vec<SnapshotLine>,
    visual: Vec<VisualLine>,
    metrics: Metrics,
    width: Option<f32>,
    height: f32,
}

/// A display buffer and its original source-coordinate mapping.
#[derive(Debug)]
pub struct LayoutProjection {
    buffer: Buffer,
    source_starts: Vec<usize>,
    visual: Range<usize>,
    top: f32,
}

impl LayoutSnapshot {
    pub(crate) fn new(lines: Vec<SnapshotLine>, metrics: Metrics, width: Option<f32>) -> Self {
        let mut visual = Vec::new();
        let mut top = 0.0;
        let mut source_start = 0;
        for (physical, line) in lines.iter().enumerate() {
            for (layout, row) in line.layout.iter().enumerate() {
                let height = row.line_height_opt.unwrap_or(metrics.line_height);
                visual.push(VisualLine { physical, layout, source_start, top, height });
                top += height;
            }
            source_start += line.text.len() + line.ending.as_str().len();
        }
        Self { lines, visual, metrics, width, height: top }
    }

    pub fn height(&self) -> f32 { self.height }
    pub fn visual_line_count(&self) -> usize { self.visual.len() }

    /// Owned source and layout allocation estimate, excluding font-system and
    /// allocator overhead. This is not a process RSS measurement.
    pub fn retained_bytes(&self) -> usize {
        self.visual.capacity() * core::mem::size_of::<VisualLine>()
            + self.lines.capacity() * core::mem::size_of::<SnapshotLine>()
            + self.lines.iter().map(|line| line.text.capacity()
                + line.layout.capacity() * core::mem::size_of::<LayoutLine>()
                + line.layout.iter().map(|row| row.glyphs.capacity() * core::mem::size_of::<LayoutGlyph>()
                    + row.decorations.capacity() * core::mem::size_of::<crate::DecorationSpan>()).sum::<usize>()).sum::<usize>()
    }

    /// Locate intersecting visual lines in logarithmic time, without scanning
    /// the wrapped prefix or invoking shaping. Bounds use paragraph coordinates.
    pub fn visible_lines(&self, top: f32, bottom: f32) -> Range<usize> {
        if !top.is_finite() || !bottom.is_finite() || bottom <= top { return 0..0; }
        let first = self.visual.partition_point(|row| row.top + row.height <= top);
        let end = self.visual.partition_point(|row| row.top < bottom);
        first.min(end)..end
    }

    /// Copy only requested visual lines into a renderer-compatible buffer.
    ///
    /// The source remains a full-context snapshot. The display buffer has one
    /// physical line per original visual line, populated with already-shaped
    /// glyphs, so rendering does not walk a hidden wrapped prefix. It must be
    /// consumed read-only; resizing or editing it requires a new projection.
    /// No horizontal glyph culling is performed.
    pub fn project(&self, range: Range<usize>) -> Option<LayoutProjection> {
        if range.start > range.end || range.end > self.visual.len() { return None; }
        let top = self.visual.get(range.start).map_or(self.height, |row| row.top);
        let mut projected = Vec::with_capacity(range.len());
        let mut source_starts = Vec::with_capacity(range.len());
        for visual in &self.visual[range.clone()] {
            let source = &self.lines[visual.physical];
            let row = &source.layout[visual.layout];
            let first = row.glyphs.iter().map(|glyph| glyph.start).min().unwrap_or(0);
            let end = row.glyphs.iter().map(|glyph| glyph.end).max().unwrap_or(first);
            // These are engine-generated cluster boundaries. Keep a checked
            // slice even though a valid shaped buffer guarantees UTF-8 bounds.
            let text = source.text.get(first..end)?.into();
            let mut layout = row.clone();
            for glyph in &mut layout.glyphs { glyph.start -= first; glyph.end -= first; }
            projected.push(BufferLine::from_visual_layout(text, source.rtl, layout));
            source_starts.push(visual.source_start + first);
        }
        let height = self.visual[range.clone()].iter().map(|row| row.height).sum();
        let buffer = Buffer::from_visual_layout(self.metrics, self.width, height, projected);
        Some(LayoutProjection { buffer, source_starts, visual: range, top })
    }

    /// Logical source coordinate of each cluster, paired with its original
    /// glyph geometry. Useful to compare projections and to build source hits.
    pub fn glyphs(&self, visual: usize) -> Option<impl Iterator<Item = (usize, &LayoutGlyph)>> {
        let row = self.visual.get(visual)?;
        Some(self.lines[row.physical].layout[row.layout].glyphs.iter().map(move |glyph| (row.source_start, glyph)))
    }
}

impl LayoutProjection {
    pub fn buffer(&self) -> &Buffer { &self.buffer }
    /// Consume into the ordinary buffer type for an immutable Arc/Weak renderer.
    pub fn into_buffer(self) -> Buffer { self.buffer }
    pub fn top(&self) -> f32 { self.top }
    pub fn visual_lines(&self) -> Range<usize> { self.visual.clone() }
    pub fn source_byte(&self, projected_line: usize, byte: usize) -> Option<usize> {
        let line = self.buffer.lines.get(projected_line)?;
        if byte > line.text().len() || !line.text().is_char_boundary(byte) { return None; }
        self.source_starts.get(projected_line)?.checked_add(byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Attrs, FontSystem, Shaping};

    #[test]
    fn projection_preserves_full_context_glyphs_and_original_source_bytes() {
        let mut fonts = FontSystem::new();
        let text = format!("{} office ffi efficient العربية עברית e\u{301} 👩‍🔬 {}", "word ".repeat(500), "نص عربي mixed ".repeat(150));
        let mut buffer = Buffer::new(&mut fonts, Metrics::new(16.0, 20.0));
        buffer.set_size(Some(720.0), None);
        buffer.set_text(&text, &Attrs::new(), Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut fonts, false);
        let snapshot = buffer.into_layout_snapshot().expect("fully shaped fixture");
        let last = snapshot.visual_line_count();
        for range in [0..3, 4..7, last.saturating_sub(3)..last] {
            let projected = snapshot.project(range.clone()).expect("valid visual range");
            let runs = projected.buffer().layout_runs().collect::<Vec<_>>();
            assert_eq!(runs.len(), range.len());
            for (local, visual) in range.enumerate() {
                let source = &snapshot.visual[visual];
                let original = &snapshot.lines[source.physical].layout[source.layout];
                assert_eq!(runs[local].glyphs.len(), original.glyphs.len());
                for (actual, expected) in runs[local].glyphs.iter().zip(&original.glyphs) {
                    assert_eq!(projected.source_byte(local, actual.start), Some(source.source_start + expected.start));
                    assert_eq!(projected.source_byte(local, actual.end), Some(source.source_start + expected.end));
                    assert_eq!((actual.glyph_id, actual.font_id, actual.level, actual.metadata), (expected.glyph_id, expected.font_id, expected.level, expected.metadata));
                    assert_eq!((actual.x.to_bits(), actual.y.to_bits(), actual.w.to_bits(), actual.x_offset.to_bits(), actual.y_offset.to_bits()), (expected.x.to_bits(), expected.y.to_bits(), expected.w.to_bits(), expected.x_offset.to_bits(), expected.y_offset.to_bits()));
                }
                assert_eq!(runs[local].rtl, snapshot.lines[source.physical].rtl);
                assert_eq!(runs[local].line_w.to_bits(), original.w.to_bits());
                assert!((runs[local].line_top + projected.top() - source.top).abs() < 0.001);
            }
        }
    }

    #[test]
    fn viewport_projection_owns_only_visible_glyphs_and_never_shapes_missing_lines() {
        let mut fonts = FontSystem::new();
        let mut buffer = Buffer::new(&mut fonts, Metrics::new(16.0, 20.0));
        buffer.set_size(Some(720.0), Some(40.0));
        buffer.set_text(&"physical line\n".repeat(100), &Attrs::new(), Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut fonts, false);
        let mut buffer = buffer.into_layout_snapshot().expect_err("hidden physical lines are not silently shaped");
        buffer.set_size(Some(720.0), None);
        buffer.shape_until_scroll(&mut fonts, false);
        let snapshot = buffer.into_layout_snapshot().expect("all physical lines are now shaped");
        let visible = snapshot.visible_lines(500.0, 540.0);
        assert_eq!(visible.len(), 2);
        let projection = snapshot.project(visible).expect("bounded projection");
        assert_eq!(projection.buffer().lines.len(), 2);
        assert_eq!(projection.buffer().layout_runs().count(), 2);
        assert!(snapshot.project(0..usize::MAX).is_none());
    }
    #[test]
    fn configuration_changes_cannot_publish_a_stale_layout_snapshot() {
        let mut fonts = FontSystem::new();
        let mut buffer = Buffer::new(&mut fonts, Metrics::new(16.0, 20.0));
        buffer.set_size(Some(720.0), None);
        buffer.set_text("a paragraph with wraps ".repeat(100).as_str(), &Attrs::new(), Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut fonts, false);
        buffer.set_size(Some(300.0), None);
        let mut buffer = buffer.into_layout_snapshot().expect_err("resize must be laid out first");
        buffer.shape_until_scroll(&mut fonts, false);
        assert!(buffer.into_layout_snapshot().is_ok());
    }

}
