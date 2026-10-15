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
    glyph_index: Option<GlyphIndex>,
}

#[derive(Debug)]
struct SourceVisual { start: usize, end: usize, max_end: usize, visual: usize }

// Large physical lines need lookup indexes; ordinary wrapped rows have a
// fixed small scan bound, avoiding per-glyph index storage for prose.
const INDEX_THRESHOLD: usize = 256;
#[cfg(test)]
std::thread_local! { static INTERVAL_VISITS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) }; }

#[derive(Debug)]
struct GlyphIndex { x: Vec<IntervalEntry>, source: Vec<IntervalEntry> }
#[derive(Debug, Clone, Copy)]
struct IntervalEntry { glyph: usize, max_end_glyph: usize }

fn x_bounds(glyph: &LayoutGlyph) -> (f32, f32) {
    let placed = glyph.x + glyph.font_size * glyph.x_offset;
    let lo = glyph.x.min(glyph.x + glyph.w).min(placed).min(placed + glyph.w);
    let hi = glyph.x.max(glyph.x + glyph.w).max(placed).max(placed + glyph.w);
    if lo.is_finite() && hi.is_finite() { (lo, hi) } else { (-f32::MAX, f32::MAX) }
}
fn source_bounds(glyph: &LayoutGlyph) -> (usize, usize) { (glyph.start, glyph.end) }

impl GlyphIndex {
    fn new(glyphs: &[LayoutGlyph]) -> Self {
        Self { x: interval_index(glyphs, x_bounds), source: interval_index(glyphs, source_bounds) }
    }
}
fn interval_index<T: PartialOrd + Copy>(glyphs: &[LayoutGlyph], bounds: impl Fn(&LayoutGlyph) -> (T,T) + Copy) -> Vec<IntervalEntry> {
    let mut entries: Vec<_> = (0..glyphs.len()).map(|glyph| IntervalEntry { glyph, max_end_glyph: glyph }).collect();
    entries.sort_unstable_by(|a,b| bounds(&glyphs[a.glyph]).0.partial_cmp(&bounds(&glyphs[b.glyph]).0).unwrap_or(core::cmp::Ordering::Equal));
    fn build<T: PartialOrd + Copy>(entries: &mut [IntervalEntry], glyphs: &[LayoutGlyph], bounds: impl Fn(&LayoutGlyph)->(T,T) + Copy) -> Option<usize> {
        if entries.is_empty() { return None; }
        let middle = entries.len()/2;
        let (left, rest) = entries.split_at_mut(middle);
        let (node, right) = rest.split_first_mut().unwrap();
        let mut max = node.glyph;
        for candidate in [build(left,glyphs,bounds), build(right,glyphs,bounds)].into_iter().flatten() {
            if bounds(&glyphs[candidate]).1 > bounds(&glyphs[max]).1 { max = candidate; }
        }
        node.max_end_glyph = max;
        Some(max)
    }
    build(&mut entries, glyphs, bounds);
    entries
}
fn interval_query<T: PartialOrd + Copy>(entries: &[IntervalEntry], glyphs: &[LayoutGlyph], bounds: impl Fn(&LayoutGlyph)->(T,T) + Copy, low: T, high: T, out: &mut Vec<usize>) {
    if entries.is_empty() { return; }
    #[cfg(test)]
    INTERVAL_VISITS.with(|visits| visits.set(visits.get()+1));
    let middle = entries.len()/2;
    let node = entries[middle];
    if bounds(&glyphs[node.max_end_glyph]).1 < low { return; }
    interval_query(&entries[..middle],glyphs,bounds,low,high,out);
    let (start,end) = bounds(&glyphs[node.glyph]);
    if start <= high {
        if end >= low { out.push(node.glyph); }
        interval_query(&entries[middle+1..],glyphs,bounds,low,high,out);
    }
}

