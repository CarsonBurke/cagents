use std::sync::Arc;

#[cfg(feature = "host")]
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

#[cfg(feature = "host")]
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
    Inline {
        length: u8,
        bytes: [u8; INLINE_CAPACITY],
    },
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
    /// The next row continues this logical line without a hard line break.
    pub wrapped: bool,
    pub hyperlinks: Vec<HyperlinkSpan>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HyperlinkSpan {
    pub start: u16,
    pub end: u16,
    pub uri: String,
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

/// Ghostty's exact viewport position, captured only with visible snapshots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScrollSnapshot {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
    pub alternate_screen: bool,
    pub mouse_tracking: bool,
    pub bracketed_paste: bool,
}

impl ScrollSnapshot {
    pub fn max_offset(self) -> u64 {
        self.total.saturating_sub(self.len)
    }

    pub fn scrollable(self) -> bool {
        !self.alternate_screen && self.len > 0 && self.max_offset() > 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderSnapshot {
    pub generation: u64,
    pub activity: Option<crate::model::TerminalOutputActivity>,
    pub columns: u16,
    pub rows: u16,
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Option<CursorSnapshot>,
    pub scroll: ScrollSnapshot,
    pub lines: Arc<[Arc<RowSnapshot>]>,
    pub detached: bool,
}

impl RenderSnapshot {
    pub fn empty() -> Self {
        Self {
            generation: 0,
            activity: None,
            columns: 0,
            rows: 0,
            foreground: Rgb::default(),
            background: Rgb::default(),
            cursor: None,
            scroll: ScrollSnapshot::default(),
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

#[cfg(feature = "host")]
pub(crate) struct VisualResources {
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    previous: Option<Arc<RenderSnapshot>>,
    /// Reused across every cell of every capture. Ghostty appends the cluster
    /// to a caller-provided `String`; owning one keeps the per-cell cost at a
    /// memcpy into `CellText` instead of an allocate-and-free pair.
    grapheme_scratch: String,
    hyperlink_scratch: Vec<u8>,
}

#[cfg(feature = "host")]
impl VisualResources {
    pub(crate) fn new() -> Result<Self, libghostty_vt::Error> {
        Ok(Self {
            render_state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            previous: None,
            grapheme_scratch: String::new(),
            hyperlink_scratch: vec![0; 4096],
        })
    }

    /// Converts Ghostty's thread-confined view into fully owned, immutable data.
    /// Both layers of Ghostty damage are cleared only after their data is copied.
    pub(crate) fn capture(
        &mut self,
        terminal: &Terminal<'static, 'static>,
        generation: u64,
        force: bool,
        activity: Option<crate::model::TerminalOutputActivity>,
    ) -> Result<Option<Arc<RenderSnapshot>>, libghostty_vt::Error> {
        let scrollbar = terminal.scrollbar()?;
        let scroll = ScrollSnapshot {
            total: scrollbar.total,
            offset: scrollbar.offset,
            len: scrollbar.len,
            alternate_screen: terminal.active_screen()? == libghostty_vt::screen::Screen::Alternate,
            mouse_tracking: terminal.is_mouse_tracking()?,
            bracketed_paste: terminal.mode(libghostty_vt::terminal::Mode::BRACKETED_PASTE)?,
        };
        let snapshot = self.render_state.update(terminal)?;
        let damage = snapshot.dirty()?;
        let mut should_publish = damage != Dirty::Clean
            || force
            || self
                .previous
                .as_ref()
                .is_some_and(|previous| previous.activity != activity || previous.scroll != scroll);

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
                let mut hyperlinks: Vec<HyperlinkSpan> = Vec::new();
                while let Some(cell) = cell_iteration.next() {
                    if cell.raw_cell()?.has_hyperlink()? {
                        let column = owned_cells.len() as u16;
                        let reference =
                            terminal.grid_ref(libghostty_vt::terminal::Point::Viewport(
                                libghostty_vt::terminal::PointCoordinate {
                                    x: column,
                                    y: row_index as u32,
                                },
                            ))?;
                        let length = match reference.hyperlink_uri(&mut self.hyperlink_scratch) {
                            Err(libghostty_vt::Error::OutOfSpace { required })
                                if required <= 65536 =>
                            {
                                self.hyperlink_scratch.resize(required, 0);
                                reference
                                    .hyperlink_uri(&mut self.hyperlink_scratch)
                                    .unwrap_or(0)
                            }
                            result => result.unwrap_or(0),
                        };
                        if let Ok(uri) = std::str::from_utf8(&self.hyperlink_scratch[..length]) {
                            if let Some(last) = hyperlinks
                                .last_mut()
                                .filter(|last| last.end == column && last.uri == uri)
                            {
                                last.end += 1;
                            } else if !uri.is_empty() {
                                hyperlinks.push(HyperlinkSpan {
                                    start: column,
                                    end: column + 1,
                                    uri: uri.to_owned(),
                                });
                            }
                        }
                    }
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

                let line = Arc::new(RowSnapshot {
                    cells: owned_cells,
                    wrapped: row.raw_row()?.is_wrapped()?,
                    hyperlinks,
                });
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
            activity,
            columns,
            rows,
            foreground,
            background,
            cursor,
            scroll,
            lines: Arc::from(lines),
            detached: false,
        });
        self.previous = Some(Arc::clone(&owned));
        Ok(Some(owned))
    }
}

#[cfg(feature = "host")]
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

#[cfg(feature = "host")]
fn map_cursor(value: CursorVisualStyle) -> CursorShape {
    match value {
        CursorVisualStyle::Bar => CursorShape::Bar,
        CursorVisualStyle::Block => CursorShape::Block,
        CursorVisualStyle::Underline => CursorShape::Underline,
        CursorVisualStyle::BlockHollow => CursorShape::HollowBlock,
        _ => CursorShape::Block,
    }
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;
    use libghostty_vt::TerminalOptions;

    #[test]
    fn capture_keeps_wrap_flags_and_coalesces_osc8_spans() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 8,
            rows: 3,
            max_scrollback: 16,
        })
        .unwrap();
        terminal.vt_write(b"\x1b]8;;https://example.com/\x1b\\abcdefghij\x1b]8;;\x1b\\\r\nnext");
        let mut visual = VisualResources::new().unwrap();
        let snapshot = visual.capture(&terminal, 1, true, None).unwrap().unwrap();
        assert!(snapshot.lines[0].wrapped);
        assert!(!snapshot.lines[1].wrapped);
        assert_eq!(
            snapshot.lines[0].hyperlinks,
            vec![HyperlinkSpan {
                start: 0,
                end: 8,
                uri: "https://example.com/".into()
            }]
        );
        assert_eq!(snapshot.lines[1].hyperlinks[0].end, 2);
    }

    #[test]
    fn scroll_metadata_tracks_output_absolute_boundaries_and_alternate_screen() {
        use libghostty_vt::terminal::ScrollViewport;
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 8,
            rows: 3,
            max_scrollback: 32,
        })
        .unwrap();
        let mut visual = VisualResources::new().unwrap();
        let initial = visual.capture(&terminal, 1, true, None).unwrap().unwrap();
        assert!(!initial.scroll.scrollable());
        for _ in 0..10 {
            terminal.vt_write(b"line\r\n");
        }
        let bottom = visual.capture(&terminal, 2, false, None).unwrap().unwrap();
        assert!(bottom.scroll.scrollable());
        assert_eq!(bottom.scroll.len, 3);
        assert_eq!(bottom.scroll.offset, bottom.scroll.max_offset());
        terminal.scroll_viewport(ScrollViewport::Row(0));
        let top = visual.capture(&terminal, 3, false, None).unwrap().unwrap();
        assert_eq!(top.scroll.offset, 0);
        terminal.scroll_viewport(ScrollViewport::Row(usize::MAX));
        let clamped = visual.capture(&terminal, 4, false, None).unwrap().unwrap();
        assert_eq!(clamped.scroll.offset, bottom.scroll.max_offset());
        terminal.vt_write(b"\x1b[?1049h\x1b[?1000h");
        let alternate = visual.capture(&terminal, 5, false, None).unwrap().unwrap();
        assert!(alternate.scroll.alternate_screen);
        assert!(alternate.scroll.mouse_tracking);
        assert!(!alternate.scroll.scrollable());
        terminal.vt_write(b"\x1b[?1000l\x1b[?1049l");
        let restored = visual.capture(&terminal, 6, false, None).unwrap().unwrap();
        assert_eq!(restored.scroll, bottom.scroll);
        assert!(visual.capture(&terminal, 7, false, None).unwrap().is_none());
    }

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
        let snapshot = visual.capture(&terminal, 1, true, None).unwrap().unwrap();

        assert_eq!(snapshot.generation, 1);
        assert_eq!(snapshot.columns, 12);
        assert_eq!(snapshot.lines[0].cells[0].text, "p");
        assert_eq!(snapshot.lines[0].cells[6].text, "r");
        assert_ne!(snapshot.lines[0].cells[6].foreground, snapshot.foreground);
    }

    #[test]
    fn osc_color_changes_repaint_defaults_and_indexed_cells_but_preserve_truecolor() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 8,
            rows: 2,
            max_scrollback: 0,
        })
        .unwrap();
        terminal.vt_write(
            b"\x1b]10;#c0c0c0\x07\x1b]11;#101010\x07\
              \x1b]4;1;#aa0000;200;#aa00aa\x07\
              D\x1b[31mA\x1b[38;5;200mI\x1b[38;2;18;52;86mT\
              \x1b[0;48;5;200mB\x1b[0;7mR\x1b[0m",
        );
        let mut visual = VisualResources::new().unwrap();
        let first = visual.capture(&terminal, 1, true, None).unwrap().unwrap();
        let rgb = |red, green, blue| Rgb { red, green, blue };
        assert_eq!(first.lines[0].cells[1].foreground, rgb(0xaa, 0, 0));
        assert_eq!(first.lines[0].cells[2].foreground, rgb(0xaa, 0, 0xaa));
        assert_eq!(first.lines[0].cells[4].background, rgb(0xaa, 0, 0xaa));
        terminal.vt_write(
            b"\x1b]10;#eeeeee\x07\x1b]11;#222222\x07\
              \x1b]4;1;#00bb00;200;#00bbbb\x07",
        );
        // No text is written and no forced capture is requested: palette/default
        // changes still have to invalidate previously resolved, cached rows.
        let changed = visual.capture(&terminal, 2, false, None).unwrap().unwrap();
        let cells = &changed.lines[0].cells;
        assert_eq!(cells[0].foreground, rgb(0xee, 0xee, 0xee));
        assert_eq!(cells[0].background, rgb(0x22, 0x22, 0x22));
        assert_eq!(cells[1].foreground, rgb(0, 0xbb, 0));
        assert_eq!(cells[2].foreground, rgb(0, 0xbb, 0xbb));
        assert_eq!(cells[3].foreground, rgb(18, 52, 86));
        assert_eq!(cells[4].background, rgb(0, 0xbb, 0xbb));
        assert_eq!(cells[5].foreground, rgb(0x22, 0x22, 0x22));
        assert_eq!(cells[5].background, rgb(0xee, 0xee, 0xee));
        assert!(visual.capture(&terminal, 3, false, None).unwrap().is_none());
    }

    #[test]
    fn incremental_repaints_preserve_untouched_rows_and_publish_late_output() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 8,
            rows: 3,
            max_scrollback: 16,
        })
        .unwrap();
        let mut visual = VisualResources::new().unwrap();
        terminal.vt_write(b"header\r\nwaiting\r\nfooter");
        let first = visual.capture(&terminal, 1, true, None).unwrap().unwrap();

        terminal.vt_write(b"\x1b[2;1H\x1b[2Kdone");
        let second = visual.capture(&terminal, 2, false, None).unwrap().unwrap();
        assert_eq!(second.lines[0].cells, first.lines[0].cells);
        assert_eq!(second.lines[2].cells, first.lines[2].cells);
        assert_eq!(second.lines[1].cells[0].text, "d");
        assert_eq!(second.lines[1].cells[3].text, "e");
        assert_ne!(second.lines[1].cells[4].text, "i");

        // More output after the previous damage was consumed must remain
        // visible without a forced refresh from mouse or keyboard input.
        terminal.vt_write(b"\x1b[3;1H\x1b[2Kfinal");
        let third = visual.capture(&terminal, 3, false, None).unwrap().unwrap();
        assert_eq!(third.lines[1].cells, second.lines[1].cells);
        assert_eq!(third.lines[2].cells[0].text, "f");
        assert_eq!(third.lines[2].cells[4].text, "l");
        assert!(visual.capture(&terminal, 4, false, None).unwrap().is_none());
    }

    #[test]
    fn a_wide_cluster_is_never_stored_as_narrow_ascii() {
        // The draw layer coalesces a run of cells into one shaped primitive
        // only where every cell is narrow ASCII, because that is the only case
        // in which each cell advances by exactly one column. A wide cluster
        // consumes two columns, so it must fail that test or the cells after it
        // would be drawn a column to the left.
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 8,
            rows: 1,
            max_scrollback: 0,
        })
        .unwrap();
        terminal.vt_write("a漢b".as_bytes());
        let mut visual = VisualResources::new().unwrap();
        let snapshot = visual.capture(&terminal, 1, true, None).unwrap().unwrap();
        let cells = &snapshot.lines[0].cells;

        assert!(cells[0].text.is_narrow_ascii(), "plain ASCII must coalesce");
        assert_eq!(cells[1].text, "漢");
        assert!(
            !cells[1].text.is_narrow_ascii(),
            "a wide cluster must be shaped on its own"
        );
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

        let first = visual.capture(&terminal, 7, true, None).unwrap().unwrap();
        let second = visual.capture(&terminal, 8, true, None).unwrap().unwrap();

        assert_eq!(first.generation, 7);
        assert_eq!(second.generation, 8);
        assert!(Arc::ptr_eq(&first.lines, &second.lines) || first.lines == second.lines);
    }
}

