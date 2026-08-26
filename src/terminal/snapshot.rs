use std::sync::Arc;

use libghostty_vt::{
    RenderState, Terminal,
    render::{CellIterator, CursorVisualStyle, Dirty, RowIterator},
    style::{RgbColor as GhosttyRgb, Underline as GhosttyUnderline},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgb {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl From<GhosttyRgb> for Rgb {
    fn from(value: GhosttyRgb) -> Self {
        Self {
            red: value.r,
            green: value.g,
            blue: value.b,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellStyle {
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: Underline,
}

/// Inline storage for one cell's grapheme cluster.
///
/// A terminal grid is the hottest allocation site in the process: a full
/// 200x50 capture used to perform one `String` allocation per cell, ten
/// thousand per frame, and scattered every cell's text behind its own heap
/// pointer so the render loop took a cache miss per column. Every cluster a
/// terminal actually emits fits in `INLINE_CAPACITY` bytes, so the common case
/// is stored inline and `Vec<CellSnapshot>` becomes one flat contiguous array.
/// Pathological clusters (long ZWJ emoji sequences) still round-trip exactly,
/// through the spilled variant, rather than being truncated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CellText(CellTextRepr);

/// Chosen so that `CellTextRepr` costs no more than the `Box<str>` it must be
/// able to hold alongside a discriminant, making the inline case free.
const INLINE_CAPACITY: usize = 22;

#[derive(Clone, Debug, PartialEq, Eq)]
enum CellTextRepr {
    Inline { length: u8, bytes: [u8; INLINE_CAPACITY] },
    Spilled(Box<str>),
}

impl Default for CellTextRepr {
    fn default() -> Self {
        Self::Inline {
            length: 0,
            bytes: [0; INLINE_CAPACITY],
        }
    }
}

impl CellText {
    pub fn new(text: &str) -> Self {
        if text.len() <= INLINE_CAPACITY {
            let mut bytes = [0; INLINE_CAPACITY];
            bytes[..text.len()].copy_from_slice(text.as_bytes());
            Self(CellTextRepr::Inline {
                length: text.len() as u8,
                bytes,
            })
        } else {
            Self(CellTextRepr::Spilled(Box::from(text)))
        }
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            // The bytes were copied from a `&str` and the length is the same
            // one that slice reported, so this range is always a UTF-8 prefix.
            CellTextRepr::Inline { length, bytes } => {
                debug_assert!(std::str::from_utf8(&bytes[..usize::from(*length)]).is_ok());
                unsafe { std::str::from_utf8_unchecked(&bytes[..usize::from(*length)]) }
            }
            CellTextRepr::Spilled(text) => text,
        }
    }

    pub fn is_empty(&self) -> bool {
        match &self.0 {
            CellTextRepr::Inline { length, .. } => *length == 0,
            CellTextRepr::Spilled(text) => text.is_empty(),
        }
    }

    /// True when the cluster is exactly one printable ASCII scalar, the only
    /// case for which a run of cells is guaranteed to advance by exactly one
    /// cell width per glyph and may therefore be shaped as a single text run.
    pub fn is_narrow_ascii(&self) -> bool {
        matches!(self.as_str().as_bytes(), [byte] if byte.is_ascii_graphic() || *byte == b' ')
    }
}

impl From<&str> for CellText {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl PartialEq<str> for CellText {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for CellText {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl serde::Serialize for CellText {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for CellText {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = CellText;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a cell grapheme cluster")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(CellText::new(value))
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CellSnapshot {
    pub text: CellText,
    pub foreground: Rgb,
    pub background: Rgb,
    pub style: CellStyle,
    pub selected: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowSnapshot {
    pub cells: Vec<CellSnapshot>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    Bar,
    #[default]
    Block,
    Underline,
    HollowBlock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorSnapshot {
    pub column: u16,
    pub row: u16,
    pub shape: CursorShape,
    pub color: Rgb,
    pub visible: bool,
    pub blinking: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderSnapshot {
    pub generation: u64,
    pub columns: u16,
    pub rows: u16,
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Option<CursorSnapshot>,
    pub lines: Arc<[Arc<RowSnapshot>]>,
    pub detached: bool,
}

impl RenderSnapshot {
    pub fn empty() -> Self {
        Self {
            generation: 0,
            columns: 0,
            rows: 0,
            foreground: Rgb::default(),
            background: Rgb::default(),
            cursor: None,
            lines: Vec::<Arc<RowSnapshot>>::new().into(),
            detached: false,
        }
    }

    pub(crate) fn detached(generation: u64) -> Self {
        Self {
            generation,
            detached: true,
            ..Self::empty()
        }
    }
}

pub(crate) struct VisualResources {
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    previous: Option<Arc<RenderSnapshot>>,
    /// Reused across every cell of every capture. Ghostty appends the cluster
    /// to a caller-provided `String`; owning one keeps the per-cell cost at a
    /// memcpy into `CellText` instead of an allocate-and-free pair.
    grapheme_scratch: String,
}

impl VisualResources {
    pub(crate) fn new() -> Result<Self, libghostty_vt::Error> {
        Ok(Self {
            render_state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            previous: None,
            grapheme_scratch: String::new(),
        })
    }

    /// Converts Ghostty's thread-confined view into fully owned, immutable data.
    /// Both layers of Ghostty damage are cleared only after their data is copied.
    pub(crate) fn capture(
        &mut self,
        terminal: &Terminal<'static, 'static>,
        generation: u64,
        force: bool,
    ) -> Result<Option<Arc<RenderSnapshot>>, libghostty_vt::Error> {
        let snapshot = self.render_state.update(terminal)?;
        let damage = snapshot.dirty()?;
        let mut should_publish = damage != Dirty::Clean || force;

        let columns = snapshot.cols()?;
        let rows = snapshot.rows()?;
        let colors = snapshot.colors()?;
        let foreground = Rgb::from(colors.foreground);
        let background = Rgb::from(colors.background);
        let rebuild_all = force
            || damage == Dirty::Full
            || self
                .previous
                .as_ref()
                .is_none_or(|previous| previous.columns != columns || previous.rows != rows);

        let row_count = usize::from(rows);
        let mut lines = if rebuild_all {
            Vec::with_capacity(row_count)
        } else {
            self.previous
                .as_ref()
                .map(|previous| previous.lines.as_ref().to_vec())
                .unwrap_or_default()
        };

        let mut row_iteration = self.rows.update(&snapshot)?;
        let mut row_index = 0usize;
        while let Some(row) = row_iteration.next() {
            let row_dirty = row.dirty()?;
            should_publish |= row_dirty;
            if rebuild_all || row_dirty {
                let mut cell_iteration = self.cells.update(row)?;
                let mut owned_cells = Vec::with_capacity(usize::from(columns));
                while let Some(cell) = cell_iteration.next() {
                    let style = cell.style()?;
                    let mut cell_foreground = cell.fg_color()?.unwrap_or(colors.foreground);
                    let mut cell_background = cell.bg_color()?.unwrap_or(colors.background);
                    if style.inverse {
                        std::mem::swap(&mut cell_foreground, &mut cell_background);
                    }

                    self.grapheme_scratch.clear();
                    cell.graphemes_utf8(&mut self.grapheme_scratch)?;
                    owned_cells.push(CellSnapshot {
                        text: CellText::new(&self.grapheme_scratch),
                        foreground: cell_foreground.into(),
                        background: cell_background.into(),
                        style: CellStyle {
                            bold: style.bold,
                            italic: style.italic,
                            faint: style.faint,
                            blink: style.blink,
                            invisible: style.invisible,
                            strikethrough: style.strikethrough,
                            overline: style.overline,
                            underline: map_underline(style.underline),
                        },
                        selected: cell.is_selected()?,
                    });
                }

                let line = Arc::new(RowSnapshot { cells: owned_cells });
                if rebuild_all {
                    if row_index < row_count {
                        lines.push(line);
                    }
                } else if let Some(existing) = lines.get_mut(row_index) {
                    *existing = line;
                }
            }

            // Global and row damage are independent in Ghostty. Clearing one
            // never clears the other, so every visited row is acknowledged.
            row.set_dirty(false)?;
            row_index += 1;
        }

        if rebuild_all {
            // The iterator normally yields exactly `row_count` rows. Preserve the
            // prior defensive defaults if the backend ever yields fewer.
            lines.resize_with(row_count, || Arc::new(RowSnapshot::default()));
        }

        snapshot.set_dirty(Dirty::Clean)?;

        if !should_publish {
            return Ok(None);
        }

        let cursor = if snapshot.cursor_visible()? {
            if let Some(position) = snapshot.cursor_viewport()? {
                Some(CursorSnapshot {
                    column: position.x,
                    row: position.y,
                    shape: map_cursor(snapshot.cursor_visual_style()?),
                    color: snapshot
                        .cursor_color()?
                        .or(colors.cursor)
                        .unwrap_or(colors.foreground)
                        .into(),
                    visible: true,
                    blinking: snapshot.cursor_blinking()?,
                })
            } else {
                None
            }
        } else {
            None
        };

        let owned = Arc::new(RenderSnapshot {
            generation,
            columns,
            rows,
            foreground,
            background,
            cursor,
            lines: Arc::from(lines),
            detached: false,
        });
        self.previous = Some(Arc::clone(&owned));
        Ok(Some(owned))
    }
}

fn map_underline(value: GhosttyUnderline) -> Underline {
    match value {
        GhosttyUnderline::None => Underline::None,
        GhosttyUnderline::Single => Underline::Single,
        GhosttyUnderline::Double => Underline::Double,
        GhosttyUnderline::Curly => Underline::Curly,
        GhosttyUnderline::Dotted => Underline::Dotted,
        GhosttyUnderline::Dashed => Underline::Dashed,
        _ => Underline::Single,
    }
}

fn map_cursor(value: CursorVisualStyle) -> CursorShape {
    match value {
        CursorVisualStyle::Bar => CursorShape::Bar,
        CursorVisualStyle::Block => CursorShape::Block,
        CursorVisualStyle::Underline => CursorShape::Underline,
        CursorVisualStyle::BlockHollow => CursorShape::HollowBlock,
        _ => CursorShape::Block,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libghostty_vt::TerminalOptions;

    #[test]
    fn terminal_parse_produces_owned_styled_cells() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 12,
            rows: 2,
            max_scrollback: 16,
        })
        .unwrap();
        terminal.vt_write(b"plain \x1b[31mred\x1b[0m");

        let mut visual = VisualResources::new().unwrap();
        let snapshot = visual.capture(&terminal, 1, true).unwrap().unwrap();

        assert_eq!(snapshot.generation, 1);
        assert_eq!(snapshot.columns, 12);
        assert_eq!(snapshot.lines[0].cells[0].text, "p");
        assert_eq!(snapshot.lines[0].cells[6].text, "r");
        assert_ne!(snapshot.lines[0].cells[6].foreground, snapshot.foreground);
    }

    #[test]
    fn cell_text_stores_short_clusters_inline_and_long_ones_exactly() {
        let ascii = CellText::new("a");
        assert!(matches!(ascii.0, CellTextRepr::Inline { length: 1, .. }));
        assert_eq!(ascii.as_str(), "a");
        assert!(ascii.is_narrow_ascii());

        let boundary = "x".repeat(INLINE_CAPACITY);
        let inline = CellText::new(&boundary);
        assert!(matches!(inline.0, CellTextRepr::Inline { .. }));
        assert_eq!(inline.as_str(), boundary);

        // One byte past the inline budget must spill rather than truncate.
        let spilled_source = "x".repeat(INLINE_CAPACITY + 1);
        let spilled = CellText::new(&spilled_source);
        assert!(matches!(spilled.0, CellTextRepr::Spilled(_)));
        assert_eq!(spilled.as_str(), spilled_source);
    }

    #[test]
    fn cell_text_recognizes_only_single_width_ascii_as_coalescable() {
        assert!(CellText::new(" ").is_narrow_ascii());
        assert!(CellText::new("~").is_narrow_ascii());
        assert!(!CellText::new("").is_narrow_ascii());
        // Wide and multi-scalar clusters have font-dependent advances, so a run
        // containing them cannot be shaped as one text primitive.
        assert!(!CellText::new("漢").is_narrow_ascii());
        assert!(!CellText::new("é").is_narrow_ascii());
        assert!(!CellText::new("\u{1f1fa}\u{1f1f8}").is_narrow_ascii());
        assert!(!CellText::new("\t").is_narrow_ascii());
    }

    #[test]
    fn snapshot_generation_is_supplied_by_publisher() {
        let terminal = Terminal::new(TerminalOptions {
            cols: 4,
            rows: 1,
            max_scrollback: 0,
        })
        .unwrap();
        let mut visual = VisualResources::new().unwrap();

        let first = visual.capture(&terminal, 7, true).unwrap().unwrap();
        let second = visual.capture(&terminal, 8, true).unwrap().unwrap();

        assert_eq!(first.generation, 7);
        assert_eq!(second.generation, 8);
        assert!(Arc::ptr_eq(&first.lines, &second.lines) || first.lines == second.lines);
    }
}