/// A compact original-source map for a projected physical line. Separate
/// intervals may be joined by unshaped NUL separators; source gaps are never
/// copied merely because their glyphs lie far apart in logical BiDi order.
#[derive(Debug, Clone)]
pub struct ProjectedSource { segments: Vec<SourceSegment> }
#[derive(Debug, Clone)]
struct SourceSegment { projected: Range<usize>, original: Range<usize> }
impl ProjectedSource {
    pub fn source_byte(&self, byte: usize) -> Option<usize> {
        let index = self.segments.partition_point(|segment| segment.projected.end < byte);
        let segment = self.segments.get(index)?;
        (segment.projected.start <= byte && byte <= segment.projected.end)
            .then(|| segment.original.start + (byte - segment.projected.start))
    }
    fn projected_byte(&self, byte: usize) -> Option<usize> {
        let index = self.segments.partition_point(|segment| segment.original.end < byte);
        let segment = self.segments.get(index)?;
        (segment.original.start <= byte && byte <= segment.original.end)
            .then(|| segment.projected.start + (byte - segment.original.start))
    }
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
    source_visual: Vec<SourceVisual>,
    metrics: Metrics,
    width: Option<f32>,
    height: f32,
}

/// A display buffer and its original source-coordinate mapping.
#[derive(Debug)]
pub struct LayoutProjection {
    buffer: Buffer,
    source_starts: Vec<usize>,
    source_maps: Vec<ProjectedSource>,
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
                visual.push(VisualLine { physical, layout, source_start, top, height,
                    glyph_index: (row.glyphs.len() > INDEX_THRESHOLD).then(|| GlyphIndex::new(&row.glyphs)) });
                top += height;
            }
            source_start += line.text.len() + line.ending.as_str().len();
        }
        let mut source_visual: Vec<_> = visual.iter().enumerate().filter_map(|(index, row)| {
            let glyphs = &lines[row.physical].layout[row.layout].glyphs;
            Some(SourceVisual { start: row.source_start + glyphs.iter().map(|glyph| glyph.start).min()?,
                end: row.source_start + glyphs.iter().map(|glyph| glyph.end).max()?, max_end: 0, visual: index })
        }).collect();
        source_visual.sort_by_key(|row| row.start);
        let mut max_end = 0;
        for row in &mut source_visual { max_end = max_end.max(row.end); row.max_end = max_end; }
        Self { lines, visual, source_visual, metrics, width, height: top }
    }

    pub fn height(&self) -> f32 { self.height }
    pub fn visual_line_count(&self) -> usize { self.visual.len() }

    /// Owned source and layout allocation estimate, excluding font-system and
    /// allocator overhead. This is not a process RSS measurement.
    pub fn retained_bytes(&self) -> usize {
        self.visual.capacity() * core::mem::size_of::<VisualLine>()
            + self.visual.iter().filter_map(|row| row.glyph_index.as_ref()).map(|index|
                (index.x.capacity() + index.source.capacity()) * core::mem::size_of::<IntervalEntry>()).sum::<usize>()
            + self.source_visual.capacity() * core::mem::size_of::<SourceVisual>()
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
        let mut source_maps = Vec::with_capacity(range.len());
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
            source_maps.push(ProjectedSource { segments: alloc::vec![SourceSegment {
                projected: 0..end-first, original: visual.source_start+first..visual.source_start+end,
            }] });
        }
        let height = self.visual[range.clone()].iter().map(|row| row.height).sum();
        let buffer = Buffer::from_visual_layout(self.metrics, self.width, height, projected);
        Some(LayoutProjection { buffer, source_starts, source_maps, visual: range, top })
    }

    fn glyph_indices_in_x(&self, visual: usize, left: f32, right: f32) -> Option<Vec<usize>> {
        if !left.is_finite() || !right.is_finite() || right < left { return None; }
        let row = self.visual.get(visual)?;
        let glyphs = &self.lines[row.physical].layout[row.layout].glyphs;
        let mut indices = Vec::new();
        if let Some(index) = &row.glyph_index {
            interval_query(&index.x,glyphs,x_bounds,left,right,&mut indices);
        } else {
            indices.extend(glyphs.iter().enumerate().filter_map(|(i,glyph)| {
                let (start,end) = x_bounds(glyph); (start <= right && end >= left).then_some(i)
            }));
        }
        indices.sort_unstable();
        Some(indices)
    }

    /// Original glyphs intersecting placed advance bounds, returned in original
    /// draw order. Long lines use an augmented interval index, never a hidden
    /// glyph-prefix walk. Exact font outline ink bounds are not in LayoutGlyph;
    /// rendering callers must include suitable ink overscan in their rectangle.
    pub fn glyphs_in_x(&self, visual: usize, left: f32, right: f32) -> Option<impl Iterator<Item=(usize,&LayoutGlyph)> + '_> {
        let indices = self.glyph_indices_in_x(visual,left,right)?;
        let row = &self.visual[visual];
        let glyphs = &self.lines[row.physical].layout[row.layout].glyphs;
        Some(indices.into_iter().map(move |index| (row.source_start,&glyphs[index])))
    }

    /// Original glyph clusters containing an exact original-global source byte.
    /// Large rows use a source interval index, including BiDi/ligature clusters.
    pub fn glyphs_at_byte(&self, visual: usize, byte: usize) -> Option<impl Iterator<Item=(usize,&LayoutGlyph)> + '_> {
        let row = self.visual.get(visual)?;
        let glyphs = &self.lines[row.physical].layout[row.layout].glyphs;
        let local = byte.checked_sub(row.source_start)?;
        let mut indices = Vec::new();
        if let Some(index) = &row.glyph_index {
            interval_query(&index.source,glyphs,source_bounds,local,local,&mut indices);
            indices.retain(|&index| glyphs[index].start <= local && local < glyphs[index].end);
        } else {
            indices.extend(glyphs.iter().enumerate().filter_map(|(i,glyph)|
                (glyph.start <= local && local < glyph.end).then_some(i)));
        }
        indices.sort_unstable();
        Some(indices.into_iter().map(move |index| (row.source_start,&glyphs[index])))
    }

    /// Project only visual rows and placed glyph advances intersecting a
    /// horizontal rectangle. Shaping context, geometry and metadata stay exact.
    /// Plain source is packed by visible cluster intervals, so mixed-direction
    /// gaps do not copy an entire physical line. Use source_maps/source_byte;
    /// source_starts alone is not an additive map for this sparse projection.
    pub fn project_rect(&self, range: Range<usize>, left: f32, right: f32) -> Option<LayoutProjection> {
        if range.start > range.end || range.end > self.visual.len() || !left.is_finite() || !right.is_finite() || right < left { return None; }
        let top = self.visual.get(range.start).map_or(self.height,|row| row.top);
        let mut projected = Vec::with_capacity(range.len());
        let mut source_maps = Vec::with_capacity(range.len());
        let mut source_starts = Vec::with_capacity(range.len());
        for visual_index in range.clone() {
            let visual = &self.visual[visual_index];
            let source = &self.lines[visual.physical];
            let row = &source.layout[visual.layout];
            let indices = self.glyph_indices_in_x(visual_index,left,right)?;
            let mut intervals: Vec<_> = indices.iter().map(|&index| row.glyphs[index].start..row.glyphs[index].end).collect();
            intervals.sort_unstable_by_key(|range| (range.start,range.end));
            let mut merged: Vec<Range<usize>> = Vec::with_capacity(intervals.len());
            for interval in intervals {
                match merged.last_mut() {
                    Some(previous) if interval.start <= previous.end => previous.end = previous.end.max(interval.end),
                    _ => merged.push(interval),
                }
            }
            let mut text = String::new();
            let mut map = ProjectedSource { segments: Vec::with_capacity(merged.len()) };
            for interval in merged {
                if !map.segments.is_empty() { text.push('\0'); }
                let start = text.len();
                text.push_str(source.text.get(interval.clone())?);
                map.segments.push(SourceSegment { projected: start..text.len(),
                    original: visual.source_start+interval.start..visual.source_start+interval.end });
            }
            let mut glyphs = Vec::with_capacity(indices.len());
            for &index in &indices {
                let mut glyph = row.glyphs[index].clone();
                glyph.start = map.projected_byte(visual.source_start+glyph.start)?;
                glyph.end = map.projected_byte(visual.source_start+glyph.end)?;
                glyphs.push(glyph);
            }
            // Decoration spans reference glyph-vector indices, not bytes.
            // Restrict each to the selected original draw-order indices.
            let mut decorations: Vec<crate::DecorationSpan> = Vec::new();
            let mut previous = None;
            for (projected_index,&original) in indices.iter().enumerate() {
                let index = row.decorations.partition_point(|span| span.glyph_range.end <= original);
                let decoration = match row.decorations.get(index) {
                    Some(span) if span.glyph_range.contains(&original) => span,
                    _ => { previous = None; continue; }
                };
                if previous == Some(index) {
                    decorations.last_mut()?.glyph_range.end = projected_index+1;
                } else {
                    let mut clipped = decoration.clone(); clipped.glyph_range = projected_index..projected_index+1;
                    decorations.push(clipped);
                }
                previous = Some(index);
            }
            let layout = LayoutLine { w: row.w, max_ascent: row.max_ascent, max_descent: row.max_descent,
                line_height_opt: row.line_height_opt, glyphs, decorations };
            projected.push(BufferLine::from_visual_layout(text,source.rtl,layout));
            source_starts.push(map.segments.first().map_or(visual.source_start,|segment| segment.original.start));
            source_maps.push(map);
        }
        let height = self.visual[range.clone()].iter().map(|row| row.height).sum();
        let buffer = Buffer::from_visual_layout(self.metrics,self.width,height,projected);
        Some(LayoutProjection { buffer,source_starts,source_maps,visual:range,top })
    }

    /// Visual rows whose source-cluster envelope contains a byte. Candidates
    /// are found through an interval index; a caller must test actual clusters
    /// because mixed-direction visual rows may have holes in their envelopes.
    pub fn source_visual_lines(&self, byte: usize) -> impl Iterator<Item = usize> + '_ {
        let upper = self.source_visual.partition_point(|row| row.start <= byte);
        self.source_visual[..upper].iter().rev().take_while(move |row| row.max_end > byte)
            .filter(move |row| row.end > byte).map(|row| row.visual)
    }
    pub fn line_top(&self, visual: usize) -> Option<f32> { self.visual.get(visual).map(|row| row.top) }
    pub fn line_height(&self, visual: usize) -> Option<f32> { self.visual.get(visual).map(|row| row.height) }

    /// Logical source coordinate of each cluster, paired with its original
    /// glyph geometry. Useful to compare projections and to build source hits.
    pub fn glyphs(&self, visual: usize) -> Option<impl Iterator<Item = (usize, &LayoutGlyph)>> {
        let row = self.visual.get(visual)?;
        Some(self.lines[row.physical].layout[row.layout].glyphs.iter().map(move |glyph| (row.source_start, glyph)))
    }
}