/// Reads an independent viewport without moving Ghostty's viewport or consuming
/// its damage. Only the worker thread can borrow the parser; all returned rows
/// own their text, styles and links.
#[cfg(feature = "host")]
pub(crate) fn capture_viewport(
    terminal: &Terminal<'static, 'static>,
    base: &RenderSnapshot,
    offset: Option<u64>,
    rows: u16,
) -> Result<Arc<RenderSnapshot>, libghostty_vt::Error> {
    use libghostty_vt::{
        style::StyleColor,
        terminal::{Point, PointCoordinate},
    };
    let mut snapshot = base.clone();
    snapshot.columns = terminal.cols()?;
    snapshot.rows = rows.max(1).min(500);
    snapshot.scroll.total = terminal.total_rows()? as u64;
    snapshot.scroll.mouse_tracking = terminal.is_mouse_tracking()?;
    snapshot.scroll.bracketed_paste =
        terminal.mode(libghostty_vt::terminal::Mode::BRACKETED_PASTE)?;
    snapshot.scroll.len = u64::from(snapshot.rows).min(snapshot.scroll.total);
    snapshot.scroll.alternate_screen =
        terminal.active_screen()? == libghostty_vt::screen::Screen::Alternate;
    snapshot.scroll.offset = offset
        .unwrap_or(snapshot.scroll.max_offset())
        .min(snapshot.scroll.max_offset());
    let palette = terminal.color_palette()?;
    let foreground = terminal
        .fg_color()?
        .map(Rgb::from)
        .unwrap_or(base.foreground);
    let background = terminal
        .bg_color()?
        .map(Rgb::from)
        .unwrap_or(base.background);
    snapshot.foreground = foreground;
    snapshot.background = background;
    let color = |value: StyleColor, default| match value {
        StyleColor::None => default,
        StyleColor::Rgb(rgb) => Rgb::from(rgb),
        StyleColor::Palette(index) => Rgb::from(palette.get(index)),
    };
    let mut lines = Vec::new();
    let mut graphemes = vec!['\0'; 32];
    let mut uri = vec![0; 1024];
    for row in snapshot.scroll.offset..snapshot.scroll.offset + snapshot.scroll.len {
        let mut cells = Vec::with_capacity(snapshot.columns as usize);
        let mut hyperlinks: Vec<HyperlinkSpan> = Vec::new();
        let mut wrapped = false;
        for column in 0..snapshot.columns {
            let reference = terminal.grid_ref(Point::Screen(PointCoordinate {
                x: column,
                y: row as u32,
            }))?;
            if column == 0 {
                wrapped = reference.row()?.is_wrapped()?;
            }
            let style = reference.style()?;
            let mut fg = color(style.fg_color, foreground);
            let mut bg = color(style.bg_color, background);
            if style.inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            let length = match reference.graphemes(&mut graphemes) {
                Err(libghostty_vt::Error::OutOfSpace { required }) => {
                    graphemes.resize(required, '\0');
                    reference.graphemes(&mut graphemes)?
                }
                result => result?,
            };
            let text = graphemes[..length].iter().collect::<String>();
            cells.push(CellSnapshot {
                text: CellText::new(&text),
                foreground: fg,
                background: bg,
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
                selected: false,
            });
            let length = match reference.hyperlink_uri(&mut uri) {
                Err(libghostty_vt::Error::OutOfSpace { required }) if required <= 65536 => {
                    uri.resize(required, 0);
                    reference.hyperlink_uri(&mut uri)?
                }
                result => result.unwrap_or(0),
            };
            if let Ok(link) = std::str::from_utf8(&uri[..length]) {
                if !link.is_empty() {
                    if let Some(last) = hyperlinks
                        .last_mut()
                        .filter(|last| last.end == column && last.uri == link)
                    {
                        last.end += 1;
                    } else {
                        hyperlinks.push(HyperlinkSpan {
                            start: column,
                            end: column + 1,
                            uri: link.into(),
                        });
                    }
                }
            }
        }
        lines.push(Arc::new(RowSnapshot {
            cells,
            wrapped,
            hyperlinks,
        }));
    }
    snapshot.lines = lines.into();
    if let Some(cursor) = &mut snapshot.cursor {
        let absolute = terminal
            .total_rows()?
            .saturating_sub(terminal.rows()? as usize) as u64
            + u64::from(terminal.cursor_y()?);
        cursor.visible &= absolute >= snapshot.scroll.offset
            && absolute < snapshot.scroll.offset + snapshot.scroll.len;
        cursor.row = absolute.saturating_sub(snapshot.scroll.offset) as u16;
        cursor.column = terminal.cursor_x()?;
    }
    Ok(Arc::new(snapshot))
}