impl LayoutProjection {
    pub fn buffer(&self) -> &Buffer { &self.buffer }
    pub fn source_starts(&self) -> &[usize] { &self.source_starts }
    pub fn source_maps(&self) -> &[ProjectedSource] { &self.source_maps }
    /// Consume into the ordinary buffer type for an immutable Arc/Weak renderer.
    pub fn into_buffer(self) -> Buffer { self.buffer }
    pub fn top(&self) -> f32 { self.top }
    pub fn visual_lines(&self) -> Range<usize> { self.visual.clone() }
    pub fn source_byte(&self, projected_line: usize, byte: usize) -> Option<usize> {
        let line = self.buffer.lines.get(projected_line)?;
        if byte > line.text().len() || !line.text().is_char_boundary(byte) { return None; }
        self.source_maps.get(projected_line)?.source_byte(byte)
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

    #[test]
    fn horizontal_projection_matches_full_no_wrap_context_and_source_clusters() {
        let mut fonts = FontSystem::new();
        for text in [
            format!("{}ffi office efficient", "x".repeat(2047)),
            "نص عربي English עברית שלום 123 ".repeat(200),
            "e\u{301} 👩‍🔬 a\tb office ffi ".repeat(200),
        ] {
            let mut buffer = Buffer::new(&mut fonts,Metrics::new(16.0,20.0));
            buffer.set_wrap(crate::Wrap::None);
            buffer.set_size(Some(720.0),None);
            buffer.set_text(&text,&Attrs::new().metadata(71),Shaping::Advanced,None);
            buffer.shape_until_scroll(&mut fonts,false);
            let snapshot = buffer.into_layout_snapshot().expect("complete no-wrap context");
            assert_eq!(snapshot.visual_line_count(),1);
            let original = &snapshot.lines[0].layout[0];
            for left in [0.0,original.w*0.5,(original.w-500.0).max(0.0)] {
                let right = left+500.0;
                let expected: Vec<_> = original.glyphs.iter().filter(|glyph| {
                    let (lo,hi) = x_bounds(glyph); lo<=right && hi>=left
                }).collect();
                let projection = snapshot.project_rect(0..1,left,right).unwrap();
                let actual = projection.buffer().layout_runs().next().unwrap();
                assert_eq!(actual.line_w.to_bits(),original.w.to_bits());
                assert_eq!(actual.glyphs.len(),expected.len());
                assert!(actual.glyphs.len()<256,"offscreen glyph owners escaped horizontal culling");
                for (actual,expected) in actual.glyphs.iter().zip(expected) {
                    assert_eq!((actual.font_id,actual.glyph_id,actual.level,actual.metadata),
                        (expected.font_id,expected.glyph_id,expected.level,expected.metadata));
                    assert_eq!((actual.x.to_bits(),actual.y.to_bits(),actual.w.to_bits(),actual.x_offset.to_bits(),actual.y_offset.to_bits()),
                        (expected.x.to_bits(),expected.y.to_bits(),expected.w.to_bits(),expected.x_offset.to_bits(),expected.y_offset.to_bits()));
                    assert_eq!(projection.source_byte(0,actual.start),Some(expected.start));
                    assert_eq!(projection.source_byte(0,actual.end),Some(expected.end));
                    assert_eq!(&actual_text(&projection)[actual.start..actual.end],&text[expected.start..expected.end]);
                    assert!(snapshot.glyphs_at_byte(0,expected.start).unwrap().any(|(_,glyph)| glyph.start==expected.start && glyph.end==expected.end));
                }
            }
        }
        fn actual_text(projection: &LayoutProjection) -> &str { projection.buffer().lines[0].text() }
    }

    #[test]
    fn sparse_bidi_projection_packs_source_holes_and_indexes_hidden_prefixes() {
        let mut fonts = FontSystem::new();
        let mut sample = Buffer::new(&mut fonts,Metrics::new(16.0,20.0));
        sample.set_text("x",&Attrs::new(),Shaping::Advanced,None);
        sample.shape_until_scroll(&mut fonts,false);
        let template = sample.layout_runs().next().unwrap().glyphs[0].clone();
        let glyphs: Vec<_> = (0..50_000).map(|index| {
            let mut glyph = template.clone();
            glyph.start=index; glyph.end=index+1; glyph.x=index as f32*10.0;
            glyph.w=10.0; glyph.x_offset=0.0; glyph
        }).collect();
        let row = LayoutLine { w:500_000.0,max_ascent:16.0,max_descent:4.0,line_height_opt:None,glyphs,decorations:Vec::new() };
        let mut snapshot = LayoutSnapshot::new(alloc::vec![SnapshotLine { text:"x".repeat(50_000),ending:LineEnding::None,rtl:false,layout:alloc::vec![row] }],Metrics::new(16.0,20.0),Some(720.0));
        INTERVAL_VISITS.with(|visits| visits.set(0));
        let projection = snapshot.project_rect(0..1,450_000.0,450_500.0).unwrap();
        assert!(projection.buffer().lines[0].text().len()<100);
        assert!(INTERVAL_VISITS.with(|visits| visits.get())<200,"lookup must not scan45k hidden glyphs");
        INTERVAL_VISITS.with(|visits| visits.set(0));
        assert_eq!(snapshot.glyphs_at_byte(0,45_000).unwrap().count(),1);
        assert!(INTERVAL_VISITS.with(|visits| visits.get())<100);
        // A visual slice can select logically distant clusters. Preserve both
        // boundary identities without copying the49k-byte logical envelope.
        let row = &mut snapshot.lines[0].layout[0];
        row.glyphs[0].x=450_010.0;
        snapshot.visual[0].glyph_index=Some(GlyphIndex::new(&row.glyphs));
        let projection=snapshot.project_rect(0..1,450_000.0,450_500.0).unwrap();
        assert!(projection.buffer().lines[0].text().len()<100);
        let run=projection.buffer().layout_runs().next().unwrap();
        assert_eq!(projection.source_byte(0,run.glyphs[0].start),Some(0));
        assert_eq!(projection.source_byte(0,run.glyphs[0].end),Some(1));
        let distant=run.glyphs.iter().find(|glyph| projection.source_byte(0,glyph.start)==Some(45_000)).unwrap();
        assert_eq!(projection.source_byte(0,distant.end),Some(45_001));
    }

}
