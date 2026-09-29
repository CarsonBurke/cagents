use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    marker::PhantomData,
    ops::Range,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

use cosmic::iced::advanced::Renderer as _;
use cosmic::iced::advanced::graphics::text::{self as graphics_text, cosmic_text, font_system};
use cosmic::iced::advanced::text::Paragraph as _;
use cosmic::{
    Element, Renderer, Theme,
    iced::{
        Border, Color, Event, Font, Length, Pixels, Point, Rectangle, Shadow, Size,
        advanced::{
            InputMethod, Layout, Shell, Widget,
            clipboard::{self, Clipboard},
            input_method, layout, mouse, renderer, text,
            widget::{Id, Operation, Tree, operation, tree},
        },
        keyboard,
        mouse::{Button as IcedMouseButton, ScrollDelta},
        window,
    },
};

use super::{
    CellSnapshot, CursorShape, KeyAction, KeyInput, KeyModifiers, MouseAction, MouseButton,
    MouseInput, PasteOrigin, RenderSnapshot, Rgb, RowSnapshot, ScrollSnapshot, TerminalKey,
    TerminalSendError, TerminalSize, TerminalSurface, Underline,
};

use crate::{
    design::{SCROLLBAR_GUTTER, SCROLLBAR_MARGIN, SCROLLBAR_THUMB_WIDTH},
    settings::TerminalCursorStyle,
};

const DEFAULT_PADDING: f32 = 8.0;

/// How long the cursor stays lit, and then dark, while it blinks. 530ms is
/// what xterm and Ghostty both default to.
const BLINK_PHASE: Duration = Duration::from_millis(530);

/// How long a terminal's grid must hold still before its PTY follows it.
///
/// Every PTY resize is a SIGWINCH, and a full-screen program answers each one
/// by repainting at the new size: Codex reflows its whole transcript. A card
/// animating to a new width crosses a column boundary on nearly every frame,
/// and a new card grows from a sliver, so following each frame would make the
/// program repaint a dozen times for one gesture, starting from one column.
/// Until the grid settles the widget keeps drawing the frame it has, clipped
/// or padded to its bounds. Kitty debounces resizes by the same interval.
const RESIZE_SETTLE: Duration = Duration::from_millis(100);

/// Opacity multiplier for the cursor of a terminal that does not hold the
/// keyboard, so that exactly one card on screen reads as the place typing
/// lands.
const UNFOCUSED_CURSOR_ALPHA: f32 = 0.45;

/// Opacity of the selection wash. The cell keeps its own background and
/// foreground underneath, so the accent has to say "selected" without making
/// the text it covers any harder to read than the text beside it.
const SELECTION_ALPHA: f32 = 0.35;

/// Rows the viewport moves for one notch of a discrete wheel by default.
///
/// A wheel reports notches rather than distance, so the scroll size gives each
/// notch a predictable number of terminal rows. The scroll-speed multiplier
/// also scales high-resolution wheel and trackpad input.
const DEFAULT_ROWS_PER_WHEEL_NOTCH: isize = 2;

/// Text metrics for one terminal grid at one scale factor.
///
/// The grid pitch is snapped to whole physical pixels so that backgrounds, the
/// cursor and mouse hit-testing all land on device pixels. A face's own advance
/// is fractional and need not match that pitch, so `faces` carries, per face,
/// the font size at which its advance comes closest to it. Drawing a face at
/// that size is what makes a whole run of cells land on the grid: see
/// [`shape_segment`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
    faces: [f32; FACE_COUNT],
    /// The family these advances were measured from, carried so that shaping
    /// cannot draw with a family the grid pitch was not measured for.
    family: &'static str,
}

/// The two style bits that select a font face.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Face {
    bold: bool,
    italic: bool,
}

impl Face {
    const PLAIN: Self = Self {
        bold: false,
        italic: false,
    };

    const fn index(self) -> usize {
        (self.bold as usize) | ((self.italic as usize) << 1)
    }

    const fn from_index(index: usize) -> Self {
        Self {
            bold: index & 1 != 0,
            italic: index & 2 != 0,
        }
    }
}

const FACE_COUNT: usize = 4;

/// Cell height as a multiple of the font size. Terminals pick a fixed ratio
/// rather than the font's own line height so that a font fallback for one row
/// cannot change the grid pitch. 1.286 keeps the historical 18px box at 14pt.
const LINE_HEIGHT_RATIO: f32 = 1.286;

pub const DEFAULT_FONT_SIZE: f32 = 14.0;

/// One glyph repeated, so that the per-glyph advance is the measured width
/// divided by the sample length. That averages away the sub-pixel rounding a
/// single glyph's bounds would carry.
const ADVANCE_SAMPLE: &str = "MMMMMMMMMMMMMMMM";

/// Samples whose per-glyph advance must match [`ADVANCE_SAMPLE`]'s for a face
/// to count as fixed-pitch: the narrowest ASCII letter and the blank, the two
/// glyphs a proportional face draws far narrower than `M`. Both are the same
/// length as `ADVANCE_SAMPLE`, so all three measurements carry the same
/// rounding.
///
/// The blank sample is bracketed by `M` rather than blank throughout so that a
/// text stack which trimmed trailing whitespace could not report it as zero
/// wide and condemn every family. Bracketing only dilutes a difference by
/// 14/16, which stays far above [`PITCH_TOLERANCE`].
const PITCH_SAMPLES: [&str; 2] = ["iiiiiiiiiiiiiiii", "M              M"];

/// How far a [`PITCH_SAMPLES`] advance may sit from `ADVANCE_SAMPLE`'s before
/// the face counts as proportional, as a fraction of the latter.
///
/// A fixed-pitch face makes the two equal - every installed monospace face
/// measures bit-identically - so the entire budget is measurement noise, and
/// the only noise on this path is a paragraph width `advance` then divides by
/// the sample length. 2% of the pitch is well clear of that at any legible
/// size and an order of magnitude below the roughly threefold `M`-to-`i`
/// spread a proportional face shows.
const PITCH_TOLERANCE: f32 = 0.02;

/// The floor under [`PITCH_TOLERANCE`]: half a pixel of paragraph width spread
/// over the sample's glyphs, the most rounding a measurement can carry.
const PITCH_TOLERANCE_FLOOR: f32 = 0.5 / ADVANCE_SAMPLE.len() as f32;

impl CellMetrics {
    /// Returns the metrics for `font_size` and `family` at `scale_factor`,
    /// measuring at most once per distinct triple for the lifetime of the
    /// process.
    ///
    /// Measuring is a shaping call and the installed faces cannot change at
    /// runtime, so the result is a pure function of its inputs. Memoising it
    /// lets `layout` and `draw` run per frame without paying for text layout.
    ///
    /// An empty `family` selects the system monospace default.
    pub fn resolve(font_size: f32, scale_factor: f32, family: &str) -> Self {
        /// Keyed by the font size and scale factor in their raw bit patterns,
        /// which are exact for the equality this cache needs, and the family
        /// name interned for the lifetime of the process.
        type MeasuredMetrics = Mutex<HashMap<(u32, u32, &'static str), CellMetrics>>;
        static MEASURED: LazyLock<MeasuredMetrics> = LazyLock::new(|| Mutex::new(HashMap::new()));

        let font_size = if font_size.is_finite() {
            font_size.clamp(1.0, 400.0)
        } else {
            DEFAULT_FONT_SIZE
        };
        let scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        let family = intern_family(family);
        let key = (font_size.to_bits(), scale_factor.to_bits(), family);
        let mut measured = MEASURED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *measured
            .entry(key)
            .or_insert_with(|| Self::measure(font_size, scale_factor, family))
    }

    fn measure(font_size: f32, scale_factor: f32, family: &'static str) -> Self {
        // Every face is measured up front: one face that is not fixed-pitch
        // disqualifies the whole family, and the same measurement then sets
        // that face's drawing size.
        let bases: [Option<f32>; FACE_COUNT] = std::array::from_fn(|index| {
            advance(ADVANCE_SAMPLE, font_size, family, Face::from_index(index))
        });

        // A family that is not fixed-pitch cannot hold the grid at all, so the
        // system monospace default is the only thing left to draw with. It is
        // never checked itself, which also bounds this recursion at one step.
        if !family.is_empty()
            && !bases.iter().enumerate().all(|(index, base)| {
                base.is_none_or(|base| {
                    is_fixed_pitch(font_size, family, Face::from_index(index), base)
                })
            })
        {
            return Self::measure(font_size, scale_factor, "");
        }

        let base = bases[Face::PLAIN.index()].unwrap_or((font_size * 0.6).max(1.0));
        let width = (base * scale_factor).round().max(1.0) / scale_factor;
        let height = (font_size * LINE_HEIGHT_RATIO * scale_factor)
            .round()
            .max(1.0)
            / scale_factor;

        let faces = std::array::from_fn(|index| {
            // Scaling the size by pitch/advance makes this face advance by
            // exactly one cell, provided its advance is linear in the size -
            // the normal case for a monospace face. A face whose advance cannot
            // be measured keeps the requested size.
            let Some(face_base) = bases[index] else {
                return font_size;
            };
            (font_size * width / face_base).clamp(1.0, 400.0)
        });

        Self {
            width,
            height,
            faces,
            family,
        }
    }

    /// The font size at which `key` advances by exactly one cell.
    fn face_size(&self, key: Face) -> f32 {
        self.faces[key.index()]
    }
}

impl Default for CellMetrics {
    fn default() -> Self {
        Self::resolve(DEFAULT_FONT_SIZE, 1.0, "")
    }
}

/// The shaped horizontal advance of one cell of `sample`, or `None` when no
/// face resolved and the measurement cannot be trusted.
fn advance(sample: &str, font_size: f32, family: &'static str, key: Face) -> Option<f32> {
    let measured = measured_advance(sample, font_size, family, key);
    (measured.is_finite() && measured >= 1.0).then_some(measured)
}

/// The mean advance of `sample`'s glyphs, whatever it measures.
fn measured_advance(sample: &str, font_size: f32, family: &'static str, key: Face) -> f32 {
    let paragraph = <Renderer as text::Renderer>::Paragraph::with_text(text::Text {
        content: sample,
        bounds: Size::new(f32::INFINITY, f32::INFINITY),
        size: Pixels(font_size),
        line_height: text::LineHeight::Relative(1.0),
        font: font(family, key),
        align_x: text::Alignment::Left,
        align_y: cosmic::iced::alignment::Vertical::Top,
        shaping: text::Shaping::Basic,
        wrapping: text::Wrapping::None,
        ellipsize: text::Ellipsize::None,
    });
    paragraph.min_bounds().width / sample.len() as f32
}

/// Whether `key` in `family` advances the narrow ASCII glyphs by `base` too,
/// `base` being the advance the grid pitch is derived from.
///
/// [`CellMetrics::measure`] only makes `ADVANCE_SAMPLE`'s glyph advance exactly
/// one cell, but [`shape_segment`] draws a coalesced run of arbitrary narrow
/// ASCII as one buffer and needs *every* glyph in it to do so - a property only
/// a fixed-pitch face has. Nothing else establishes it: `fontdb` matches no
/// face for a name it does not know and cosmic-text then falls back to the
/// first face of its globally sorted match list, so a proportional family, a
/// mistyped monospace one, or a monospace one whose bold face is simply not
/// installed all still measure a finite advance. The grid would then shear
/// silently, glyphs drifting left of their columns while the backgrounds,
/// selection, decorations and cursor stayed on the pitch.
fn is_fixed_pitch(font_size: f32, family: &'static str, key: Face, base: f32) -> bool {
    let tolerance = (base * PITCH_TOLERANCE).max(PITCH_TOLERANCE_FLOOR);
    PITCH_SAMPLES.iter().all(|sample| {
        let measured = measured_advance(sample, font_size, family, key);
        // A measurement that is not finite is evidence of nothing, so the
        // family keeps the benefit of the doubt, exactly as it does when
        // `ADVANCE_SAMPLE` itself cannot be measured.
        !measured.is_finite() || (measured - base).abs() <= tolerance
    })
}

fn font(family: &'static str, key: Face) -> Font {
    Font {
        family: if family.is_empty() {
            Font::MONOSPACE.family
        } else {
            cosmic::iced::font::Family::Name(family)
        },
        weight: if key.bold {
            cosmic::iced::font::Weight::Bold
        } else {
            cosmic::iced::font::Weight::Normal
        },
        style: if key.italic {
            cosmic::iced::font::Style::Italic
        } else {
            cosmic::iced::font::Style::Normal
        },
        ..Font::MONOSPACE
    }
}

/// Interns a family name for the lifetime of the process, mapping an empty name
/// to the system monospace default.
///
/// iced names a family with a `&'static str`, but ours comes from a settings
/// file at runtime and `view` rebuilds the widget on every frame: leaking there
/// would leak per frame. Each distinct name is therefore leaked exactly once,
/// so repeated frames - and an edit back to a name already seen - allocate
/// nothing. The set of families a user types is bounded in practice.
pub(crate) fn monospace_font(family: &str) -> Font {
    font(
        intern_family(family),
        Face {
            bold: false,
            italic: false,
        },
    )
}

fn intern_family(family: &str) -> &'static str {
    static INTERNED: LazyLock<Mutex<HashSet<&'static str>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));

    let family = family.trim();
    if family.is_empty() {
        return "";
    }
    let mut interned = INTERNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(&existing) = interned.get(family) {
        return existing;
    }
    let leaked: &'static str = String::from(family).leak();
    interned.insert(leaked);
    leaked
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TerminalGeometry {
    content_bounds: Rectangle,
    grid_bounds: Rectangle,
    columns: u16,
    rows: u16,
    cell_width: f32,
    cell_height: f32,
    cell_width_px: u32,
    cell_height_px: u32,
    scale_factor: f32,
    metrics: CellMetrics,
}

impl TerminalGeometry {
    fn new(
        bounds: Rectangle,
        font_size: f32,
        scale_factor: f32,
        family: &str,
        line_height: f32,
        padding: f32,
    ) -> Self {
        let scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        // `resolve` already snapped the pitch to whole physical pixels at this
        // scale, so these conversions are exact rather than a second rounding.
        let mut metrics = CellMetrics::resolve(font_size, scale_factor, family);
        // Line spacing does not change font advances, so reuse the measured
        // faces and only replace their line box. Shaping keys this height too.
        metrics.height = (font_size * line_height * scale_factor).round().max(1.0) / scale_factor;
        let cell_width_px = (metrics.width * scale_factor).round().max(1.0) as u32;
        let cell_height_px = (metrics.height * scale_factor).round().max(1.0) as u32;
        let cell_width = cell_width_px as f32 / scale_factor;
        let cell_height = cell_height_px as f32 / scale_factor;

        // Size the PTY from the card's stable logical extent. Snapping both
        // absolute edges would make the available physical width depend on
        // the card's fractional x/y position and reflow while panning.
        // Reserve the rail even when hidden: focusing or gaining history must
        // not resize the PTY, and no glyph or mouse cell may occupy this gutter.
        let content_width = (bounds.width - padding * 2.0 - SCROLLBAR_GUTTER).max(0.0);
        let content_height = (bounds.height - padding * 2.0).max(0.0);
        let columns = ((content_width * scale_factor / cell_width_px as f32).floor() as u16).max(1);
        let rows = ((content_height * scale_factor / cell_height_px as f32).floor() as u16).max(1);

        // Absolute snapping still gives drawing and input a pixel-aligned
        // origin and a conservative clip. The fixed-size grid may lose a
        // fractional edge pixel to this clip, but its PTY extent stays fixed.
        let left = ((bounds.x + padding) * scale_factor).ceil() / scale_factor;
        let top = ((bounds.y + padding) * scale_factor).ceil() / scale_factor;
        let right = ((bounds.x + bounds.width - padding - SCROLLBAR_GUTTER) * scale_factor).floor()
            / scale_factor;
        let bottom = ((bounds.y + bounds.height - padding) * scale_factor).floor() / scale_factor;
        let content_bounds = Rectangle {
            x: left,
            y: top,
            width: (right - left).max(0.0),
            height: (bottom - top).max(0.0),
        };
        let grid_bounds = Rectangle {
            x: content_bounds.x,
            y: content_bounds.y,
            width: f32::from(columns) * cell_width,
            height: f32::from(rows) * cell_height,
        };

        Self {
            content_bounds,
            grid_bounds,
            columns,
            rows,
            cell_width,
            cell_height,
            cell_width_px,
            cell_height_px,
            scale_factor,
            metrics,
        }
    }

    fn terminal_size(self) -> TerminalSize {
        TerminalSize {
            columns: self.columns,
            rows: self.rows,
            cell_width_px: self.cell_width_px,
            cell_height_px: self.cell_height_px,
        }
    }

    fn screen_width_px(self) -> u32 {
        u32::from(self.columns).saturating_mul(self.cell_width_px)
    }

    fn screen_height_px(self) -> u32 {
        u32::from(self.rows).saturating_mul(self.cell_height_px)
    }

    fn cursor_position(self, cursor: mouse::Cursor) -> Option<Point> {
        let visible_grid = self.grid_bounds.intersection(&self.content_bounds)?;
        cursor.position_in(visible_grid)
    }

    /// The grid cell a pointer position - as returned by
    /// [`Self::cursor_position`] - falls in.
    ///
    /// Mouse motion is deduplicated on this: every reporting format but
    /// pixel-resolution SGR (mode 1016) encodes motion inside one cell
    /// identically, so only a cell crossing is worth a frame. A child using
    /// 1016 therefore sees cell-quantised motion, which is the trade that
    /// keeps a hover off the bounded command channel entirely.
    fn cell_at(self, position: Point) -> (u16, u16) {
        let column = (position.x / self.cell_width) as u32;
        let row = (position.y / self.cell_height) as u32;
        (
            column.min(u32::from(self.columns.saturating_sub(1))) as u16,
            row.min(u32::from(self.rows.saturating_sub(1))) as u16,
        )
    }

    /// [`Self::cell_at`], limited to a published grid of `columns` x `rows`.
    ///
    /// While a resize is pending the snapshot still has the old size, so a
    /// cell past its edge would name text that is not there - the draw path
    /// clips to the same bound.
    fn cell_within(self, position: Point, columns: u16, rows: u16) -> (u16, u16) {
        let (column, row) = self.cell_at(position);
        (
            column.min(columns.saturating_sub(1)),
            row.min(rows.saturating_sub(1)),
        )
    }

    /// The clipped rectangle covering `length` consecutive cells starting at
    /// `column`.
    ///
    /// The grid's left edge coincides with `content_bounds`, so this clip can
    /// only ever trim the right edge — never shift the origin — which is what
    /// lets a text run be positioned from `x` alone.
    fn run_bounds(self, column: u16, length: u16, row: u16) -> Option<Rectangle> {
        if length == 0 || column >= self.columns || row >= self.rows {
            return None;
        }
        let length = length.min(self.columns - column);
        Rectangle {
            x: self.grid_bounds.x + f32::from(column) * self.cell_width,
            y: self.grid_bounds.y + f32::from(row) * self.cell_height,
            width: f32::from(length) * self.cell_width,
            height: self.cell_height,
        }
        .intersection(&self.content_bounds)
    }

    fn cell_bounds(self, column: u16, row: u16) -> Option<Rectangle> {
        self.run_bounds(column, 1, row)
    }

    /// The draw position for a shaped buffer whose first cell is `cell_bounds`.
    ///
    /// A raw `cosmic_text` buffer is drawn from its top-left corner, and its
    /// line box is exactly one cell high, so the cell's own corner is the
    /// anchor. Ascenders stay inside the row rather than clipping above it.
    fn text_origin(self, cell_bounds: Rectangle) -> Point {
        Point::new(cell_bounds.x, cell_bounds.y)
    }
}

/// A focus-revealed scrollbar in the permanently reserved right-hand gutter.
#[derive(Clone, Copy, Debug)]
struct TerminalScrollbar {
    track: Rectangle,
    thumb: Rectangle,
    max_offset: u64,
}

impl TerminalScrollbar {
    /// The rail's geometry, whenever the viewport has history to move through.
    ///
    /// Whether it is painted at all is a separate question: see
    /// [`Self::visible`].
    fn new(bounds: Rectangle, scroll: super::ScrollSnapshot) -> Option<Self> {
        if !scroll.scrollable() || bounds.height <= 0.0 || bounds.width <= 0.0 {
            return None;
        }
        let track = Rectangle {
            x: bounds.x + (bounds.width - SCROLLBAR_GUTTER).max(0.0),
            width: bounds.width.min(SCROLLBAR_GUTTER),
            ..bounds
        };
        let height = (track.height * (scroll.len as f64 / scroll.total as f64) as f32)
            .max(24.0)
            .min(track.height);
        let max_offset = scroll.max_offset();
        let thumb = Rectangle {
            x: track.x + SCROLLBAR_MARGIN.min(track.width / 2.0),
            width: SCROLLBAR_THUMB_WIDTH.min((track.width - 2.0 * SCROLLBAR_MARGIN).max(0.0)),
            y: track.y
                + (track.height - height)
                    * (scroll.offset.min(max_offset) as f64 / max_offset as f64) as f32,
            height,
            ..track
        };
        Some(Self {
            track,
            thumb,
            max_offset,
        })
    }

    fn row_at(self, pointer_y: f32, grab_offset: f32) -> u64 {
        let travel = self.track.height - self.thumb.height;
        if travel <= 0.0 {
            return 0;
        }
        let fraction = ((pointer_y - self.track.y - grab_offset) / travel).clamp(0.0, 1.0);
        (fraction as f64 * self.max_offset as f64).round() as u64
    }

    /// Whether the rail is painted.
    ///
    /// The focused terminal always shows it; an unfocused one only while its
    /// viewport sits above the live bottom, so that a card left in scrollback
    /// says so instead of looking like it is following the child.
    fn visible(scroll: super::ScrollSnapshot, focused: bool) -> bool {
        focused || scroll.offset < scroll.max_offset()
    }
}

/// One cell in scrollback coordinates: an absolute row of the child's history
/// and a grid column.
///
/// The viewport moves under a selection - a drag past the edge scrolls it, and
/// so does the child's own output - so a viewport row would silently come to
/// name different text. Ordering is reading order, which is what lets a drag
/// be normalized by comparing its two ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SelectionCell {
    row: u64,
    column: u16,
}

/// What a press selects, and what a drag after it keeps snapping to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionMode {
    Character,
    Word,
    Line,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    anchor: SelectionCell,
    head: SelectionCell,
    mode: SelectionMode,
}

impl Selection {
    fn new(cell: SelectionCell, mode: SelectionMode) -> Self {
        Self {
            anchor: cell,
            head: cell,
            mode,
        }
    }

    /// The first and last cell the selection covers, inclusive, in reading
    /// order.
    ///
    /// Both ends are expanded against the live grid rather than stored
    /// expanded, so a drag that began on a double click keeps snapping to
    /// whole words as it grows.
    fn span(self, grid: SelectionGrid<'_>) -> (SelectionCell, SelectionCell) {
        let (mut start, mut end) = if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        match self.mode {
            SelectionMode::Character => {}
            SelectionMode::Word => {
                start.column = grid.word_bounds(start).0;
                end.column = grid.word_bounds(end).1;
            }
            SelectionMode::Line => {
                start.column = 0;
                end.column = grid.columns.saturating_sub(1);
            }
        }
        (start, end)
    }
}

/// The grid a selection resolves against: the snapshot's rows, and the
/// absolute row its first line holds.
#[derive(Clone, Copy)]
struct SelectionGrid<'a> {
    rows: &'a [Arc<RowSnapshot>],
    offset: u64,
    columns: u16,
}

impl<'a> SelectionGrid<'a> {
    /// The cells of an absolute row, when the snapshot holds that row.
    ///
    /// History the capture does not reach has no text here, which is the one
    /// place a selection stops resolving.
    fn cells(self, row: u64) -> Option<&'a [CellSnapshot]> {
        let index = usize::try_from(row.checked_sub(self.offset)?).ok()?;
        let cells = &self.rows.get(index)?.cells;
        Some(&cells[..cells.len().min(usize::from(self.columns))])
    }

    /// Last occupied display cell, including the tail of a wide grapheme.
    fn wrapped(self, row: u64) -> bool {
        row.checked_sub(self.offset)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| self.rows.get(index))
            .is_some_and(|row| row.wrapped)
    }

    /// Last occupied display cell, including the tail of a wide grapheme.
    fn content_end(self, row: u64) -> Option<u16> {
        let cells = self.cells(row)?;
        if self.wrapped(row) {
            return u16::try_from(cells.len().checked_sub(1)?).ok();
        }
        let (column, cell) = cells
            .iter()
            .enumerate()
            .rfind(|(_, cell)| !cell.text.as_str().trim().is_empty())?;
        Some(
            (column + unicode_width::UnicodeWidthStr::width(cell.text.as_str()).max(1) - 1)
                .min(cells.len().saturating_sub(1)) as u16,
        )
    }

    fn cluster_start(self, cell: SelectionCell) -> u16 {
        let Some(cells) = self.cells(cell.row) else {
            return cell.column;
        };
        let Some((column, glyph)) = cells
            .iter()
            .enumerate()
            .take(usize::from(cell.column) + 1)
            .rfind(|(_, glyph)| !glyph.text.is_empty())
        else {
            return cell.column;
        };
        if column + unicode_width::UnicodeWidthStr::width(glyph.text.as_str())
            > usize::from(cell.column)
        {
            column as u16
        } else {
            cell.column
        }
    }

    /// Blank rows and padding beyond the last glyph are not selectable text.
    /// Indentation and spaces within a row's output remain valid anchors.
    fn selection_start(self, cell: SelectionCell) -> Option<SelectionCell> {
        // Keep the pointed cell as the gesture anchor. Snapping a wide tail
        // to its lead here would mistake a leftward drag across that glyph
        // for a click; drawing and copying expand clusters separately.
        (cell.column <= self.content_end(cell.row)?).then_some(cell)
    }

    /// The selected part of a row, excluding unused cells beyond its output.
    fn selected_columns(
        self,
        span: (SelectionCell, SelectionCell),
        row: u64,
    ) -> Option<(u16, u16)> {
        let (first, last) = selection_columns(span, row, self.columns)?;
        let first = self.cluster_start(SelectionCell { row, column: first });
        let end = self.content_end(row)?;
        let cluster = self.cluster_start(SelectionCell { row, column: last });
        let width = self
            .cells(row)?
            .get(usize::from(cluster))
            .map_or(1, |cell| {
                unicode_width::UnicodeWidthStr::width(cell.text.as_str()).max(1)
            }) as u16;
        let last = last.max(cluster.saturating_add(width - 1)).min(end);
        (first <= last).then_some((first, last))
    }

    /// Bound copied output to captured rows with selected text. Blank lines
    /// between them are preserved; unused rows above or below are not copied.
    fn text_span(
        self,
        span: (SelectionCell, SelectionCell),
    ) -> Option<(SelectionCell, SelectionCell)> {
        let mut first = None;
        let mut last = None;
        for index in 0..self.rows.len() {
            let row = self.offset.saturating_add(index as u64);
            if let Some((start, end)) = self.selected_columns(span, row) {
                first.get_or_insert(SelectionCell { row, column: start });
                last = Some(SelectionCell { row, column: end });
            }
        }
        Some((first?, last?))
    }

    /// The run of like cells around `cell`: the word under a double click, or
    /// the blanks between two words.
    fn word_bounds(self, cell: SelectionCell) -> (u16, u16) {
        let Some(cells) = self.cells(cell.row).filter(|cells| !cells.is_empty()) else {
            return (cell.column, cell.column);
        };
        let index = usize::from(cell.column).min(cells.len() - 1);
        // A wide cluster's trailing cell carries no text of its own, so it
        // takes the class of the cluster it completes rather than ending the
        // run halfway through a glyph.
        let class = (0..=index)
            .rev()
            .find_map(|index| word_class(&cells[index]))
            .unwrap_or(WordClass::Symbol);
        let extends =
            |cell: &CellSnapshot| word_class(cell).is_none_or(|candidate| candidate == class);
        let mut start = index;
        while start > 0 && extends(&cells[start - 1]) {
            start -= 1;
        }
        let mut end = index;
        while end + 1 < cells.len() && extends(&cells[end + 1]) {
            end += 1;
        }
        (start as u16, end as u16)
    }
}

/// The punctuation a double click keeps inside one word. What a user reaches
/// for in an agent's terminal is a path, a flag, a package name or a
/// `file:line` reference, none of which survive a word rule that stops at
/// every symbol.
const WORD_PUNCTUATION: [char; 7] = ['_', '-', '.', '/', '+', '~', ':'];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WordClass {
    Word,
    Blank,
    Symbol,
}

/// A cell's word class, or `None` for a cell carrying no text of its own: the
/// trailing cell of a wide cluster, which [`segments`] skips for the same
/// reason.
fn word_class(cell: &CellSnapshot) -> Option<WordClass> {
    match cell.text.as_str().chars().next() {
        None => None,
        Some(character) if character.is_alphanumeric() || WORD_PUNCTUATION.contains(&character) => {
            Some(WordClass::Word)
        }
        Some(character) if character.is_whitespace() => Some(WordClass::Blank),
        Some(_) => Some(WordClass::Symbol),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionRouting {
    Local,
    /// Preserve agent clicks, while ordinary drags select its output locally.
    ClickOrSelect,
    Application,
}

/// Keep motion in the same layout coordinates as the initial press. Containers
/// may translate the cursor while preserving the raw window-space event.
fn drag_position(cursor: mouse::Cursor, raw: Point) -> Point {
    cursor.position().unwrap_or(raw)
}

fn selection_routing(
    read_only: bool,
    mouse_tracking: bool,
    agent: bool,
    modifiers: KeyModifiers,
) -> SelectionRouting {
    let unmodified = !modifiers.control && !modifiers.alt && !modifiers.super_key;
    if read_only || modifiers.shift || (!mouse_tracking && unmodified) {
        SelectionRouting::Local
    } else if agent && unmodified {
        SelectionRouting::ClickOrSelect
    } else {
        SelectionRouting::Application
    }
}

/// The first and last column a selection covers on one absolute row, or `None`
/// when the row falls outside it.
///
/// A multi-row selection runs from its anchor column to the end of that row,
/// covers the rows between whole, and stops at its head column on the last.
fn selection_columns(
    (start, end): (SelectionCell, SelectionCell),
    row: u64,
    columns: u16,
) -> Option<(u16, u16)> {
    if row < start.row || row > end.row {
        return None;
    }
    let last = columns.checked_sub(1)?;
    let first = if row == start.row {
        start.column.min(last)
    } else {
        0
    };
    let final_column = if row == end.row {
        end.column.min(last)
    } else {
        last
    };
    (first <= final_column).then_some((first, final_column))
}

/// Renders a selection span to text.
///
/// Each row is trimmed at its right edge before the rows are joined: a grid
/// row is space-padded to its full width, and a copied command carrying sixty
/// trailing blanks is useless wherever it is pasted.
fn selection_text(grid: SelectionGrid<'_>, span: (SelectionCell, SelectionCell)) -> String {
    let Some(span) = grid.text_span(span) else {
        return String::new();
    };
    let mut text = String::new();
    for row in span.0.row..=span.1.row {
        if row > span.0.row && !grid.wrapped(row - 1) {
            text.push('\n');
        }
        let Some((first, last)) = grid.selected_columns(span, row) else {
            continue;
        };
        let Some(cells) = grid.cells(row) else {
            continue;
        };
        let line = text.len();
        let mut covered_until = usize::from(first);
        for (column, cell) in cells
            .iter()
            .enumerate()
            .take(usize::from(last) + 1)
            .skip(usize::from(first))
        {
            if !cell.text.is_empty() {
                text.push_str(cell.text.as_str());
                covered_until =
                    column + unicode_width::UnicodeWidthStr::width(cell.text.as_str()).max(1);
            } else if column >= covered_until {
                // Cursor-addressed TUIs can leave untouched cells between
                // words. Those are spaces; only wide-grapheme tails are
                // omitted, otherwise copying would join separate words.
                text.push(' ');
            }
        }
        if !grid.wrapped(row) {
            text.truncate(line + text[line..].trim_end().len());
        }
    }
    text
}

/// What the PTY should do about the grid size the latest layout produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeSync {
    /// The PTY already has this size.
    Current,
    /// The size has held still long enough: resize the PTY to it.
    Apply(TerminalSize),
    /// The size is still moving. Look again at this instant.
    Settle(Instant),
}

/// Holds PTY resizes back until the grid stops changing size.
#[derive(Debug, Default)]
struct PtySize {
    /// The size last sent to the PTY. `None` before the first resize.
    applied: Option<TerminalSize>,
    /// The newest size that differs from `applied`, and when layout first
    /// produced it. Any other size restarts the wait.
    pending: Option<(TerminalSize, Instant)>,
}

impl PtySize {
    fn observe(&mut self, size: TerminalSize, now: Instant) -> SizeSync {
        if self.applied == Some(size) {
            self.pending = None;
            return SizeSync::Current;
        }
        let since = match self.pending {
            Some((pending, since)) if pending == size => since,
            _ => {
                self.pending = Some((size, now));
                now
            }
        };
        let due = since + RESIZE_SETTLE;
        if now >= due {
            SizeSync::Apply(size)
        } else {
            SizeSync::Settle(due)
        }
    }

    fn applied(&mut self, size: TerminalSize) {
        self.applied = Some(size);
        self.pending = None;
    }
}

#[derive(Default)]
struct WidgetState {
    history_total: u64,
    completed_read_only: bool,
    shortcut_keys: Vec<keyboard::key::Physical>,
    select_all_request: u64,
    selection_reset: u64,
    pty_size: PtySize,
    input_focused: bool,
    focused: bool,
    focus_requested: bool,
    pressed_buttons: u8,
    modifiers: KeyModifiers,
    last_cursor: Option<Point>,
    scrollbar_drag: Option<f32>,
    wheel_remainder: f32,
    /// The viewport move the frame being composed owes this terminal, folded
    /// across every wheel event, auto-scroll step, scrollbar sample and paging
    /// key since the last redraw. `None` while nothing is owed.
    owed_scroll: Option<ViewportScroll>,
    /// The grid cell the last reported motion landed in.
    last_motion_cell: Option<(u16, u16)>,
    /// When the current cursor-blink phase began, on the frame clock. `None`
    /// while nothing is blinking.
    blink_since: Option<Instant>,
    /// Whether the blinking cursor is currently in its dark phase. The default
    /// is lit, so a cursor that has never blinked is drawn.
    cursor_dark: bool,
    /// The cell the blink phase is anchored to.
    blink_anchor: Option<(u16, u16)>,
    /// Text being composed by the input method, held so the shell can paint it
    /// at the cursor until it is committed.
    preedit: Option<input_method::Preedit>,
    /// The active selection, in absolute scrollback coordinates. `None` means
    /// nothing is selected.
    selection: Option<Selection>,
    /// Whether the pointer is currently extending `selection`.
    selecting: bool,
    /// An agent receives an ordinary click, but a drag belongs to local text
    /// selection. Defer its press until release so a selection never also
    /// moves the agent's cursor or activates a control.
    selection_click: Option<Point>,
    /// The previous left press, so that a second and third one in the same
    /// place count as a double and triple click.
    last_click: Option<mouse::Click>,
    /// Whether the last `Escape` press was spent clearing a selection, so that
    /// its release is withheld from the child too.
    escape_cleared: bool,
    /// The screen the selection was made against, so that switching to or from
    /// the alternate screen drops it.
    alternate_screen: bool,
    /// Shaped text, reused across frames. Mutated through `&self` because
    /// `draw` is the only place that knows which rows are actually visible.
    shaped: RefCell<ShapeCache>,
}

impl operation::Focusable for WidgetState {
    fn is_focused(&self) -> bool {
        self.input_focused
    }

    fn focus(&mut self) {
        self.input_focused = true;
    }

    fn unfocus(&mut self) {
        self.input_focused = false;
    }
}

impl WidgetState {
    fn begin_selection(
        &mut self,
        grid: SelectionGrid<'_>,
        cell: SelectionCell,
        position: Point,
        defer_click: bool,
    ) {
        let click = mouse::Click::new(position, IcedMouseButton::Left, self.last_click);
        self.selection = grid.selection_start(cell).map(|cell| {
            Selection::new(
                cell,
                match click.kind() {
                    mouse::click::Kind::Single => SelectionMode::Character,
                    mouse::click::Kind::Double => SelectionMode::Word,
                    mouse::click::Kind::Triple => SelectionMode::Line,
                },
            )
        });
        self.selection_click =
            (defer_click && click.kind() == mouse::click::Kind::Single).then_some(position);
        self.last_click = self.selection.is_some().then_some(click);
        self.selecting = self.selection.is_some() || self.selection_click.is_some();
    }

    fn extend_selection(&mut self, cell: SelectionCell) {
        self.selection_click = None;
        if let Some(selection) = &mut self.selection {
            selection.head = cell;
        }
    }

    /// Advances the cursor-blink phase to the frame instant `now` and returns
    /// the instant the next phase begins.
    ///
    /// The phase is driven from the frame clock and one scheduled redraw per
    /// phase rather than a frame subscription, so a terminal sitting at an idle
    /// prompt wakes the compositor twice a second instead of sixty times - and
    /// an unfocused or steady-cursor terminal, which never calls this, wakes it
    /// not at all.
    fn advance_blink(&mut self, now: Instant, anchor: Option<(u16, u16)>) -> Instant {
        // Frames stop entirely while a terminal is unfocused or occluded, so a
        // resumed cycle - or a cursor that just moved under the user's typing -
        // restarts lit rather than folding an unbounded number of phases.
        let since = match self.blink_since {
            Some(since)
                if self.blink_anchor == anchor
                    && now.saturating_duration_since(since) < BLINK_PHASE * 2 =>
            {
                since
            }
            _ => {
                self.blink_anchor = anchor;
                self.cursor_dark = false;
                self.blink_since = Some(now);
                now
            }
        };
        if now.saturating_duration_since(since) >= BLINK_PHASE {
            self.cursor_dark = !self.cursor_dark;
            self.blink_since = Some(since + BLINK_PHASE);
            return since + BLINK_PHASE * 2;
        }
        since + BLINK_PHASE
    }

    /// Converts one wheel event's `travel` into the work it implies, carrying
    /// the sub-unit remainder so that a high-resolution trackpad keeps moving
    /// continuously instead of in whole-notch jumps.
    ///
    /// The configured speed scales viewport and alternate-screen movement.
    /// Mouse-reporting applications remain unscaled because they count
    /// detents themselves.
    ///
    /// Terminal input is never folded, because a child counts what it
    /// receives, so the two input cases return a count for the caller to send
    /// one by one. A viewport move has no such observer and is returned for
    /// folding.
    fn wheel_effect(
        &mut self,
        travel: WheelTravel,
        scroll: ScrollSnapshot,
        scroll_size: isize,
        scroll_speed: f32,
    ) -> WheelEffect {
        let speed = if scroll_speed.is_finite() {
            scroll_speed.max(0.25)
        } else {
            1.0
        };
        let reporting = scroll.mouse_tracking && !self.modifiers.shift;
        let units = match travel {
            WheelTravel::Notches(notches) if !reporting => notches * scroll_size as f32 * speed,
            WheelTravel::Rows(rows) if !reporting => rows * speed,
            WheelTravel::Notches(units) | WheelTravel::Rows(units) => units,
        };
        self.wheel_remainder -= units;
        let delta = (self.wheel_remainder.trunc() as isize).clamp(-128, 128);
        self.wheel_remainder -= delta as f32;
        if delta == 0 {
            return WheelEffect::Idle;
        }
        let rows = delta;
        if self.modifiers.shift {
            return WheelEffect::Viewport(rows);
        }
        let up = delta < 0;
        if scroll.mouse_tracking {
            WheelEffect::Report {
                button: if up {
                    MouseButton::WheelUp
                } else {
                    MouseButton::WheelDown
                },
                notches: delta.unsigned_abs() as u32,
            }
        } else if scroll.alternate_screen {
            WheelEffect::Keys {
                key: if up {
                    TerminalKey::ArrowUp
                } else {
                    TerminalKey::ArrowDown
                },
                presses: rows.unsigned_abs() as u32,
            }
        } else {
            WheelEffect::Viewport(rows)
        }
    }

    /// Folds `next` into the viewport move this frame already owes, returning
    /// an earlier move that cannot be folded into it and so has to be sent
    /// first.
    ///
    /// Deltas add. A newer absolute destination discards everything before it,
    /// since nothing in between was ever presented. A delta after an absolute
    /// destination cannot be folded at all, because only the terminal knows
    /// which row that destination clamped to, so the pair is sent in order
    /// rather than guessed at.
    fn owe_scroll(&mut self, next: ViewportScroll) -> Option<ViewportScroll> {
        match (self.owed_scroll, next) {
            (Some(ViewportScroll::Lines(owed)), ViewportScroll::Lines(delta)) => {
                self.owed_scroll = Some(ViewportScroll::Lines(owed.saturating_add(delta)));
                None
            }
            (owed, ViewportScroll::Lines(_)) => {
                self.owed_scroll = Some(next);
                owed
            }
            _ => {
                self.owed_scroll = Some(next);
                None
            }
        }
    }

    /// Takes the viewport move owed to the frame now being drawn, if any.
    fn take_owed_scroll(&mut self) -> Option<ViewportScroll> {
        self.owed_scroll.take()
    }
}

/// The inputs that invalidate every shaped buffer at once.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ShapeParams {
    font_size: u32,
    scale_factor: u32,
    cell_height: u32,
    family: &'static str,
    columns: u16,
    fonts: graphics_text::Version,
}

/// One row's shaped segments, keyed by the row it was shaped from.
///
/// Pointer identity cheaply recognizes untouched rows. Content equality also
/// recognizes rows reconstructed by IPC after moving within the viewport.
#[derive(Default)]
struct ShapedRow {
    source: Option<Arc<RowSnapshot>>,
    segments: Vec<ShapedSegment>,
}

struct ShapedSegment {
    column: u16,
    /// Held here so the `Weak` handed to the renderer stays live for the frame.
    buffer: Arc<cosmic_text::Buffer>,
}

#[derive(Default)]
struct ShapeCache {
    params: Option<ShapeParams>,
    offset: u64,
    rows: Vec<ShapedRow>,
    /// Scratch reused while shaping, so a cache miss allocates nothing extra.
    content: String,
    spans: Vec<(Range<usize>, cosmic_text::Attrs<'static>)>,
}

impl ShapeCache {
    /// Align buffers with the new viewport before checking their contents.
    /// Rotation only proposes reuse: equality below validates every candidate,
    /// including wrapped entries, output changes, and scrollback eviction.
    fn scroll_to(&mut self, offset: u64) {
        if !self.rows.is_empty() {
            let distance = offset.abs_diff(self.offset);
            if distance < self.rows.len() as u64 {
                if offset > self.offset {
                    self.rows.rotate_left(distance as usize);
                } else {
                    self.rows.rotate_right(distance as usize);
                }
            }
        }
        self.offset = offset;
    }

    /// Brings the cache up to date for the visible rows and returns nothing to
    /// draw for rows that shape to no glyphs.
    ///
    /// Rows the daemon left untouched keep their buffers, so a terminal that is
    /// not changing shapes nothing and never takes the font system lock. The
    /// lock is taken once for the whole frame, and only when something is
    /// actually stale.
    fn refresh(
        &mut self,
        geometry: &TerminalGeometry,
        font_size: f32,
        rows: &[Arc<RowSnapshot>],
        column_limit: usize,
    ) {
        let params = ShapeParams {
            font_size: font_size.to_bits(),
            scale_factor: geometry.scale_factor.to_bits(),
            cell_height: geometry.metrics.height.to_bits(),
            family: geometry.metrics.family,
            columns: u16::try_from(column_limit).unwrap_or(u16::MAX),
            fonts: font_system()
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .version(),
        };
        if self.params != Some(params) {
            self.params = Some(params);
            self.rows.clear();
        }
        self.rows.resize_with(rows.len(), ShapedRow::default);

        let mut stale = false;
        for (row, cached) in rows.iter().zip(self.rows.iter_mut()) {
            if cached.source.as_ref().is_some_and(|source| source == row) {
                // Keep the latest allocation so subsequent cursor/pointer
                // redraws take the Arc identity fast path again.
                cached.source = Some(Arc::clone(row));
            } else {
                stale = true;
            }
        }
        if !stale {
            return;
        }

        let mut font_system = font_system()
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let font_system = font_system.raw();
        for (row, cached) in rows.iter().zip(self.rows.iter_mut()) {
            if cached
                .source
                .as_ref()
                .is_some_and(|source| Arc::ptr_eq(source, row))
            {
                continue;
            }
            cached.segments.clear();
            let cells = &row.cells[..row.cells.len().min(column_limit)];
            for segment in segments(cells) {
                let range = usize::from(segment.start)..usize::from(segment.start + segment.len);
                let buffer = shape_segment(
                    font_system,
                    &geometry.metrics,
                    &cells[range],
                    &mut self.content,
                    &mut self.spans,
                    segment.single,
                );
                cached.segments.push(ShapedSegment {
                    column: segment.start,
                    buffer: Arc::new(buffer),
                });
            }
            cached.source = Some(Arc::clone(row));
        }
    }
}

type ErrorMapper<Message> = dyn Fn(TerminalSendError) -> Message + Send + Sync + 'static;
type LinkMapper<Message> = dyn Fn(url::Url, crate::settings::LinkOpenGesture) -> Message + 'static;

/// Resolve a printed link without confusing byte offsets with terminal columns.
/// Only web addresses and local file URLs are activatable; arbitrary schemes
/// from terminal output must not invoke external handlers.
fn printed_link_at(row: &RowSnapshot, column: u16) -> Option<url::Url> {
    if let Some(link) = row
        .hyperlinks
        .iter()
        .find(|link| link.start <= column && column < link.end)
    {
        let url = url::Url::parse(&link.uri).ok()?;
        return matches!(url.scheme(), "http" | "https" | "file").then_some(url);
    }
    let column = usize::from(column);
    let cell = row.cells.get(column)?;
    let boundary = |cell: &CellSnapshot| {
        cell.style.invisible
            || cell.text.is_empty()
            || cell
                .text
                .as_str()
                .chars()
                .any(|ch| ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`'))
    };
    if boundary(cell) {
        return None;
    }
    let start = row.cells[..column]
        .iter()
        .rposition(boundary)
        .map_or(0, |i| i + 1);
    let end = row.cells[column..]
        .iter()
        .position(boundary)
        .map_or(row.cells.len(), |i| column + i);
    let text: String = row.cells[start..end]
        .iter()
        .map(|cell| cell.text.as_str())
        .collect();
    let text = text
        .trim_start_matches(['(', '[', '{'])
        .trim_end_matches(['.', ',', ';', '!', '?']);
    // A balanced parenthesis is legal in a URL; trim only surrounding prose.
    let mut text = text;
    for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
        while text.ends_with(close) && text.matches(close).count() > text.matches(open).count() {
            text = &text[..text.len() - 1];
        }
    }
    let url = url::Url::parse(text).ok()?;
    match url.scheme() {
        "http" | "https" if url.host_str().is_some() => Some(url),
        "file" if url.to_file_path().is_ok() => Some(url),
        _ => None,
    }
}

fn terminal_link_at(
    snapshot: &RenderSnapshot,
    row: usize,
    column: u16,
    cwd: &std::path::Path,
) -> Option<url::Url> {
    let line = snapshot.lines.get(row)?;
    if !line.hyperlinks.is_empty()
        && let Some(url) = printed_link_at(line, column)
    {
        return Some(url);
    }
    let mut first = row;
    while first > 0 && snapshot.lines[first - 1].wrapped {
        first -= 1;
    }
    let mut last = row;
    while last + 1 < snapshot.lines.len() && snapshot.lines[last].wrapped {
        last += 1;
    }
    let combined = if first == last {
        std::borrow::Cow::Borrowed(line.as_ref())
    } else {
        let mut combined = RowSnapshot::default();
        for line in &snapshot.lines[first..=last] {
            combined.cells.extend_from_slice(&line.cells);
        }
        std::borrow::Cow::Owned(combined)
    };
    let offset = snapshot.lines[first..row]
        .iter()
        .map(|row| row.cells.len())
        .sum::<usize>()
        + usize::from(column);
    let offset = u16::try_from(offset).ok()?;
    if let Some(url) = printed_link_at(&combined, offset) {
        return Some(url);
    }
    // Compiler locations are word tokens, optionally surrounded by prose.
    let boundary = |cell: &CellSnapshot| {
        cell.text
            .as_str()
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`'))
    };
    let index = usize::from(offset);
    if boundary(combined.cells.get(index)?) {
        return None;
    }
    let start = combined.cells[..index]
        .iter()
        .rposition(boundary)
        .map_or(0, |index| index + 1);
    let end = combined.cells[index..]
        .iter()
        .position(boundary)
        .map_or(combined.cells.len(), |end| index + end);
    let text: String = combined.cells[start..end]
        .iter()
        .map(|cell| cell.text.as_str())
        .collect();
    let text = text.trim_matches(['(', ')', '[', ']', '{', '}', ',', ';']);
    let (path, final_number) = text.rsplit_once(':')?;
    let final_number = final_number
        .parse::<usize>()
        .ok()
        .filter(|number| *number > 0)?;
    let (path, line, column) = match path.rsplit_once(':') {
        Some((path, line)) if line.parse::<usize>().is_ok() => {
            (path, line.parse::<usize>().ok()?, final_number)
        }
        _ => (path, final_number, 1),
    };
    if path.is_empty() || path.contains("://") || line == 0 {
        return None;
    }
    let path = cwd.join(path);
    let mut url = url::Url::from_file_path(path).ok()?;
    url.set_fragment(Some(&format!("L{line}:{column}")));
    Some(url)
}

/// A lock-free iced view over a single immutable terminal snapshot.
///
/// Construct this in the application's `view` function. It loads ArcSwap once,
/// holds only owned data while drawing, and sends bounded commands back to the
/// worker without ever borrowing or locking Ghostty state.
pub struct TerminalWidget<Message> {
    id: Option<Id>,
    read_only: bool,
    handle: TerminalSurface,
    snapshot: Arc<RenderSnapshot>,
    font_size: f32,
    font_family: &'static str,
    line_height: f32,
    padding: f32,
    scroll_size: isize,
    scroll_speed: f32,
    cursor_style: TerminalCursorStyle,
    scale_factor: f32,
    model_focused: bool,
    on_focus: Option<Message>,
    on_shortcut: Option<Box<dyn Fn(keyboard::Event) -> Option<Message>>>,
    on_context: Option<Box<dyn Fn(Point, Option<super::SearchMatch>, Option<url::Url>) -> Message>>,
    on_copy: Option<Box<dyn Fn(super::SearchMatch) -> Message>>,
    on_paste: Option<Box<dyn Fn(String) -> Message>>,
    on_command_error: Option<Arc<ErrorMapper<Message>>>,
    on_open_link: Option<Box<LinkMapper<Message>>>,
    working_directory: std::path::PathBuf,
    find_matches: Option<(Arc<[super::SearchMatch]>, usize)>,
    select_all_request: u64,
    selection_reset: u64,
    _message: PhantomData<fn() -> Message>,
}

impl<Message> std::fmt::Debug for TerminalWidget<Message> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalWidget")
            .field("generation", &self.snapshot.generation)
            .field("font_size", &self.font_size)
            .field("font_family", &self.font_family)
            .finish_non_exhaustive()
    }
}

impl<Message> TerminalWidget<Message> {
    pub fn new(handle: impl Into<TerminalSurface>) -> Self {
        let handle = handle.into();
        let snapshot = handle.snapshot();
        Self {
            id: None,
            read_only: false,
            handle,
            snapshot,
            font_size: DEFAULT_FONT_SIZE,
            font_family: "",
            line_height: LINE_HEIGHT_RATIO,
            padding: DEFAULT_PADDING,
            scroll_size: DEFAULT_ROWS_PER_WHEEL_NOTCH,
            scroll_speed: 1.0,
            cursor_style: TerminalCursorStyle::default(),
            scale_factor: 1.0,
            model_focused: false,
            on_focus: None,
            on_shortcut: None,
            on_context: None,
            on_copy: None,
            on_paste: None,
            on_command_error: None,
            on_open_link: None,
            working_directory: std::path::PathBuf::from("/"),
            find_matches: None,
            select_all_request: 0,
            selection_reset: 0,
            _message: PhantomData,
        }
    }

    /// Names the input surface for focus handoffs from tabs and other panes.
    pub fn find_matches(mut self, matches: Option<(Arc<[super::SearchMatch]>, usize)>) -> Self {
        self.find_matches = matches;
        self
    }
    pub fn selection_reset(mut self, generation: u64) -> Self {
        self.selection_reset = generation;
        self
    }
    pub fn select_all_request(mut self, request: u64) -> Self {
        self.select_all_request = request;
        self
    }
    pub fn on_context(
        mut self,
        callback: impl Fn(Point, Option<super::SearchMatch>, Option<url::Url>) -> Message + 'static,
    ) -> Self {
        self.on_context = Some(Box::new(callback));
        self
    }
    pub fn on_paste(mut self, callback: impl Fn(String) -> Message + 'static) -> Self {
        self.on_paste = Some(Box::new(callback));
        self
    }
    pub fn on_copy(mut self, callback: impl Fn(super::SearchMatch) -> Message + 'static) -> Self {
        self.on_copy = Some(Box::new(callback));
        self
    }

    /// Names the input surface for focus handoffs from tabs and other panes.
    pub fn id(mut self, id: Id) -> Self {
        self.id = Some(id);
        self
    }

    /// Retain the final grid and selection after a successful process exit.
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Sets the terminal font size.
    ///
    /// The cell box is measured from the renderer's real advance for the
    /// configured family at this size, so the grid, the PTY's reported pixel
    /// size, and the drawn glyphs cannot disagree.
    pub fn font_size(mut self, font_size: f32) -> Self {
        self.font_size = if font_size.is_finite() {
            font_size.clamp(1.0, 400.0)
        } else {
            DEFAULT_FONT_SIZE
        };
        self
    }

    pub fn working_directory(mut self, path: std::path::PathBuf) -> Self {
        self.working_directory = path;
        self
    }

    /// Sets the font family the grid is measured and drawn with. An empty or
    /// whitespace-only name selects the system monospace default.
    ///
    /// A family whose glyph coverage the shell prompt needs - powerline
    /// separators, for one - has to be named here: the generic monospace family
    /// resolves to whatever the system prefers, which need not cover them.
    pub fn font_family(mut self, family: &str) -> Self {
        self.font_family = intern_family(family);
        self
    }

    /// Sets the cell line box as a multiple of the font size.
    pub fn line_height(mut self, line_height: f32) -> Self {
        self.line_height = if line_height.is_finite() {
            line_height.clamp(1.0, 2.0)
        } else {
            LINE_HEIGHT_RATIO
        };
        self
    }

    /// Sets the terminal inset in logical pixels, shared by drawing and input.
    pub fn padding(mut self, padding: f32) -> Self {
        self.padding = if padding.is_finite() {
            padding.clamp(0.0, 24.0)
        } else {
            DEFAULT_PADDING
        };
        self
    }
    /// Sets the number of rows moved by one mouse-wheel notch.
    ///
    /// The speed multiplier also scales high-resolution wheel/trackpad input.
    pub fn scroll_size(mut self, scroll_size: usize) -> Self {
        self.scroll_size = scroll_size
            .try_into()
            .unwrap_or(DEFAULT_ROWS_PER_WHEEL_NOTCH)
            .max(1);
        self
    }

    /// Sets the multiplier applied to wheel input.
    pub fn scroll_speed(mut self, scroll_speed: f32) -> Self {
        self.scroll_speed = if scroll_speed.is_finite() {
            scroll_speed.clamp(0.25, 4.0)
        } else {
            1.0
        };
        self
    }

    /// Overrides the application's cursor shape without changing visibility.
    pub fn cursor_style(mut self, cursor_style: TerminalCursorStyle) -> Self {
        self.cursor_style = cursor_style;
        self
    }

    /// Sets the window's physical pixels per logical pixel. Grid sizing remains
    /// logical while PTY pixel metadata and mouse coordinates use physical
    /// pixels, including fractional-scale displays.
    pub fn scale_factor(mut self, scale_factor: f32) -> Self {
        self.scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        self
    }

    pub fn model_focused(mut self, focused: bool) -> Self {
        self.model_focused = focused;
        self
    }

    pub fn on_focus(mut self, message: Message) -> Self {
        self.on_focus = Some(message);
        self
    }

    pub fn on_shortcut(
        mut self,
        callback: impl Fn(keyboard::Event) -> Option<Message> + 'static,
    ) -> Self {
        self.on_shortcut = Some(Box::new(callback));
        self
    }

    /// Ctrl-click opens a printed web address, including in mouse-reporting TUIs.
    pub fn on_open_link(
        mut self,
        mapper: impl Fn(url::Url, crate::settings::LinkOpenGesture) -> Message + 'static,
    ) -> Self {
        self.on_open_link = Some(Box::new(mapper));
        self
    }

    pub fn on_command_error(
        mut self,
        mapper: impl Fn(TerminalSendError) -> Message + Send + Sync + 'static,
    ) -> Self {
        self.on_command_error = Some(Arc::new(mapper));
        self
    }

    fn publish_error(&self, error: TerminalSendError, shell: &mut Shell<'_, Message>) {
        if let Some(mapper) = &self.on_command_error {
            shell.publish(mapper(error));
        }
    }

    /// Defers a viewport move to the frame it asks for, folding it into
    /// whatever that frame already owes.
    ///
    /// A gesture on a 120 Hz trackpad or pointer produces several moves per
    /// displayed frame, and only the last of them is ever seen. Applying the
    /// rest would each cost a command, a viewport re-read, a snapshot
    /// publication, a serialized grid on a hosted terminal and a redraw that
    /// is overwritten before it reaches the screen.
    ///
    /// One case is observably different rather than merely cheaper: a gesture
    /// that reverses direction against a bound inside a single frame now keeps
    /// its net movement instead of sticking to the bound that the discarded
    /// intermediate positions would have clamped against. Since none of those
    /// positions was ever presented, the net result is the one the user asked
    /// for.
    fn defer_scroll(
        &self,
        state: &mut WidgetState,
        next: ViewportScroll,
        shell: &mut Shell<'_, Message>,
    ) {
        if let Some(superseded) = state.owe_scroll(next) {
            self.apply_scroll(superseded, shell);
        }
        // The flush rides the frame requested here, so nothing owed is
        // stranded by deferring it.
        shell.request_redraw();
    }

    fn apply_scroll(&self, scroll: ViewportScroll, shell: &mut Shell<'_, Message>) {
        if let Err(error) = scroll.apply(&self.handle) {
            self.publish_error(error, shell);
        }
    }

    fn synchronize_focus(&self, state: &mut WidgetState, shell: &mut Shell<'_, Message>) {
        if self.read_only {
            state.focused = self.model_focused;
            state.preedit = None;
            return;
        }
        if self.model_focused {
            state.focus_requested = false;
        }
        if state.focused == self.model_focused {
            return;
        }
        match self.handle.focus(self.model_focused) {
            Ok(()) => {
                state.focused = self.model_focused;
                if !state.focused {
                    state.pressed_buttons = 0;
                    state.scrollbar_drag = None;
                    // A composition belongs to whoever holds the keyboard, and
                    // the input method reopens on the next focus: keeping the
                    // old preedit would republish text the user abandoned.
                    state.preedit = None;
                    // A highlight in a card that no longer holds the keyboard
                    // would advertise a copy whose chord now lands elsewhere.
                    state.selection = None;
                    state.selecting = false;
                    state.selection_click = None;
                }
            }
            Err(error) => self.publish_error(error, shell),
        }
    }

    fn synchronize_size(
        &self,
        state: &mut WidgetState,
        geometry: TerminalGeometry,
        shell: &mut Shell<'_, Message>,
    ) {
        match state
            .pty_size
            .observe(geometry.terminal_size(), Instant::now())
        {
            SizeSync::Current => {}
            // No event may arrive once layout stops moving, so ask for the
            // frame that finishes the wait.
            SizeSync::Settle(due) => shell.request_redraw_at(due),
            SizeSync::Apply(size) => {
                // Reflow moves every cell the selection named, so keeping it
                // would leave it covering text the user never dragged over.
                // Before the first resize there is nothing to clear.
                if state.pty_size.applied.is_some() {
                    state.selection = None;
                    state.selecting = false;
                    state.selection_click = None;
                }
                match self.handle.resize(size) {
                    Ok(()) => state.pty_size.applied(size),
                    Err(error) => {
                        // The size stays pending, so the next frame sends it
                        // again without another wait. Once layout is still,
                        // nothing else may ask for that frame.
                        if matches!(error, TerminalSendError::Full) {
                            shell.request_redraw();
                        }
                        self.publish_error(error, shell);
                    }
                }
            }
        }
    }

    fn send_mouse(
        &self,
        state: &WidgetState,
        geometry: TerminalGeometry,
        position: Point,
        action: MouseAction,
        button: Option<MouseButton>,
        shell: &mut Shell<'_, Message>,
    ) {
        if self.read_only {
            return;
        }
        let input = MouseInput {
            action,
            button,
            modifiers: state.modifiers,
            x: position.x * geometry.scale_factor,
            y: position.y * geometry.scale_factor,
            screen_width: geometry.screen_width_px(),
            screen_height: geometry.screen_height_px(),
            cell_width: geometry.cell_width_px,
            cell_height: geometry.cell_height_px,
            any_button_pressed: state.pressed_buttons != 0,
        };
        if let Err(error) = self.handle.mouse(input) {
            self.publish_error(error, shell);
        }
    }

    /// The cell under a pointer position, within both the laid-out grid and
    /// the published one.
    fn cell_at(&self, geometry: TerminalGeometry, position: Point) -> (u16, u16) {
        geometry.cell_within(position, self.snapshot.columns, self.snapshot.rows)
    }

    /// The grid this frame's selection resolves against.
    fn selection_grid(&self) -> SelectionGrid<'_> {
        SelectionGrid {
            rows: &self.snapshot.lines,
            offset: self.snapshot.scroll.offset,
            columns: self.snapshot.columns,
        }
    }

    /// The absolute scrollback cell a viewport cell names, so that a selection
    /// keeps meaning the same text once the viewport moves under it.
    fn selection_cell(&self, (column, row): (u16, u16)) -> SelectionCell {
        SelectionCell {
            row: self.snapshot.scroll.offset + u64::from(row),
            column,
        }
    }

    /// The selected text, or `None` when nothing is selected and when a
    /// selection covers only blanks - neither is worth putting on a clipboard
    /// the user can no longer get their previous contents back from.
    fn selected_text(&self, state: &WidgetState) -> Option<String> {
        let selection = state.selection?;
        let grid = self.selection_grid();
        let text = selection_text(grid, selection.span(grid));
        (!text.trim().is_empty()).then_some(text)
    }
    fn selected_range(&self, state: &WidgetState) -> Option<super::SearchMatch> {
        let (start, end) = state.selection?.span(self.selection_grid());
        Some(super::SearchMatch {
            start_row: start.row,
            start_column: start.column,
            end_row: end.row,
            end_column: end.column.saturating_add(1),
        })
    }

    /// Sends pasted text to the child. The terminal's owner encodes it, since
    /// only the owner knows whether the child is expecting a bracketed paste
    /// at the moment the text arrives.
    fn paste(&self, text: &str, shell: &mut Shell<'_, Message>) {
        if self.read_only || text.is_empty() {
            return;
        }
        if let Some(callback) = &self.on_paste {
            shell.publish(callback(text.to_owned()));
            return;
        }
        if let Err(error) = self.handle.paste(text, PasteOrigin::Clipboard) {
            self.publish_error(error, shell);
        }
    }

    /// The composition target published to the shell while this terminal holds
    /// the keyboard: the cursor's own cell.
    ///
    /// The preedit rides along rather than being drawn here. The shell paints
    /// it against this rectangle, which is exactly where the committed text
    /// will land, and the grid has no columns to lend text that the child has
    /// not accepted yet - a composed cluster's cell width is only known once
    /// it is committed and the child echoes it back.
    fn input_method<'state>(
        &self,
        state: &'state WidgetState,
        geometry: TerminalGeometry,
    ) -> InputMethod<&'state str> {
        if self.read_only || !self.model_focused {
            return InputMethod::Disabled;
        }
        let Some(caret) = self
            .snapshot
            .cursor
            .and_then(|caret| geometry.cell_bounds(caret.column, caret.row))
        else {
            return InputMethod::Disabled;
        };
        InputMethod::Enabled {
            cursor: caret,
            purpose: input_method::Purpose::Terminal,
            preedit: state.preedit.as_ref().map(input_method::Preedit::as_ref),
        }
    }
}

impl<Message: Clone> Widget<Message, Theme, Renderer> for TerminalWidget<Message> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<WidgetState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(WidgetState::default())
    }

    fn id(&self) -> Option<Id> {
        self.id.clone()
    }

    fn set_id(&mut self, id: Id) {
        self.id = Some(id);
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        // The workspace owns which terminal receives raw input. Register the
        // active surface so a handoff clears focus from buttons and text fields
        // instead of focusing the tab button that would consume Enter.
        if self.model_focused {
            operation.focusable(
                self.id.as_ref(),
                layout.bounds(),
                tree.state.downcast_mut::<WidgetState>(),
            );
        }
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let geometry = TerminalGeometry::new(
            bounds,
            self.font_size,
            self.scale_factor,
            self.font_family,
            self.line_height,
            self.padding,
        );
        let state = tree.state.downcast_mut::<WidgetState>();
        if self.snapshot.scroll.total < state.history_total
            || (self.read_only && !state.completed_read_only)
            || self.selection_reset != state.selection_reset
        {
            state.selection = None;
            state.selecting = false;
            state.selection_click = None;
        }
        state.selection_reset = self.selection_reset;
        state.history_total = self.snapshot.scroll.total;
        state.completed_read_only = self.read_only;
        self.synchronize_size(state, geometry, shell);
        if state.select_all_request != self.select_all_request {
            state.select_all_request = self.select_all_request;
            state.selection = Some(Selection {
                anchor: SelectionCell { row: 0, column: 0 },
                head: SelectionCell {
                    row: self.snapshot.scroll.total.saturating_sub(1),
                    column: self.snapshot.columns.saturating_sub(1),
                },
                mode: SelectionMode::Line,
            });
        }
        self.synchronize_focus(state, shell);
        // Switching to or from the alternate screen replaces the whole grid,
        // and scrollback coordinates do not carry across that swap.
        if state.alternate_screen != self.snapshot.scroll.alternate_screen {
            state.alternate_screen = self.snapshot.scroll.alternate_screen;
            state.selection = None;
            state.selecting = false;
            state.selection_click = None;
        }
        // Only the focused terminal's rail is draggable. An unfocused card may
        // still paint one (see [`TerminalScrollbar::visible`]), but the click
        // that reaches it focuses the terminal instead of grabbing the thumb.
        let scrollbar =
            TerminalScrollbar::new(bounds, self.snapshot.scroll).filter(|_| self.model_focused);
        if scrollbar.is_none() {
            state.scrollbar_drag = None;
        }
        if let Some(bar) = scrollbar {
            let target = match event {
                Event::Mouse(mouse::Event::ButtonPressed(IcedMouseButton::Left))
                    if cursor.is_over(bar.track) =>
                {
                    let position = cursor.position().expect("cursor over scrollbar");
                    let grab = if bar.thumb.contains(position) {
                        position.y - bar.thumb.y
                    } else {
                        bar.thumb.height / 2.0
                    };
                    state.scrollbar_drag = Some(grab);
                    Some(bar.row_at(position.y, grab))
                }
                Event::Mouse(mouse::Event::CursorMoved { position })
                    if state.scrollbar_drag.is_some() =>
                {
                    Some(bar.row_at(
                        drag_position(cursor, *position).y,
                        state.scrollbar_drag.unwrap(),
                    ))
                }
                Event::Mouse(mouse::Event::ButtonReleased(IcedMouseButton::Left))
                    if state.scrollbar_drag.take().is_some() =>
                {
                    shell.capture_event();
                    return;
                }
                _ => None,
            };
            if let Some(row) = target {
                self.defer_scroll(state, ViewportScroll::Row(row), shell);
                shell.capture_event();
                return;
            }
        }

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(IcedMouseButton::Right))
                if cursor.is_over(bounds)
                    && (!self.snapshot.scroll.mouse_tracking || state.modifiers.shift) =>
            {
                if let Some(callback) = &self.on_context {
                    let cell =
                        self.cell_at(geometry, cursor.position().expect("cursor over terminal"));
                    let link = terminal_link_at(
                        &self.snapshot,
                        usize::from(cell.1),
                        cell.0,
                        &self.working_directory,
                    );
                    let pointer = cursor.position().expect("cursor over terminal");
                    shell.publish(callback(
                        Point::new(pointer.x - bounds.x, pointer.y - bounds.y),
                        self.selected_range(state),
                        link,
                    ));
                    shell.capture_event();
                    return;
                }
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modified_key,
                physical_key,
                modifiers,
                text,
                repeat,
                ..
            }) if self.model_focused => {
                if let Some(message) = self.on_shortcut.as_ref().and_then(|callback| {
                    let Event::Keyboard(event) = event else {
                        return None;
                    };
                    callback(event.clone())
                }) {
                    if !state.shortcut_keys.contains(physical_key) {
                        state.shortcut_keys.push(*physical_key);
                    }
                    shell.publish(message);
                    shell.capture_event();
                    return;
                }
                state.modifiers = map_iced_modifiers(*modifiers);
                // Copy, paste and dropping a selection are this widget's
                // chords, never the child's, and each is captured even when it
                // does nothing so that no half of one reaches the child.
                if let Some(chord) = clipboard_chord(key, *physical_key, *modifiers) {
                    match chord {
                        ClipboardChord::Copy => {
                            if let Some(callback) = &self.on_copy
                                && let Some(range) = self.selected_range(state)
                            {
                                shell.publish(callback(range));
                            } else if let Some(text) = self.selected_text(state) {
                                clipboard.write(clipboard::Kind::Standard, text);
                            }
                        }
                        ClipboardChord::Paste => {
                            if let Some(text) = clipboard.read(clipboard::Kind::Standard) {
                                self.paste(&text, shell);
                            }
                        }
                    }
                    shell.capture_event();
                    return;
                }
                if state.selection.is_some() && is_escape(key) {
                    state.selection = None;
                    state.selecting = false;
                    state.selection_click = None;
                    state.escape_cleared = true;
                    shell.capture_event();
                    shell.request_redraw();
                    return;
                }
                // Shift plus a paging key moves this terminal's viewport, the
                // way every terminal binds scrollback, and the child never sees
                // it. Scrollback exists only on the primary screen, so an
                // alternate-screen application still receives the shifted key
                // and can bind it itself.
                if self.snapshot.scroll.scrollable()
                    && let Some(scroll) = viewport_scroll(key, *modifiers, geometry.rows)
                {
                    self.defer_scroll(state, scroll, shell);
                    shell.capture_event();
                    return;
                }
                if self.read_only {
                    shell.capture_event();
                    return;
                }
                let input = key_press(
                    key,
                    modified_key,
                    *physical_key,
                    *modifiers,
                    text.as_deref(),
                    *repeat,
                );
                match self.handle.key(input) {
                    Ok(()) => shell.capture_event(),
                    Err(error) => self.publish_error(error, shell),
                }
            }
            Event::Keyboard(keyboard::Event::KeyReleased {
                key,
                physical_key,
                modifiers,
                ..
            }) if self.model_focused => {
                state.modifiers = map_iced_modifiers(*modifiers);
                if self.read_only {
                    shell.capture_event();
                    return;
                }
                // The press was consumed here, so its release must be too: a
                // child reading key releases may not see one half of a chord.
                if state.escape_cleared && is_escape(key) {
                    state.escape_cleared = false;
                    shell.capture_event();
                    return;
                }
                if clipboard_chord(key, *physical_key, *modifiers).is_some() {
                    shell.capture_event();
                    return;
                }
                if let Some(index) = state
                    .shortcut_keys
                    .iter()
                    .position(|key| key == physical_key)
                {
                    state.shortcut_keys.swap_remove(index);
                    shell.capture_event();
                    return;
                }
                if self.snapshot.scroll.scrollable()
                    && viewport_scroll(key, *modifiers, geometry.rows).is_some()
                {
                    shell.capture_event();
                    return;
                }
                let input = key_release(key, *physical_key, *modifiers);
                match self.handle.key(input) {
                    Ok(()) => shell.capture_event(),
                    Err(error) => self.publish_error(error, shell),
                }
            }
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = map_iced_modifiers(*modifiers);
            }
            Event::Mouse(mouse::Event::ButtonPressed(button)) if cursor.is_over(bounds) => {
                if !self.model_focused
                    && !state.focus_requested
                    && let Some(message) = self.on_focus.clone()
                {
                    state.focus_requested = true;
                    shell.publish(message);
                }
                if let Some(position) = geometry.cursor_position(cursor) {
                    state.last_cursor = Some(position);
                    let cell = self.cell_at(geometry, position);
                    state.last_motion_cell = Some(cell);
                    if *button == IcedMouseButton::Left
                        && state.modifiers.control
                        && let Some(open) = &self.on_open_link
                        && let Some(url) = terminal_link_at(
                            &self.snapshot,
                            usize::from(cell.1),
                            cell.0,
                            &self.working_directory,
                        )
                    {
                        shell.publish(open(
                            url,
                            if state.modifiers.shift {
                                crate::settings::LinkOpenGesture::ControlShiftClick
                            } else {
                                crate::settings::LinkOpenGesture::ControlClick
                            },
                        ));
                        shell.capture_event();
                        return;
                    }
                    // Selecting and pasting belong to this widget whenever the
                    // child is not reading the mouse itself, and shift takes
                    // them back from a child that is. Without that override a
                    // full-screen application's own output could never be
                    // copied, which is why every terminal binds it. For an
                    // observed agent, defer an ordinary click until release:
                    // its controls still get clicks while drags select text.
                    let routing = selection_routing(
                        self.read_only,
                        self.snapshot.scroll.mouse_tracking,
                        self.handle.agent_activity().is_some(),
                        state.modifiers,
                    );
                    let ours = routing == SelectionRouting::Local;
                    let agent_drag = routing == SelectionRouting::ClickOrSelect;
                    match button {
                        IcedMouseButton::Left if ours || agent_drag => {
                            state.begin_selection(
                                self.selection_grid(),
                                self.selection_cell(cell),
                                position,
                                agent_drag,
                            );
                            shell.request_redraw();
                        }
                        IcedMouseButton::Middle if ours => {
                            // The primary selection is what a middle click
                            // pastes everywhere else on X11 and Wayland.
                            if let Some(text) = clipboard.read(clipboard::Kind::Primary) {
                                self.paste(&text, shell);
                            }
                        }
                        button => {
                            state.pressed_buttons = state.pressed_buttons.saturating_add(1);
                            self.send_mouse(
                                state,
                                geometry,
                                position,
                                MouseAction::Press,
                                Some(map_iced_button(*button)),
                                shell,
                            );
                        }
                    }
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(IcedMouseButton::Left))
                if state.selecting =>
            {
                state.selecting = false;
                // A press that never left its cell is a click, not a
                // selection: it drops the old highlight instead of putting one
                // character on the primary selection.
                if state.selection.is_some_and(|selection| {
                    selection.mode == SelectionMode::Character && selection.anchor == selection.head
                }) {
                    state.selection = None;
                } else if let Some(text) = self.selected_text(state) {
                    // Release is the gesture a middle-click paste consumes, so
                    // it is where the primary selection is written.
                    clipboard.write(clipboard::Kind::Primary, text);
                } else {
                    state.selection = None;
                }
                if let Some(position) = state.selection_click.take() {
                    state.pressed_buttons = state.pressed_buttons.saturating_add(1);
                    self.send_mouse(
                        state,
                        geometry,
                        position,
                        MouseAction::Press,
                        Some(MouseButton::Left),
                        shell,
                    );
                    state.pressed_buttons = state.pressed_buttons.saturating_sub(1);
                    self.send_mouse(
                        state,
                        geometry,
                        position,
                        MouseAction::Release,
                        Some(MouseButton::Left),
                        shell,
                    );
                }
                shell.request_redraw();
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(button)) if state.pressed_buttons != 0 => {
                state.pressed_buttons = state.pressed_buttons.saturating_sub(1);
                if let Some(position) = geometry.cursor_position(cursor).or(state.last_cursor) {
                    state.last_cursor = Some(position);
                    self.send_mouse(
                        state,
                        geometry,
                        position,
                        MouseAction::Release,
                        Some(map_iced_button(*button)),
                        shell,
                    );
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) if state.selecting => {
                // Ancestor containers translate the cursor into this layout's
                // coordinates but leave the raw motion event in window space.
                let position = drag_position(cursor, *position);
                let grid = geometry.grid_bounds;
                // Dragging past an edge keeps the selection growing into
                // history: one line per motion event, which is the pointer's
                // own rate and so needs no timer of its own.
                let step = if position.y < grid.y {
                    -1
                } else if position.y >= grid.y + grid.height {
                    1
                } else {
                    0
                };
                let scrolled = step != 0 && self.snapshot.scroll.scrollable();
                if scrolled {
                    self.defer_scroll(state, ViewportScroll::Lines(step), shell);
                }
                // Clamping happens in `cell_at`, so a pointer dragged outside
                // the grid still extends along its nearest edge.
                let cell = self.cell_at(
                    geometry,
                    Point::new(position.x - grid.x, position.y - grid.y),
                );
                // A scrolled viewport puts a different absolute row under an
                // unchanged cell, so that case may not be deduplicated away.
                if scrolled || state.last_motion_cell != Some(cell) {
                    state.last_motion_cell = Some(cell);
                    state.extend_selection(self.selection_cell(cell));
                    shell.request_redraw();
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if let Some(position) = geometry.cursor_position(cursor) {
                    state.last_cursor = Some(position);
                    // A child that never enabled mouse reporting discards every
                    // motion report, yet each one still costs a request frame,
                    // the daemon's reply, and a slot in the same bounded command
                    // channel the user's keystrokes queue in. Report only what
                    // can be observed - tracking enabled, or a drag in progress
                    // - and then only on a cell crossing.
                    let cell = self.cell_at(geometry, position);
                    if (self.snapshot.scroll.mouse_tracking || state.pressed_buttons != 0)
                        && state.last_motion_cell != Some(cell)
                    {
                        state.last_motion_cell = Some(cell);
                        self.send_mouse(
                            state,
                            geometry,
                            position,
                            MouseAction::Motion,
                            None,
                            shell,
                        );
                    }
                } else {
                    // Leaving and re-entering the grid is a fresh motion even
                    // when the pointer lands back on the same cell.
                    state.last_motion_cell = None;
                }
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if cursor.is_over(bounds) => {
                let travel = match delta {
                    ScrollDelta::Lines { y, .. } => WheelTravel::Notches(*y),
                    ScrollDelta::Pixels { y, .. } => WheelTravel::Rows(*y / geometry.cell_height),
                };
                match state.wheel_effect(
                    travel,
                    self.snapshot.scroll,
                    self.scroll_size,
                    self.scroll_speed,
                ) {
                    WheelEffect::Idle => {}
                    WheelEffect::Viewport(delta) => {
                        self.defer_scroll(state, ViewportScroll::Lines(delta), shell);
                    }
                    // A viewport move folded earlier in this same frame is
                    // still parked, and these arms write to the child now.
                    // Applying the owed move first keeps the child's view of
                    // the gesture in the order the user made it: a flick that
                    // crosses a mode boundary - Shift released part-way, or an
                    // application enabling mouse tracking mid-gesture - would
                    // otherwise deliver its later notches before the scroll
                    // that preceded them.
                    WheelEffect::Report { button, notches } => {
                        if let Some(owed) = state.take_owed_scroll() {
                            self.apply_scroll(owed, shell);
                        }
                        if let Some(position) = geometry.cursor_position(cursor) {
                            for _ in 0..notches {
                                self.send_mouse(
                                    state,
                                    geometry,
                                    position,
                                    MouseAction::Press,
                                    Some(button),
                                    shell,
                                );
                            }
                        }
                    }
                    WheelEffect::Keys { key, presses } => {
                        if let Some(owed) = state.take_owed_scroll() {
                            self.apply_scroll(owed, shell);
                        }
                        for _ in 0..presses {
                            let input = KeyInput {
                                action: KeyAction::Press,
                                key,
                                modifiers: state.modifiers,
                                text: None,
                                unshifted_codepoint: None,
                            };
                            if let Err(error) = self.handle.key(input) {
                                self.publish_error(error, shell);
                                break;
                            }
                        }
                    }
                }
                shell.capture_event();
            }
            Event::Window(window::Event::RedrawRequested(now)) => {
                // Every viewport move folded since the last frame is applied
                // exactly once, here, so that a gesture costs one command, one
                // capture and one snapshot per displayed frame rather than one
                // per input event.
                if let Some(owed) = state.take_owed_scroll() {
                    self.apply_scroll(owed, shell);
                }
                let caret = self
                    .snapshot
                    .cursor
                    .filter(|caret| caret.visible && !self.read_only);
                let anchor = caret.map(|caret| (caret.column, caret.row));
                if self.model_focused && caret.is_some_and(|caret| caret.blinking) {
                    shell.request_redraw_at(state.advance_blink(*now, anchor));
                } else {
                    // Nothing to animate: drop the phase so the next blink
                    // starts lit, and ask for no further frames.
                    state.blink_since = None;
                    state.blink_anchor = anchor;
                    state.cursor_dark = false;
                }
                // The shell only honours an input-method request raised during
                // a redraw, and only the focused terminal returns an enabled
                // one, so exactly one card claims the composition target.
                shell.request_input_method(&self.input_method(state, geometry));
            }
            Event::InputMethod(event) if self.model_focused && !self.read_only => match event {
                input_method::Event::Opened => {
                    state.preedit = Some(input_method::Preedit::new());
                    shell.request_redraw();
                }
                input_method::Event::Closed => {
                    state.preedit = None;
                    shell.request_redraw();
                }
                input_method::Event::Preedit(content, selection) => {
                    state.preedit = Some(input_method::Preedit {
                        content: content.clone(),
                        selection: selection.clone(),
                        text_size: Some(Pixels(self.font_size)),
                    });
                    shell.request_redraw();
                }
                input_method::Event::Commit(text) => {
                    state.preedit = None;
                    // Composition is over, so the child receives exactly the
                    // bytes a keyboard able to type this would have sent. It
                    // deliberately bypasses key encoding: composed text has no
                    // key of its own to encode.
                    if !text.is_empty()
                        && let Err(error) = self.handle.input(text.clone().into_bytes())
                    {
                        self.publish_error(error, shell);
                    }
                    shell.capture_event();
                    shell.request_redraw();
                }
            },
            _ => {}
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        if self.snapshot.detached {
            self.handle.acknowledge_redraw(self.snapshot.generation);
            return;
        }

        let bounds = layout.bounds();
        let geometry = TerminalGeometry::new(
            bounds,
            self.font_size,
            self.scale_factor,
            self.font_family,
            self.line_height,
            self.padding,
        );
        let Some(widget_clip) = bounds.intersection(viewport) else {
            self.handle.acknowledge_redraw(self.snapshot.generation);
            return;
        };
        let draw_started =
            tracing::enabled!(target: "cagents::terminal_pipeline", tracing::Level::TRACE)
                .then(std::time::Instant::now);
        renderer.with_layer(widget_clip, |renderer| {
            renderer.fill_quad(
                renderer::Quad {
                    bounds,
                    border: Border::default(),
                    shadow: Shadow::default(),
                    snap: true,
                },
                color(self.snapshot.background),
            );

            let Some(content_clip) = geometry.content_bounds.intersection(&widget_clip) else {
                return;
            };
            renderer.with_layer(content_clip, |renderer| {
                let state = tree.state.downcast_ref::<WidgetState>();
                let row_limit = usize::from(self.snapshot.rows.min(geometry.rows));
                let column_limit = usize::from(self.snapshot.columns.min(geometry.columns));
                let rows = &self.snapshot.lines[..self.snapshot.lines.len().min(row_limit)];
                for (row_index, row) in rows.iter().enumerate() {
                    let cells = &row.cells[..row.cells.len().min(column_limit)];
                    draw_row(
                        renderer,
                        &geometry,
                        row_index as u16,
                        cells,
                        self.snapshot.background,
                    );
                }

                // The wash goes over the cells' own backgrounds and under the
                // glyphs, so selected text keeps every colour it had and only
                // gains the accent behind it.
                if let Some((matches, selected)) = &self.find_matches {
                    let offset = self.snapshot.scroll.offset;
                    let first = matches.partition_point(|hit| hit.end_row < offset);
                    for (index, hit) in matches
                        .iter()
                        .enumerate()
                        .skip(first)
                        .take_while(|(_, hit)| hit.start_row < offset + rows.len() as u64)
                    {
                        for absolute in hit.start_row.max(offset)
                            ..=hit.end_row.min(offset + rows.len() as u64 - 1)
                        {
                            let left = if absolute == hit.start_row {
                                hit.start_column
                            } else {
                                0
                            };
                            let right = if absolute == hit.end_row {
                                hit.end_column
                            } else {
                                column_limit as u16
                            };
                            if let Some(bounds) = geometry.run_bounds(
                                left,
                                right.saturating_sub(left),
                                (absolute - offset) as u16,
                            ) {
                                renderer.fill_quad(
                                    renderer::Quad {
                                        bounds,
                                        border: Border::default(),
                                        shadow: Shadow::default(),
                                        snap: true,
                                    },
                                    Color::from(*theme.cosmic().accent_color())
                                        .scale_alpha(if index == *selected { 0.5 } else { 0.2 }),
                                );
                            }
                        }
                    }
                }
                if let Some(selection) = state.selection {
                    let span = selection.span(self.selection_grid());
                    let columns = self.snapshot.columns.min(geometry.columns);
                    let wash =
                        Color::from(*theme.cosmic().accent_color()).scale_alpha(SELECTION_ALPHA);
                    for row_index in 0..rows.len() {
                        let row = row_index as u16;
                        let absolute = self.snapshot.scroll.offset + u64::from(row);
                        let Some((first, last)) = self
                            .selection_grid()
                            .selected_columns(span, absolute)
                            .map(|(first, last)| (first, last.min(columns.saturating_sub(1))))
                            .filter(|(first, last)| first <= last)
                        else {
                            continue;
                        };
                        let Some(bounds) = geometry.run_bounds(first, last - first + 1, row) else {
                            continue;
                        };
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds,
                                border: Border::default(),
                                shadow: Shadow::default(),
                                snap: true,
                            },
                            wash,
                        );
                    }
                }

                // Reuse glyph buffers for overlapping content after scrolling;
                // only newly exposed or rewritten rows need shaping.
                let mut shaped = state.shaped.borrow_mut();
                shaped.scroll_to(self.snapshot.scroll.offset);
                shaped.refresh(&geometry, self.font_size, rows, column_limit);
                let default_foreground = color(self.snapshot.foreground);
                for (row_index, cached) in shaped.rows.iter().enumerate() {
                    let Ok(row) = u16::try_from(row_index) else {
                        break;
                    };
                    for segment in &cached.segments {
                        let Some(bounds) = geometry.cell_bounds(segment.column, row) else {
                            continue;
                        };
                        text::Renderer::fill_raw(
                            renderer,
                            graphics_text::Raw {
                                buffer: Arc::downgrade(&segment.buffer),
                                position: geometry.text_origin(bounds),
                                color: default_foreground,
                                clip_bounds: content_clip,
                            },
                        );
                    }
                }
                drop(shaped);

                if state.modifiers.control
                    && let Some(position) = geometry.cursor_position(cursor)
                {
                    let (column, row) = self.cell_at(geometry, position);
                    if terminal_link_at(
                        &self.snapshot,
                        usize::from(row),
                        column,
                        &self.working_directory,
                    )
                    .is_some()
                        && let Some(line) = rows.get(usize::from(row))
                    {
                        let span = line
                            .hyperlinks
                            .iter()
                            .find(|span| span.start <= column && column < span.end)
                            .map(|span| (span.start, span.end))
                            .unwrap_or_else(|| {
                                let boundary = |cell: &CellSnapshot| {
                                    cell.text.as_str().chars().any(char::is_whitespace)
                                };
                                let start = line.cells[..usize::from(column)]
                                    .iter()
                                    .rposition(boundary)
                                    .map_or(0, |index| index + 1);
                                let end = line.cells[usize::from(column)..]
                                    .iter()
                                    .position(boundary)
                                    .map_or(line.cells.len(), |index| usize::from(column) + index);
                                (start as u16, end as u16)
                            });
                        if let Some(bounds) =
                            geometry.run_bounds(span.0, span.1.saturating_sub(span.0), row)
                        {
                            draw_decorations(
                                renderer,
                                bounds,
                                default_foreground,
                                Underline::Single,
                                false,
                                false,
                            );
                        }
                    }
                }
                if let Some((cursor, cell_bounds)) = self
                    .snapshot
                    .cursor
                    // Blinking hides the cursor outright for its dark phase,
                    // and only the focused terminal blinks: a card in the
                    // background keeps a static mark and asks for no frames.
                    .filter(|cursor| {
                        !self.read_only
                            && cursor.visible
                            && !(self.model_focused && cursor.blinking && state.cursor_dark)
                    })
                    .and_then(|cursor| {
                        geometry
                            .cell_bounds(cursor.column, cursor.row)
                            .map(|bounds| (cursor, bounds))
                    })
                {
                    let shape = match self.cursor_style {
                        TerminalCursorStyle::Application => cursor.shape,
                        TerminalCursorStyle::Block => CursorShape::Block,
                        TerminalCursorStyle::Beam => CursorShape::Bar,
                        TerminalCursorStyle::Underline => CursorShape::Underline,
                    };
                    // Two physical pixels, never wider than the cell it
                    // outlines.
                    let stroke = (2.0 / geometry.scale_factor)
                        .min(cell_bounds.width)
                        .min(cell_bounds.height);
                    let cursor_bounds = match shape {
                        CursorShape::Bar => Rectangle {
                            width: stroke,
                            ..cell_bounds
                        },
                        CursorShape::Underline => Rectangle {
                            y: cell_bounds.y + cell_bounds.height - stroke,
                            height: stroke,
                            ..cell_bounds
                        },
                        CursorShape::Block | CursorShape::HollowBlock => cell_bounds,
                    };
                    // One terminal at a time owns the keyboard, and a solid
                    // block in each of several visible cards says nothing about
                    // where typing lands. The unfocused ones hollow their block
                    // out and dim whatever they do draw.
                    let hollow = shape == CursorShape::HollowBlock
                        || (!self.model_focused && shape == CursorShape::Block);
                    let alpha = if self.model_focused {
                        1.0
                    } else {
                        UNFOCUSED_CURSOR_ALPHA
                    };
                    let border = if hollow {
                        Border {
                            color: color(cursor.color).scale_alpha(alpha),
                            width: 1.0 / geometry.scale_factor,
                            radius: 0.0.into(),
                        }
                    } else {
                        Border::default()
                    };
                    let fill = if hollow {
                        Color::TRANSPARENT
                    } else {
                        color(cursor.color).scale_alpha(0.65 * alpha)
                    };
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds: cursor_bounds,
                            border,
                            shadow: Shadow::default(),
                            snap: true,
                        },
                        fill,
                    );
                }
            });
            if let Some(bar) = TerminalScrollbar::new(bounds, self.snapshot.scroll).filter(|_| {
                !self.read_only
                    && TerminalScrollbar::visible(self.snapshot.scroll, self.model_focused)
            }) {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: bar.thumb,
                        border: Border {
                            radius: (SCROLLBAR_THUMB_WIDTH / 2.0).into(),
                            ..Border::default()
                        },
                        shadow: Shadow::default(),
                        snap: true,
                    },
                    // Dimmed when unfocused: the rail is there to say this card
                    // is looking at history, not to invite a drag that only the
                    // focused terminal services.
                    color(self.snapshot.foreground).scale_alpha(if self.model_focused {
                        0.45
                    } else {
                        0.2
                    }),
                );
            }
        });
        if let Some(started) = draw_started {
            // This records CPU draw-command construction, not GPU completion
            // or compositor presentation/scanout.
            tracing::trace!(target: "cagents::terminal_pipeline", stage = "draw",
                generation = self.snapshot.generation,
                activity_sequence = self.snapshot.activity.map_or(0, |fact| fact.sequence),
                elapsed_ns = started.elapsed().as_nanos() as u64);
        }

        self.handle.acknowledge_redraw(self.snapshot.generation);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if self.model_focused
            && TerminalScrollbar::new(layout.bounds(), self.snapshot.scroll)
                .is_some_and(|bar| cursor.is_over(bar.track))
        {
            return mouse::Interaction::Pointer;
        }
        let geometry = TerminalGeometry::new(
            layout.bounds(),
            self.font_size,
            self.scale_factor,
            self.font_family,
            self.line_height,
            self.padding,
        );
        if let Some(position) = geometry.cursor_position(cursor) {
            let cell = self.cell_at(geometry, position);
            if tree.state.downcast_ref::<WidgetState>().modifiers.control
                && self.on_open_link.is_some()
                && terminal_link_at(
                    &self.snapshot,
                    usize::from(cell.1),
                    cell.0,
                    &self.working_directory,
                )
                .is_some()
            {
                return mouse::Interaction::Pointer;
            }
            mouse::Interaction::Text
        } else {
            mouse::Interaction::None
        }
    }
}

impl<'a, Message: Clone + 'a> From<TerminalWidget<Message>> for Element<'a, Message> {
    fn from(widget: TerminalWidget<Message>) -> Self {
        Element::new(widget)
    }
}

fn color(value: Rgb) -> Color {
    Color::from_rgb8(value.red, value.green, value.blue)
}

// Ghostty's default faint-opacity is 0.5, rounded up to an 8-bit alpha.
const FAINT_ALPHA: u8 = 128;

/// Draws one grid row's quads: backgrounds and decorations.
///
/// A cell-at-a-time loop cost one background quad plus up to three decoration
/// quads *per column*: for a 200-column grid that is tens of thousands of
/// primitives every frame. Terminal rows are overwhelmingly uniform, so each
/// pass coalesces every maximal run of cells sharing the attribute it draws.
/// Quads are positioned individually, so a run needs no length cap.
///
/// Glyphs are drawn separately, from shaped buffers cached across frames: see
/// [`ShapeCache::refresh`].
fn draw_row(
    renderer: &mut Renderer,
    geometry: &TerminalGeometry,
    row: u16,
    cells: &[CellSnapshot],
    default_background: Rgb,
) {
    // Backgrounds. Cells matching the terminal default are already covered by
    // the widget-wide fill, so only runs that differ are drawn.
    for run in runs(cells, |cell| (resolved(cell).1, cell.selected)) {
        let (background, selected) = run.key;
        if background == default_background && !selected {
            continue;
        }
        let Some(bounds) = geometry.run_bounds(run.start, run.len, row) else {
            continue;
        };
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                border: Border::default(),
                shadow: Shadow::default(),
                snap: true,
            },
            color(background),
        );
    }

    // Decorations. Keyed on colour plus only the line-drawing bits, so a
    // bold/italic change inside an underlined span does not split the rule.
    for run in runs(cells, |cell| {
        let (foreground, _) = resolved(cell);
        (
            foreground,
            cell.style.underline,
            cell.style.strikethrough,
            cell.style.overline,
            cell.style.faint,
            cell.style.invisible,
        )
    }) {
        let (foreground, underline, strikethrough, overline, faint, invisible) = run.key;
        if invisible || (underline == Underline::None && !strikethrough && !overline) {
            continue;
        }
        let Some(bounds) = geometry.run_bounds(run.start, run.len, row) else {
            continue;
        };
        draw_decorations(
            renderer,
            bounds,
            color(foreground).scale_alpha(if faint {
                f32::from(FAINT_ALPHA) / 255.0
            } else {
                1.0
            }),
            underline,
            strikethrough,
            overline,
        );
    }
}

/// A span of columns drawn by one shaped buffer.
///
/// `single` marks a segment that must be shaped on its own because its advance
/// is not predictable: see [`segments`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Segment {
    start: u16,
    len: u16,
    single: bool,
}

/// Splits a row into the segments that can each be drawn as one shaped buffer.
///
/// `set_monospace_width` rounds every glyph's advance to a whole number of
/// cells, so a run of cells lands on its columns exactly - but only if each
/// cell contributes its own width and nothing else. That holds for a single
/// printable ASCII scalar, which is what `is_narrow_ascii` tests, and cannot be
/// assumed otherwise: a wide cluster consumes two columns and the grid stores
/// its second column as a separate cell whose content this layer cannot
/// distinguish from a blank. Such a cell is therefore given a segment of its
/// own, positioned explicitly, exactly as every cell used to be.
///
/// Invisible and empty cells draw nothing, so they are skipped and end the run
/// they interrupt. A blank is an ordinary narrow-ASCII space and stays inside
/// its run, which is what keeps a line of text one shaped buffer.
fn segments(cells: &[CellSnapshot]) -> impl Iterator<Item = Segment> {
    let mut column = 0usize;
    std::iter::from_fn(move || {
        loop {
            let cell = cells.get(column)?;
            let start = u16::try_from(column).ok()?;
            if cell.style.invisible || cell.text.is_empty() {
                column += 1;
                continue;
            }
            if !cell.text.is_narrow_ascii() {
                column += 1;
                return Some(Segment {
                    start,
                    len: 1,
                    single: true,
                });
            }
            let mut len = 0u16;
            while let Some(cell) = cells.get(column) {
                if cell.style.invisible || !cell.text.is_narrow_ascii() {
                    break;
                }
                column += 1;
                len += 1;
            }
            return Some(Segment {
                start,
                len,
                single: false,
            });
        }
    })
}

/// Shapes one segment into a buffer whose glyphs land on the grid.
///
/// Each span carries its own [`cosmic_text::Metrics`], holding the font size at
/// which that face advances by exactly one cell. cosmic-text multiplies a
/// glyph's em advance by its span's font size, so every cell in the run
/// advances by exactly the pitch and the Nth glyph starts exactly N pitches
/// along. The pitch is a whole number of physical pixels, so once the renderer
/// applies the scale factor every origin is an integer device pixel.
///
/// This is deliberately not `set_monospace_width`, which would do the same job
/// in the shaper: that path reads `Font::monospace_em_width`, which is `None`
/// unless cosmic-text is built with `monospace_fallback` *and* the resolved face
/// sets `post.isFixedPitch`. Noto Sans Mono does not, so it would be silently
/// inert on exactly the systems where the generic monospace family resolves to
/// it. Deriving the size from our own measurement depends on no font metadata.
///
/// Hinting must stay disabled: it rounds advances to whole *logical* pixels,
/// which on a fractional scale factor reintroduces per-cell drift.
fn shape_segment(
    font_system: &mut cosmic_text::FontSystem,
    metrics: &CellMetrics,
    cells: &[CellSnapshot],
    content: &mut String,
    spans: &mut Vec<(Range<usize>, cosmic_text::Attrs<'static>)>,
    advanced: bool,
) -> cosmic_text::Buffer {
    let line_height = metrics.height.max(1.0);
    content.clear();
    spans.clear();
    for cell in cells {
        let start = content.len();
        // `segments` skips invisible cells, so a segment should not contain
        // one. Kept as a guard rather than an assertion: if that ever changes,
        // the cell owes its column an advance and must not render its text.
        if cell.style.invisible {
            content.push(' ');
        } else {
            content.push_str(cell.text.as_str());
        }
        let key = Face {
            bold: cell.style.bold,
            italic: cell.style.italic,
        };
        let foreground = resolved(cell).0;
        let attrs = graphics_text::to_attributes(font(metrics.family, key))
            .metrics(cosmic_text::Metrics::new(
                metrics.face_size(key),
                line_height,
            ))
            .color(cosmic_text::Color::rgba(
                foreground.red,
                foreground.green,
                foreground.blue,
                if cell.style.faint { FAINT_ALPHA } else { 255 },
            ));
        match spans.last_mut() {
            Some((range, last)) if *last == attrs => range.end = content.len(),
            _ => spans.push((start..content.len(), attrs)),
        }
    }

    let mut buffer = cosmic_text::Buffer::new_empty(cosmic_text::Metrics::new(
        metrics.face_size(Face::PLAIN),
        line_height,
    ));
    buffer.set_wrap(cosmic_text::Wrap::None);
    buffer.set_size(None, None);
    buffer.set_hinting(cosmic_text::Hinting::Disabled);
    let default_attrs = graphics_text::to_attributes(font(metrics.family, Face::PLAIN));
    buffer.set_rich_text(
        spans
            .iter()
            .map(|(range, attrs)| (&content[range.clone()], attrs.clone())),
        &default_attrs,
        // Script detection and font fallback are only skippable for plain
        // ASCII; a wide or multi-scalar cluster needs both or renders as tofu.
        if advanced {
            cosmic_text::Shaping::Advanced
        } else {
            cosmic_text::Shaping::Basic
        },
        None,
    );
    buffer.shape_until_scroll(font_system, false);
    buffer
}

/// The foreground/background pair a cell is actually drawn with, after applying
/// the selection swap.
fn resolved(cell: &CellSnapshot) -> (Rgb, Rgb) {
    if cell.selected {
        (cell.background, cell.foreground)
    } else {
        (cell.foreground, cell.background)
    }
}

struct Run<Key> {
    start: u16,
    len: u16,
    key: Key,
}

/// Splits `cells` into maximal spans whose `key` is equal.
fn runs<Key: PartialEq, F: Fn(&CellSnapshot) -> Key>(
    cells: &[CellSnapshot],
    key: F,
) -> impl Iterator<Item = Run<Key>> {
    let mut start = 0u16;
    std::iter::from_fn(move || {
        let first = usize::from(start);
        let current = key(cells.get(first)?);
        let mut len = 1u16;
        while let Some(next) = cells.get(first + usize::from(len))
            && key(next) == current
        {
            len += 1;
        }
        let run = Run {
            start,
            len,
            key: current,
        };
        start += len;
        Some(run)
    })
}

/// Draws the rules for a span of cells that share a colour and decoration set.
fn draw_decorations(
    renderer: &mut Renderer,
    bounds: Rectangle,
    foreground: Color,
    underline: Underline,
    strikethrough: bool,
    overline: bool,
) {
    let line = |renderer: &mut Renderer, y: f32, height: f32| {
        renderer.fill_quad(
            renderer::Quad {
                bounds: Rectangle {
                    x: bounds.x,
                    y,
                    width: bounds.width,
                    height,
                },
                border: Border::default(),
                shadow: Shadow::default(),
                snap: true,
            },
            foreground,
        );
    };
    if underline != Underline::None {
        let thickness = if underline == Underline::Double {
            1.0
        } else {
            1.5
        };
        line(renderer, bounds.y + bounds.height - 2.0, thickness);
        if underline == Underline::Double {
            line(renderer, bounds.y + bounds.height - 4.0, 1.0);
        }
    }
    if strikethrough {
        line(renderer, bounds.y + bounds.height * 0.52, 1.0);
    }
    if overline {
        line(renderer, bounds.y, 1.0);
    }
}

fn map_iced_modifiers(modifiers: keyboard::Modifiers) -> KeyModifiers {
    KeyModifiers {
        shift: modifiers.shift(),
        alt: modifiers.alt(),
        control: modifiers.control(),
        super_key: modifiers.logo(),
        caps_lock: modifiers.contains(keyboard::Modifiers::CAPS_LOCK),
    }
}

/// The press iced reported, as the encoder takes it.
fn key_press(
    key: &keyboard::Key,
    modified_key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
    text: Option<&str>,
    repeat: bool,
) -> KeyInput {
    KeyInput {
        action: if repeat {
            KeyAction::Repeat
        } else {
            KeyAction::Press
        },
        key: map_iced_key(key, physical_key),
        modifiers: map_iced_modifiers(modifiers),
        text: key_text(text, modified_key),
        unshifted_codepoint: character_from_key(key),
    }
}

/// The release iced reported, naming the key its press named.
fn key_release(
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
) -> KeyInput {
    KeyInput {
        action: KeyAction::Release,
        key: map_iced_key(key, physical_key),
        modifiers: map_iced_modifiers(modifiers),
        text: None,
        unshifted_codepoint: character_from_key(key),
    }
}

/// The text a key press hands the encoder: what the key types without Ctrl.
///
/// Winit's text carries Ctrl's effect, so Ctrl+C types U+0003. Ghostty's
/// encoder makes control characters itself, from the key and its modifiers,
/// but it reads a one-byte text before the key, and U+0003 is not a character
/// it knows how to control: it falls back to `CSI 3;5u`, which no line
/// discipline turns into SIGINT. So a lone control character is replaced by
/// the key's own text, as Ghostty's GTK and macOS front ends do. Enter, Tab,
/// Escape and Backspace type control characters even without Ctrl; they hand
/// over no text, and the encoder names them from the key alone.
fn key_text(text: Option<&str>, modified_key: &keyboard::Key) -> Option<String> {
    let text = text?;
    let mut characters = text.chars();
    let lone_control = characters
        .next()
        .is_some_and(|character| u32::from(character) < 0x20)
        && characters.next().is_none();
    if !lone_control {
        return Some(text.to_owned());
    }
    match modified_key {
        keyboard::Key::Character(typed) if !typed.chars().any(char::is_control) => {
            Some(typed.to_string())
        }
        _ => None,
    }
}

fn character_from_key(key: &keyboard::Key) -> Option<char> {
    let keyboard::Key::Character(text) = key else {
        return None;
    };
    let mut characters = text.chars();
    let character = characters.next()?;
    characters.next().is_none().then_some(character)
}

/// The key a press names to the encoder: when the active layout types a
/// non-Latin character, the key a US layout has in the same place.
///
/// Ghostty's encoder turns Ctrl and a key into a control character by the
/// key's US name, so on a Russian layout Ctrl+С is Ctrl+C - SIGINT - and Ctrl+Ъ
/// is Ctrl+] - GS - as they are in Ghostty's own front ends. The press still
/// carries the layout's own text and unshifted character, so a child reading
/// the kitty protocol sees `с`, and `c` as its base-layout key when it asks for
/// alternates.
fn map_iced_key(key: &keyboard::Key, physical_key: keyboard::key::Physical) -> TerminalKey {
    match key.as_ref() {
        keyboard::Key::Character(text) => key
            .to_latin(physical_key)
            .or_else(|| us_punctuation(physical_key))
            .or_else(|| text.chars().next())
            .map_or(TerminalKey::Unidentified, TerminalKey::Character),
        keyboard::Key::Named(named) => match named {
            keyboard::key::Named::Backspace => TerminalKey::Backspace,
            keyboard::key::Named::Enter => TerminalKey::Enter,
            keyboard::key::Named::Tab => TerminalKey::Tab,
            keyboard::key::Named::Escape => TerminalKey::Escape,
            keyboard::key::Named::Delete => TerminalKey::Delete,
            keyboard::key::Named::Insert => TerminalKey::Insert,
            keyboard::key::Named::Home => TerminalKey::Home,
            keyboard::key::Named::End => TerminalKey::End,
            keyboard::key::Named::PageUp => TerminalKey::PageUp,
            keyboard::key::Named::PageDown => TerminalKey::PageDown,
            keyboard::key::Named::ArrowUp => TerminalKey::ArrowUp,
            keyboard::key::Named::ArrowDown => TerminalKey::ArrowDown,
            keyboard::key::Named::ArrowLeft => TerminalKey::ArrowLeft,
            keyboard::key::Named::ArrowRight => TerminalKey::ArrowRight,
            keyboard::key::Named::F1 => TerminalKey::Function(1),
            keyboard::key::Named::F2 => TerminalKey::Function(2),
            keyboard::key::Named::F3 => TerminalKey::Function(3),
            keyboard::key::Named::F4 => TerminalKey::Function(4),
            keyboard::key::Named::F5 => TerminalKey::Function(5),
            keyboard::key::Named::F6 => TerminalKey::Function(6),
            keyboard::key::Named::F7 => TerminalKey::Function(7),
            keyboard::key::Named::F8 => TerminalKey::Function(8),
            keyboard::key::Named::F9 => TerminalKey::Function(9),
            keyboard::key::Named::F10 => TerminalKey::Function(10),
            keyboard::key::Named::F11 => TerminalKey::Function(11),
            keyboard::key::Named::F12 => TerminalKey::Function(12),
            keyboard::key::Named::F13 => TerminalKey::Function(13),
            keyboard::key::Named::F14 => TerminalKey::Function(14),
            keyboard::key::Named::F15 => TerminalKey::Function(15),
            keyboard::key::Named::F16 => TerminalKey::Function(16),
            keyboard::key::Named::F17 => TerminalKey::Function(17),
            keyboard::key::Named::F18 => TerminalKey::Function(18),
            keyboard::key::Named::F19 => TerminalKey::Function(19),
            keyboard::key::Named::F20 => TerminalKey::Function(20),
            keyboard::key::Named::F21 => TerminalKey::Function(21),
            keyboard::key::Named::F22 => TerminalKey::Function(22),
            keyboard::key::Named::F23 => TerminalKey::Function(23),
            keyboard::key::Named::F24 => TerminalKey::Function(24),
            keyboard::key::Named::F25 => TerminalKey::Function(25),
            _ => TerminalKey::Unidentified,
        },
        keyboard::Key::Unidentified => TerminalKey::Unidentified,
    }
}

/// What a US layout's punctuation key types unshifted, the part of the
/// writing-system keys iced's `Key::to_latin` leaves out.
fn us_punctuation(physical_key: keyboard::key::Physical) -> Option<char> {
    use keyboard::key::Code;

    let keyboard::key::Physical::Code(code) = physical_key else {
        return None;
    };
    Some(match code {
        Code::Backquote => '`',
        Code::Minus => '-',
        Code::Equal => '=',
        Code::BracketLeft => '[',
        Code::BracketRight => ']',
        Code::Backslash => '\\',
        Code::Semicolon => ';',
        Code::Quote => '\'',
        Code::Comma => ',',
        Code::Period => '.',
        Code::Slash => '/',
        _ => return None,
    })
}

fn map_iced_button(button: IcedMouseButton) -> MouseButton {
    match button {
        IcedMouseButton::Left => MouseButton::Left,
        IcedMouseButton::Right => MouseButton::Right,
        IcedMouseButton::Middle => MouseButton::Middle,
        IcedMouseButton::Back => MouseButton::Back,
        IcedMouseButton::Forward => MouseButton::Forward,
        IcedMouseButton::Other(number) => MouseButton::Other(number),
    }
}

/// A viewport move applied to this terminal instead of being encoded and sent
/// to the child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewportScroll {
    Lines(isize),
    Row(u64),
    Top,
    Bottom,
}

impl ViewportScroll {
    fn apply(self, handle: &TerminalSurface) -> Result<(), TerminalSendError> {
        match self {
            Self::Lines(delta) => handle.scroll_lines(delta),
            Self::Row(row) => handle.scroll_to_row(row),
            Self::Top => handle.scroll_to_top(),
            Self::Bottom => handle.scroll_to_bottom(),
        }
    }
}

/// One wheel event's vertical travel, in the unit the device that produced it
/// actually measures.
///
/// A wheel and a trackpad are not the same gesture wearing different numbers:
/// one counts detents, the other measures a finger. Keeping them apart here is
/// what lets the viewport give a detent a useful size without also stretching
/// a trackpad's real distance.
#[derive(Clone, Copy, Debug, PartialEq)]
enum WheelTravel {
    /// Notches of a discrete wheel, each scaled by the configured scroll speed.
    Notches(f32),
    /// A trackpad's distance, already divided down to rows of the grid.
    Rows(f32),
}

/// What one wheel event does to a terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WheelEffect {
    /// The gesture has not accumulated a whole line yet.
    Idle,
    /// A viewport move, safe to fold into the rest of the frame.
    Viewport(isize),
    /// One mouse report per notch, in gesture order, for a child that asked
    /// to read the wheel itself.
    Report { button: MouseButton, notches: u32 },
    /// Arrow-key presses, in gesture order, for an alternate-screen
    /// application that has no scrollback to move. One press per row the
    /// gesture asked for, so the cursor covers the same distance the
    /// scrollback viewport would have.
    Keys { key: TerminalKey, presses: u32 },
}

/// The viewport move a key press performs instead of reaching the child, if
/// any.
///
/// A page step keeps one row of overlap so the eye can stitch the two screens
/// together, the way a pager's own paging does. Any modifier beyond shift
/// belongs to the child: those are chords applications bind themselves.
fn viewport_scroll(
    key: &keyboard::Key,
    modifiers: keyboard::Modifiers,
    rows: u16,
) -> Option<ViewportScroll> {
    if !modifiers.shift() || modifiers.control() || modifiers.alt() || modifiers.logo() {
        return None;
    }
    let page = rows.saturating_sub(1).max(1) as isize;
    match key.as_ref() {
        keyboard::Key::Named(keyboard::key::Named::PageUp) => Some(ViewportScroll::Lines(-page)),
        keyboard::Key::Named(keyboard::key::Named::PageDown) => Some(ViewportScroll::Lines(page)),
        keyboard::Key::Named(keyboard::key::Named::Home) => Some(ViewportScroll::Top),
        keyboard::Key::Named(keyboard::key::Named::End) => Some(ViewportScroll::Bottom),
        _ => None,
    }
}

/// The clipboard action a key press performs here instead of reaching the
/// child, if any.
///
/// The shift is what makes these the terminal's own: the unshifted chords are
/// the child's, where Ctrl+C interrupts the foreground job and Ctrl+V quotes
/// the next character. Any further modifier belongs to the child for the same
/// reason [`viewport_scroll`] hands one back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipboardChord {
    Copy,
    Paste,
}

/// Ctrl+Shift+C and Ctrl+Shift+V, by the Latin key in their place so the
/// chords survive a switch to a non-Latin layout.
fn clipboard_chord(
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
) -> Option<ClipboardChord> {
    if key.as_ref() == keyboard::Key::Named(keyboard::key::Named::Insert)
        && modifiers.shift()
        && !modifiers.control()
        && !modifiers.alt()
        && !modifiers.logo()
    {
        return Some(ClipboardChord::Paste);
    }
    if !modifiers.control() || !modifiers.shift() || modifiers.alt() || modifiers.logo() {
        return None;
    }
    // iced reports the key without modifiers, but a capital costs nothing to
    // fold.
    match key.to_latin(physical_key)?.to_ascii_lowercase() {
        'c' => Some(ClipboardChord::Copy),
        'v' => Some(ClipboardChord::Paste),
        _ => None,
    }
}

fn is_escape(key: &keyboard::Key) -> bool {
    matches!(
        key.as_ref(),
        keyboard::Key::Named(keyboard::key::Named::Escape)
    )
}

#[cfg(test)]
mod tests {
    use crate::terminal::CellText;

    use super::*;

    #[test]
    fn scrollbar_requires_history_and_drag_clamps_to_real_offsets() {
        let bounds = Rectangle {
            x: 20.0,
            y: 30.0,
            width: 400.0,
            height: 300.0,
        };
        let scroll = super::super::ScrollSnapshot {
            total: 100,
            offset: 40,
            len: 20,
            alternate_screen: false,
            mouse_tracking: false,
            bracketed_paste: false,
        };
        assert!(
            TerminalScrollbar::new(
                bounds,
                super::super::ScrollSnapshot {
                    alternate_screen: true,
                    ..scroll
                },
            )
            .is_none()
        );
        assert!(
            TerminalScrollbar::new(
                bounds,
                super::super::ScrollSnapshot {
                    total: 20,
                    ..scroll
                },
            )
            .is_none()
        );
        // An unfocused terminal paints its rail only while scrolled back, so a
        // card left in history says so and a live one stays clean.
        let live = super::super::ScrollSnapshot {
            offset: scroll.max_offset(),
            ..scroll
        };
        assert!(TerminalScrollbar::visible(scroll, false));
        assert!(!TerminalScrollbar::visible(live, false));
        assert!(TerminalScrollbar::visible(live, true));
        let bar = TerminalScrollbar::new(bounds, scroll).unwrap();
        assert_eq!(bar.thumb.height, 60.0);
        assert_eq!(bar.row_at(bar.thumb.y + 10.0, 10.0), 40);
        assert_eq!(bar.row_at(-500.0, 10.0), 0);
        assert_eq!(bar.row_at(900.0, 10.0), 80);
        assert_eq!(bar.track.x + bar.track.width, bounds.x + bounds.width);
    }

    #[test]
    fn shifted_paging_keys_move_the_viewport_and_nothing_else_does() {
        let named = |key| keyboard::Key::Named(key);
        let shift = keyboard::Modifiers::SHIFT;
        assert_eq!(
            viewport_scroll(&named(keyboard::key::Named::PageUp), shift, 24),
            Some(ViewportScroll::Lines(-23))
        );
        assert_eq!(
            viewport_scroll(&named(keyboard::key::Named::PageDown), shift, 24),
            Some(ViewportScroll::Lines(23))
        );
        assert_eq!(
            viewport_scroll(&named(keyboard::key::Named::Home), shift, 24),
            Some(ViewportScroll::Top)
        );
        assert_eq!(
            viewport_scroll(&named(keyboard::key::Named::End), shift, 24),
            Some(ViewportScroll::Bottom)
        );
        // A one-row grid still steps by a row rather than standing still.
        assert_eq!(
            viewport_scroll(&named(keyboard::key::Named::PageUp), shift, 1),
            Some(ViewportScroll::Lines(-1))
        );
        // Unshifted and over-modified paging keys belong to the child.
        assert_eq!(
            viewport_scroll(
                &named(keyboard::key::Named::PageUp),
                keyboard::Modifiers::empty(),
                24
            ),
            None
        );
        assert_eq!(
            viewport_scroll(
                &named(keyboard::key::Named::PageUp),
                shift | keyboard::Modifiers::CTRL,
                24
            ),
            None
        );
        assert_eq!(
            viewport_scroll(&keyboard::Key::Character("a".into()), shift, 24),
            None
        );
    }

    #[test]
    fn a_key_hands_over_what_it_types_without_ctrl() {
        let key = |text: &str| keyboard::Key::Character(text.into());
        let named = |named| keyboard::Key::Named(named);
        let physical_c = keyboard::key::Physical::Code(keyboard::key::Code::KeyC);

        // Ctrl+C, Ctrl+C under Caps Lock, and Ctrl+[ as winit reports them.
        assert_eq!(key_text(Some("\u{3}"), &key("c")).as_deref(), Some("c"));
        assert_eq!(key_text(Some("\u{3}"), &key("C")).as_deref(), Some("C"));
        assert_eq!(key_text(Some("\u{1b}"), &key("[")).as_deref(), Some("["));
        for (text, key) in [
            ("\r", keyboard::key::Named::Enter),
            ("\t", keyboard::key::Named::Tab),
            ("\u{1b}", keyboard::key::Named::Escape),
            ("\u{8}", keyboard::key::Named::Backspace),
        ] {
            assert_eq!(key_text(Some(text), &named(key)), None, "{key:?}");
        }
        // Text without a control character is the key's own.
        assert_eq!(key_text(Some("a"), &key("a")).as_deref(), Some("a"));
        assert_eq!(key_text(Some("é"), &key("é")).as_deref(), Some("é"));
        assert_eq!(key_text(None, &key("c")), None);

        // The press a terminal sends for Ctrl+C names the key and its text,
        // never the ETX winit typed for it.
        let press = key_press(
            &key("c"),
            &key("c"),
            physical_c,
            keyboard::Modifiers::CTRL,
            Some("\u{3}"),
            false,
        );
        assert_eq!(press.key, TerminalKey::Character('c'));
        assert!(press.modifiers.control);
        assert_eq!(press.text.as_deref(), Some("c"));
        assert_eq!(press.unshifted_codepoint, Some('c'));
    }

    #[test]
    fn a_non_latin_layout_names_the_latin_key_in_its_place() {
        let key = |text: &str| keyboard::Key::Character(text.into());
        let physical = keyboard::key::Physical::Code;

        // Ctrl+С on a Russian layout names C, which the encoder makes ETX of,
        // and keeps the layout's text and character for the kitty protocol.
        let press = key_press(
            &key("с"),
            &key("с"),
            physical(keyboard::key::Code::KeyC),
            keyboard::Modifiers::CTRL,
            Some("с"),
            false,
        );
        assert_eq!(press.key, TerminalKey::Character('c'));
        assert_eq!(press.text.as_deref(), Some("с"));
        assert_eq!(press.unshifted_codepoint, Some('с'));
        // Its release names the same key, or a child reading releases would
        // see one key go down and another come up.
        let release = key_release(
            &key("с"),
            physical(keyboard::key::Code::KeyC),
            keyboard::Modifiers::CTRL,
        );
        assert_eq!(release.action, KeyAction::Release);
        assert_eq!(release.key, press.key);
        assert_eq!(release.unshifted_codepoint, Some('с'));
        assert_eq!(release.text, None);
        // Punctuation positions too: Ctrl+Ъ is Ctrl+] and Ctrl+Ж is Ctrl+;.
        assert_eq!(
            map_iced_key(&key("ъ"), physical(keyboard::key::Code::BracketRight)),
            TerminalKey::Character(']')
        );
        assert_eq!(
            map_iced_key(&key("ж"), physical(keyboard::key::Code::Semicolon)),
            TerminalKey::Character(';')
        );
        // A Latin layout's key is its own, wherever it sits.
        assert_eq!(
            map_iced_key(&key("q"), physical(keyboard::key::Code::KeyA)),
            TerminalKey::Character('q')
        );
        assert_eq!(
            map_iced_key(&key("ü"), physical(keyboard::key::Code::BracketLeft)),
            TerminalKey::Character('ü')
        );
        // A non-Latin key with no US key in its place stays itself.
        assert_eq!(
            map_iced_key(&key("ё"), physical(keyboard::key::Code::IntlBackslash)),
            TerminalKey::Character('ё')
        );

        // Copy and paste follow the same key.
        let chord = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
        assert_eq!(
            clipboard_chord(&key("С"), physical(keyboard::key::Code::KeyC), chord),
            Some(ClipboardChord::Copy)
        );
        assert_eq!(
            clipboard_chord(&key("М"), physical(keyboard::key::Code::KeyV), chord),
            Some(ClipboardChord::Paste)
        );
        assert_eq!(
            clipboard_chord(&key("C"), physical(keyboard::key::Code::KeyC), chord),
            Some(ClipboardChord::Copy)
        );
        assert_eq!(
            clipboard_chord(
                &key("с"),
                physical(keyboard::key::Code::KeyC),
                keyboard::Modifiers::CTRL
            ),
            None
        );
    }

    #[test]
    fn cursor_blink_alternates_per_phase_and_restarts_after_a_stall() {
        let mut state = WidgetState::default();
        let start = Instant::now();
        let anchor = Some((3, 4));
        // The first frame lights the cursor and asks for exactly one redraw,
        // one phase out.
        assert_eq!(state.advance_blink(start, anchor), start + BLINK_PHASE);
        assert!(!state.cursor_dark);
        // Crossing a boundary flips the phase once.
        assert_eq!(
            state.advance_blink(start + BLINK_PHASE, anchor),
            start + BLINK_PHASE * 2
        );
        assert!(state.cursor_dark);
        // A frame inside the phase changes nothing and does not move the
        // deadline.
        assert_eq!(
            state.advance_blink(start + BLINK_PHASE + BLINK_PHASE / 2, anchor),
            start + BLINK_PHASE * 2
        );
        assert!(state.cursor_dark);
        // A long gap - unfocused, occluded - restarts the cycle lit instead of
        // folding every elapsed phase.
        let resumed = start + BLINK_PHASE * 9;
        state.advance_blink(resumed, anchor);
        assert!(!state.cursor_dark);
        state.advance_blink(resumed + BLINK_PHASE, anchor);
        assert!(state.cursor_dark);
        // A cursor that moved - the user typed - is lit again immediately.
        state.advance_blink(resumed + BLINK_PHASE, Some((3, 5)));
        assert!(!state.cursor_dark);
    }

    #[test]
    fn drag_uses_translated_cursor_coordinates_and_clamps_outside_the_grid() {
        let geometry = TerminalGeometry::new(
            Rectangle {
                x: 900.0,
                y: 40.0,
                width: 400.0,
                height: 300.0,
            },
            DEFAULT_FONT_SIZE,
            1.0,
            "",
            LINE_HEIGHT_RATIO,
            DEFAULT_PADDING,
        );
        let cell = geometry.cell_bounds(9, 2).unwrap();
        let translated = Point::new(cell.center_x(), cell.center_y());
        let raw = Point::new(translated.x - 100.0, translated.y);
        let position = drag_position(mouse::Cursor::Available(translated), raw);
        assert_eq!(
            geometry.cell_at(Point::new(
                position.x - geometry.grid_bounds.x,
                position.y - geometry.grid_bounds.y,
            )),
            (9, 2)
        );
        let outside = Point::new(geometry.grid_bounds.x - 10.0, translated.y);
        let position = drag_position(mouse::Cursor::Available(outside), raw);
        assert_eq!(
            geometry.cell_at(Point::new(
                position.x - geometry.grid_bounds.x,
                position.y - geometry.grid_bounds.y,
            )),
            (0, 2)
        );
        assert_eq!(drag_position(mouse::Cursor::Unavailable, raw), raw);
    }

    #[test]
    fn pointer_positions_inside_one_cell_share_a_motion_key() {
        let bounds = Rectangle {
            x: 12.0,
            y: 18.0,
            width: 400.0,
            height: 300.0,
        };
        let geometry = TerminalGeometry::new(
            bounds,
            DEFAULT_FONT_SIZE,
            1.0,
            "",
            LINE_HEIGHT_RATIO,
            DEFAULT_PADDING,
        );
        let key = |x: f32, y: f32| {
            geometry
                .cursor_position(mouse::Cursor::Available(Point::new(x, y)))
                .map(|position| geometry.cell_at(position))
        };
        let cell = geometry.cell_bounds(3, 2).unwrap();
        assert_eq!(key(cell.x + 0.5, cell.y + 0.5), Some((3, 2)));
        assert_eq!(
            key(cell.x + cell.width - 0.5, cell.y + cell.height - 0.5),
            Some((3, 2))
        );
        assert_eq!(key(cell.x + cell.width + 0.5, cell.y + 0.5), Some((4, 2)));
        assert_eq!(key(cell.x + 0.5, cell.y + cell.height + 0.5), Some((3, 3)));
    }

    #[test]
    fn pointer_cells_stay_inside_a_grid_still_awaiting_its_resize() {
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 400.0,
            height: 300.0,
        };
        let geometry = TerminalGeometry::new(
            bounds,
            DEFAULT_FONT_SIZE,
            1.0,
            "",
            LINE_HEIGHT_RATIO,
            DEFAULT_PADDING,
        );
        let corner = geometry
            .cell_bounds(geometry.columns - 1, geometry.rows - 1)
            .unwrap();
        let position = Point::new(
            corner.center_x() - geometry.grid_bounds.x,
            corner.center_y() - geometry.grid_bounds.y,
        );
        let laid_out = (geometry.columns - 1, geometry.rows - 1);
        assert_eq!(geometry.cell_at(position), laid_out);
        // The published grid is still the smaller one from before the resize.
        assert_eq!(geometry.cell_within(position, 5, 3), (4, 2));
        // A larger published grid never extends past the laid-out one.
        assert_eq!(
            geometry.cell_within(position, geometry.columns + 5, geometry.rows + 5),
            laid_out
        );
    }

    #[test]
    fn scrollbar_gutter_never_overlaps_drawn_or_addressable_cells() {
        for scale in [1.0, 1.25, 2.0] {
            for padding in [0.0, 8.0, 16.0] {
                let bounds = Rectangle {
                    x: 10.2,
                    y: 20.4,
                    width: 360.0,
                    height: 220.0,
                };
                let geometry = TerminalGeometry::new(
                    bounds,
                    DEFAULT_FONT_SIZE,
                    scale,
                    "",
                    LINE_HEIGHT_RATIO,
                    padding,
                );
                let scroll = super::super::ScrollSnapshot {
                    total: 100,
                    offset: 80,
                    len: 20,
                    alternate_screen: false,
                    mouse_tracking: false,
                    bracketed_paste: false,
                };
                let bar = TerminalScrollbar::new(bounds, scroll).unwrap();
                assert!(geometry.content_bounds.x + geometry.content_bounds.width <= bar.track.x);
                let final_cell = geometry
                    .cell_bounds(geometry.columns - 1, geometry.rows - 1)
                    .unwrap();
                assert!(final_cell.x + final_cell.width <= bar.track.x);
                assert!(final_cell.y + final_cell.height <= bounds.y + bounds.height);
                assert!(
                    geometry
                        .cursor_position(mouse::Cursor::Available(Point::new(
                            bar.thumb.center_x(),
                            bar.thumb.center_y(),
                        )))
                        .is_none()
                );
                let final_center = Point::new(final_cell.center_x(), final_cell.center_y());
                let position = geometry
                    .cursor_position(mouse::Cursor::Available(final_center))
                    .unwrap();
                assert_eq!(
                    (position.x / geometry.cell_width).floor() as u16,
                    geometry.columns - 1
                );
                assert_eq!(
                    (position.y / geometry.cell_height).floor() as u16,
                    geometry.rows - 1
                );
                assert!(bar.thumb.x >= bar.track.x);
                assert!(bar.thumb.x + bar.thumb.width <= bar.track.x + bar.track.width);
                assert_eq!(bar.row_at(bar.thumb.y, 0.0), scroll.max_offset());
            }
        }
    }

    #[test]
    fn scrollbar_tiny_viewport_keeps_thumb_and_drag_inside_track() {
        let scroll = super::super::ScrollSnapshot {
            total: 100,
            offset: 80,
            len: 20,
            alternate_screen: false,
            mouse_tracking: false,
            bracketed_paste: false,
        };
        for width in [1.0, 4.0, 14.0] {
            let bounds = Rectangle {
                x: 0.0,
                y: 0.0,
                width,
                height: 10.0,
            };
            let bar = TerminalScrollbar::new(bounds, scroll).unwrap();
            assert!(bar.thumb.x >= bounds.x);
            assert!(bar.thumb.x + bar.thumb.width <= bounds.x + bounds.width);
            assert_eq!(bar.thumb.height, bounds.height);
            assert_eq!(bar.row_at(100.0, 0.0), 0);
        }
    }

    fn pty_size(columns: u16) -> TerminalSize {
        TerminalSize {
            columns,
            rows: 40,
            cell_width_px: 10,
            cell_height_px: 21,
        }
    }

    /// Replays a card animating between two widths at 60 Hz, as the workspace
    /// eases its layout, and returns every size the PTY was sent.
    fn animate_card(from: f32, to: f32, pty: &mut PtySize, start: Instant) -> Vec<TerminalSize> {
        const FRAME: Duration = Duration::from_micros(16_667);
        let animation = Duration::from_millis(220);
        let mut applied = Vec::new();
        for frame in 0..60u32 {
            let elapsed = FRAME * frame;
            let progress = (elapsed.as_secs_f32() / animation.as_secs_f32()).min(1.0);
            let eased = 1.0 - (1.0 - progress).powi(3);
            let bounds = Rectangle {
                x: 0.0,
                y: 0.0,
                width: from + (to - from) * eased,
                height: 900.0,
            };
            let geometry =
                TerminalGeometry::new(bounds, DEFAULT_FONT_SIZE, 1.0, "", LINE_HEIGHT_RATIO, 8.0);
            match pty.observe(geometry.terminal_size(), start + elapsed) {
                SizeSync::Apply(size) => {
                    pty.applied(size);
                    applied.push(size);
                }
                SizeSync::Settle(due) => assert!(due > start + elapsed),
                SizeSync::Current => {}
            }
        }
        applied
    }

    fn settled_size(width: f32) -> TerminalSize {
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width,
            height: 900.0,
        };
        TerminalGeometry::new(bounds, DEFAULT_FONT_SIZE, 1.0, "", LINE_HEIGHT_RATIO, 8.0)
            .terminal_size()
    }

    #[test]
    fn an_animated_card_resizes_its_pty_once_at_the_final_width() {
        for (from, to) in [(540.0, 1080.0), (1080.0, 540.0)] {
            let start = Instant::now();
            let mut pty = PtySize::default();
            assert_eq!(
                pty.observe(settled_size(from), start - RESIZE_SETTLE),
                SizeSync::Settle(start)
            );
            assert_eq!(
                pty.observe(settled_size(from), start),
                SizeSync::Apply(settled_size(from))
            );
            pty.applied(settled_size(from));

            // Each frame of the animation crosses a column boundary; the child
            // must see one SIGWINCH for the gesture, not one per frame.
            assert_eq!(
                animate_card(from, to, &mut pty, start),
                vec![settled_size(to)],
                "{from} -> {to}"
            );
        }
    }

    #[test]
    fn a_new_card_growing_from_a_sliver_never_sends_its_first_frame() {
        let mut pty = PtySize::default();
        let applied = animate_card(1.0, 800.0, &mut pty, Instant::now());
        assert_eq!(applied, vec![settled_size(800.0)]);
    }

    #[test]
    fn a_resize_waits_for_its_size_to_hold_still() {
        let start = Instant::now();
        let mut pty = PtySize::default();
        pty.applied(pty_size(80));

        assert_eq!(pty.observe(pty_size(80), start), SizeSync::Current);
        assert_eq!(
            pty.observe(pty_size(90), start),
            SizeSync::Settle(start + RESIZE_SETTLE)
        );
        // Seeing the same size again does not restart its wait.
        let later = start + RESIZE_SETTLE / 2;
        assert_eq!(
            pty.observe(pty_size(90), later),
            SizeSync::Settle(start + RESIZE_SETTLE)
        );
        // A different size does.
        assert_eq!(
            pty.observe(pty_size(95), later),
            SizeSync::Settle(later + RESIZE_SETTLE)
        );
        assert_eq!(
            pty.observe(pty_size(95), later + RESIZE_SETTLE),
            SizeSync::Apply(pty_size(95))
        );
        pty.applied(pty_size(95));
        assert_eq!(
            pty.observe(pty_size(95), later + RESIZE_SETTLE),
            SizeSync::Current
        );
    }

    #[test]
    fn returning_to_the_applied_size_cancels_a_pending_resize() {
        let start = Instant::now();
        let mut pty = PtySize::default();
        pty.applied(pty_size(80));
        assert!(matches!(
            pty.observe(pty_size(60), start),
            SizeSync::Settle(_)
        ));
        assert_eq!(
            pty.observe(pty_size(80), start + Duration::from_millis(10)),
            SizeSync::Current
        );
        // The abandoned size gets a fresh wait if layout produces it again.
        let again = start + RESIZE_SETTLE * 2;
        assert_eq!(
            pty.observe(pty_size(60), again),
            SizeSync::Settle(again + RESIZE_SETTLE)
        );
    }

    #[test]
    fn a_failed_resize_is_retried_without_another_wait() {
        let start = Instant::now();
        let mut pty = PtySize::default();
        pty.observe(pty_size(80), start);
        let due = start + RESIZE_SETTLE;
        assert_eq!(
            pty.observe(pty_size(80), due),
            SizeSync::Apply(pty_size(80))
        );
        // The send failed, so nothing was recorded as applied.
        assert_eq!(
            pty.observe(pty_size(80), due + Duration::from_millis(1)),
            SizeSync::Apply(pty_size(80))
        );
    }

    #[test]
    fn fractional_scale_grid_and_resize_share_one_geometry() {
        let bounds = Rectangle {
            x: 20.0,
            y: 44.0,
            width: 311.3,
            height: 193.7,
        };
        let geometry =
            TerminalGeometry::new(bounds, DEFAULT_FONT_SIZE, 1.25, "", LINE_HEIGHT_RATIO, 4.0);
        let size = geometry.terminal_size();

        assert_eq!(size.columns, geometry.columns);
        assert_eq!(size.rows, geometry.rows);
        assert_eq!(size.cell_width_px, 11);
        assert_eq!(size.cell_height_px, 23);
        assert_eq!(geometry.cell_width * geometry.scale_factor, 11.0);
        assert_eq!(geometry.cell_height * geometry.scale_factor, 23.0);
        assert!(geometry.content_bounds.y > 44.0);
    }

    #[test]
    fn panning_fractional_scale_terminal_keeps_grid_and_pty_extent_stable() {
        let geometry_at = |x| {
            TerminalGeometry::new(
                Rectangle {
                    x,
                    y: 44.0,
                    width: 483.2,
                    height: 376.0,
                },
                DEFAULT_FONT_SIZE,
                1.25,
                "",
                LINE_HEIGHT_RATIO,
                4.0,
            )
        };
        let baseline = geometry_at(0.0);

        assert_eq!(baseline.rows, 20);
        assert_eq!(baseline.cell_width_px, 11);
        assert_eq!(baseline.cell_height_px, 23);
        assert_eq!(baseline.screen_height_px(), 460);

        assert!(
            geometry_at(0.2).content_bounds.width <= baseline.content_bounds.width,
            "absolute edge snapping should still conservatively clip the fixed grid"
        );

        for x in [0.2, 0.4, 0.6, 117.37] {
            let panned = geometry_at(x);

            assert_eq!(panned.terminal_size(), baseline.terminal_size());
            assert_eq!(panned.screen_width_px(), baseline.screen_width_px());
            assert_eq!(panned.screen_height_px(), baseline.screen_height_px());
            assert_eq!(panned.grid_bounds.width, baseline.grid_bounds.width);
            assert_eq!(panned.grid_bounds.height, baseline.grid_bounds.height);
            assert_eq!(panned.grid_bounds.y, baseline.grid_bounds.y);
            assert_eq!(panned.grid_bounds.x, ((x + 4.0) * 1.25).ceil() / 1.25);
        }
    }

    #[test]
    fn configured_spacing_and_padding_share_resize_drawing_and_mouse_boundaries() {
        let bounds = Rectangle {
            x: 10.2,
            y: 20.4,
            width: 360.0,
            height: 220.0,
        };
        let geometry = TerminalGeometry::new(bounds, 16.0, 1.25, "", 1.75, 16.0);
        let size = geometry.terminal_size();
        assert_eq!(size.cell_height_px, 35);
        assert_eq!(size.rows, 6);
        assert_eq!(
            size.columns,
            ((bounds.width - 32.0 - SCROLLBAR_GUTTER) * 1.25 / size.cell_width_px as f32).floor()
                as u16
        );
        let second_cell = geometry.cell_bounds(1, 1).unwrap();
        let position = geometry
            .cursor_position(mouse::Cursor::Available(Point::new(
                second_cell.x + second_cell.width / 2.0,
                second_cell.y + second_cell.height / 2.0,
            )))
            .unwrap();
        assert!((position.x * 1.25 - size.cell_width_px as f32 * 1.5).abs() < 0.001);
        assert!((position.y * 1.25 - size.cell_height_px as f32 * 1.5).abs() < 0.001);
        assert!(
            geometry
                .cursor_position(mouse::Cursor::Available(Point::new(
                    bounds.x + 8.0,
                    bounds.y + 8.0,
                )))
                .is_none()
        );
        let last = geometry
            .cell_bounds(size.columns - 1, size.rows - 1)
            .unwrap();
        assert!(last.x + last.width <= bounds.x + bounds.width - 16.0);
        assert!(last.y + last.height <= bounds.y + bounds.height - 16.0);
    }

    #[test]
    fn last_cell_and_clip_stay_inside_terminal_body_and_column() {
        let bounds = Rectangle {
            x: 400.0,
            y: 44.0,
            width: 317.3,
            height: 401.2,
        };
        let geometry =
            TerminalGeometry::new(bounds, DEFAULT_FONT_SIZE, 1.25, "", LINE_HEIGHT_RATIO, 4.0);
        let last = geometry
            .cell_bounds(geometry.columns - 1, geometry.rows - 1)
            .unwrap();
        let right = |rectangle: Rectangle| rectangle.x + rectangle.width;
        let bottom = |rectangle: Rectangle| rectangle.y + rectangle.height;

        assert!(last.x >= geometry.content_bounds.x);
        assert!(last.y >= geometry.content_bounds.y);
        assert!(right(last) <= right(geometry.content_bounds));
        assert!(bottom(last) <= bottom(geometry.content_bounds));
        assert!(right(geometry.content_bounds) < right(bounds));

        let neighboring_viewport = Rectangle {
            x: bounds.x + 120.0,
            y: bounds.y,
            width: 80.0,
            height: bounds.height,
        };
        let clip = geometry
            .content_bounds
            .intersection(&neighboring_viewport)
            .unwrap();
        assert!(clip.x >= neighboring_viewport.x);
        assert!(right(clip) <= right(neighboring_viewport));
    }

    #[test]
    fn the_text_anchor_is_the_first_cell_corner_inside_the_grid() {
        // A raw buffer is drawn from its top-left corner and its line box is
        // exactly one cell high, so the anchor must be the cell corner. The old
        // centre anchor belonged to iced's `Vertical::Center` text path; keeping
        // it here would push every row half a cell down.
        let geometry = TerminalGeometry::new(
            Rectangle {
                x: 20.0,
                y: 44.0,
                width: 300.0,
                height: 200.0,
            },
            DEFAULT_FONT_SIZE,
            1.25,
            "",
            LINE_HEIGHT_RATIO,
            4.0,
        );
        let first_cell = geometry.cell_bounds(0, 0).unwrap();
        let origin = geometry.text_origin(first_cell);

        assert_eq!(origin.x, first_cell.x);
        assert_eq!(origin.y, first_cell.y);
        assert!(origin.y >= geometry.content_bounds.y);
        let bottom = geometry.content_bounds.y + geometry.content_bounds.height;
        assert!(origin.y + geometry.cell_height <= bottom);
    }

    #[test]
    fn out_of_grid_cursor_coordinates_have_no_drawable_cell() {
        let geometry = TerminalGeometry::new(
            Rectangle {
                x: 0.0,
                y: 44.0,
                width: 300.0,
                height: 200.0,
            },
            DEFAULT_FONT_SIZE,
            1.5,
            "",
            LINE_HEIGHT_RATIO,
            4.0,
        );

        assert!(
            geometry
                .cell_bounds(geometry.columns, geometry.rows - 1)
                .is_none()
        );
        assert!(
            geometry
                .cell_bounds(geometry.columns - 1, geometry.rows)
                .is_none()
        );
    }

    #[test]
    fn a_uniform_row_coalesces_into_one_run_and_a_styled_span_splits_it() {
        let plain = cells("hello world", |_, cell| cell);
        let keyed = runs(&plain, |cell| (cell.foreground, cell.style)).count();
        assert_eq!(keyed, 1, "a uniformly styled row must be one run");

        // A trailing styled span splits the row in two, and the space joins the
        // unstyled run because it carries the same key.
        let suffix = cells("hello world", |index, mut cell| {
            cell.style.bold = index >= 6;
            cell
        });
        let split = runs(&suffix, |cell| (cell.foreground, cell.style)).collect::<Vec<_>>();
        assert_eq!(
            split
                .iter()
                .map(|run| (run.start, run.len, run.key.1.bold))
                .collect::<Vec<_>>(),
            vec![(0, 6, false), (6, 5, true)]
        );

        // An interior span splits it in three, bounded exactly by the styling.
        let interior = cells("hello world", |index, mut cell| {
            cell.style.bold = (6..9).contains(&index);
            cell
        });
        let split = runs(&interior, |cell| (cell.foreground, cell.style)).collect::<Vec<_>>();
        assert_eq!(
            split
                .iter()
                .map(|run| (run.start, run.len, run.key.1.bold))
                .collect::<Vec<_>>(),
            vec![(0, 6, false), (6, 3, true), (9, 2, false)]
        );
    }

    #[test]
    fn a_run_spans_its_cells_and_stops_at_the_grid_edge() {
        let geometry = TerminalGeometry::new(
            Rectangle {
                x: 0.0,
                y: 0.0,
                width: 300.0,
                height: 200.0,
            },
            DEFAULT_FONT_SIZE,
            1.0,
            "",
            LINE_HEIGHT_RATIO,
            4.0,
        );
        let single = geometry.run_bounds(3, 1, 0).unwrap();
        let five = geometry.run_bounds(3, 5, 0).unwrap();
        assert_eq!(five.x, single.x, "a run starts at its first cell");
        assert_eq!(five.width, single.width * 5.0);
        assert_eq!(five.height, single.height);

        // A run reaching past the last column is clamped, never extended.
        let clamped = geometry.run_bounds(geometry.columns - 2, 10, 0).unwrap();
        let last = geometry.run_bounds(geometry.columns - 1, 1, 0).unwrap();
        assert!(clamped.x + clamped.width <= last.x + last.width + f32::EPSILON);
        assert!(geometry.run_bounds(0, 0, 0).is_none());
    }

    fn cells(text: &str, style: impl Fn(usize, CellSnapshot) -> CellSnapshot) -> Vec<CellSnapshot> {
        text.chars()
            .enumerate()
            .map(|(index, character)| {
                style(
                    index,
                    CellSnapshot {
                        text: CellText::new(&character.to_string()),
                        ..CellSnapshot::default()
                    },
                )
            })
            .collect()
    }

    #[test]
    fn the_grid_pitch_is_a_whole_number_of_physical_pixels() {
        // Backgrounds, the cursor and mouse hit-testing all derive from the
        // pitch, so a fractional pitch would put them on half pixels at every
        // scale factor a fractional display actually reports.
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let metrics = CellMetrics::resolve(DEFAULT_FONT_SIZE, scale, "");
            let physical = metrics.width * scale;
            assert!(
                (physical - physical.round()).abs() <= 1e-3,
                "scale {scale}: pitch {physical}px is not a whole pixel"
            );
            let physical = metrics.height * scale;
            assert!(
                (physical - physical.round()).abs() <= 1e-3,
                "scale {scale}: row height {physical}px is not a whole pixel"
            );
        }
    }

    /// Every installed family whose faces `fontdb` flags as `monospaced` or
    /// not, interned and deduplicated.
    ///
    /// The flag is the font's own metadata, which is exactly what the grid
    /// cannot trust - `AR PL New Sung Mono` claims to be monospaced and draws
    /// Latin proportionally - so this is a source of candidate names, nothing
    /// more.
    fn installed_families(monospaced: bool) -> Vec<&'static str> {
        let guard = font_system()
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut names: Vec<&'static str> = guard
            .db()
            .faces()
            .filter(|face| face.monospaced == monospaced)
            .filter_map(|face| face.families.first())
            .map(|(name, _)| intern_family(name))
            .collect();
        drop(guard);
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Whether every face of `family` measures fixed-pitch, which is what
    /// [`CellMetrics::measure`] accepts a family on.
    fn family_is_fixed_pitch(font_size: f32, family: &'static str) -> bool {
        (0..FACE_COUNT).all(|index| {
            let key = Face::from_index(index);
            advance(ADVANCE_SAMPLE, font_size, family, key)
                .is_none_or(|base| is_fixed_pitch(font_size, family, key, base))
        })
    }

    #[test]
    fn the_grid_pitch_follows_the_font_family() {
        // `resolve` memoises on its inputs, so a family missing from that key
        // would hand a newly chosen family the previous one's pitch and drift
        // every glyph off its column.
        for family in installed_families(true) {
            let metrics = CellMetrics::resolve(DEFAULT_FONT_SIZE, 1.0, family);
            let expected = advance(
                ADVANCE_SAMPLE,
                DEFAULT_FONT_SIZE,
                metrics.family,
                Face::PLAIN,
            )
            .unwrap_or((DEFAULT_FONT_SIZE * 0.6).max(1.0))
            .round()
            .max(1.0);

            assert_eq!(
                metrics.width, expected,
                "{family} was memoised at another family's pitch"
            );
            assert_eq!(
                metrics.family == family,
                family_is_fixed_pitch(DEFAULT_FONT_SIZE, family),
                "{family} must be drawn with exactly when every face of it \
                 measures fixed-pitch"
            );
        }

        // A name the system cannot resolve is served by an arbitrary face from
        // cosmic-text's globally sorted match list, so the only thing that can
        // be asserted of it is that same invariant.
        for name in ["Cagents Test Alpha", "Cagents Test Beta"] {
            let metrics = CellMetrics::resolve(DEFAULT_FONT_SIZE, 1.0, name);
            assert_eq!(
                metrics.family == name,
                family_is_fixed_pitch(DEFAULT_FONT_SIZE, intern_family(name)),
                "{name} names no installed family and must only be drawn with \
                 if whatever answered for it holds the grid"
            );
        }
        assert_eq!(
            CellMetrics::resolve(DEFAULT_FONT_SIZE, 1.0, "   ").family,
            "",
            "a family that names nothing must fall back to the system monospace"
        );
    }

    #[test]
    fn a_family_name_is_interned_once_and_an_empty_name_is_the_system_monospace() {
        let first = intern_family("Cagents Interned Family");
        let second = intern_family(&format!("  {first}  "));

        assert!(
            first.as_ptr() == second.as_ptr(),
            "a repeated family must reuse its allocation instead of leaking again"
        );
        assert_eq!(
            font("", Face::PLAIN).family,
            cosmic::iced::font::Family::Monospace
        );
        assert_eq!(
            font(first, Face::PLAIN).family,
            cosmic::iced::font::Family::Name(first)
        );
    }

    /// Shapes `cells` through the real text stack and returns the glyph
    /// positions along with the width the shaper reports for the whole run.
    fn shaped_positions(
        metrics: &CellMetrics,
        cells: &[CellSnapshot],
        single: bool,
    ) -> (Vec<f32>, f32) {
        let mut content = String::new();
        let mut spans = Vec::new();
        let mut guard = font_system()
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let buffer = shape_segment(
            guard.raw(),
            metrics,
            cells,
            &mut content,
            &mut spans,
            single,
        );
        drop(guard);
        let mut positions = Vec::new();
        let mut width = 0.0f32;
        for run in buffer.layout_runs() {
            positions.extend(run.glyphs.iter().map(|glyph| glyph.x));
            width += run.line_w;
        }
        (positions, width)
    }

    /// Asserts that shaping `cells` as one buffer puts the Nth glyph exactly N
    /// pitches along and leaves the run exactly as wide as its columns.
    fn assert_columns(metrics: &CellMetrics, cells: &[CellSnapshot], case: &str) {
        let (positions, width) = shaped_positions(metrics, cells, false);

        assert_eq!(positions.len(), cells.len(), "{case}: one glyph per cell");
        for (column, x) in positions.iter().enumerate() {
            let expected = metrics.width * column as f32;
            assert!(
                (x - expected).abs() <= 1e-3,
                "{case}: column {column} drew at {x}, expected {expected}"
            );
        }
        let expected = metrics.width * cells.len() as f32;
        assert!(
            (width - expected).abs() <= 1e-3,
            "{case}: run is {width} wide, expected {expected}"
        );
    }

    #[test]
    fn every_shaped_glyph_lands_on_its_own_column() {
        // The invariant the whole design rests on, and the one a previous
        // attempt at coalescing broke: drawing N cells as a single shaped
        // primitive must put the Nth glyph exactly N pitches along. Measured
        // from the real shaper at the scale factors a fractional display
        // reports, not derived from an advance estimate.
        //
        // The family is user-settable, which is the axis the invariant breaks
        // along, so every installed monospace family is exercised as well as
        // the default, and at more than one font size: the pitch is rounded to
        // whole physical pixels, so each size divides a face's own advance
        // differently.
        let cells = cells("the quick brown fox 0123456789", |_, cell| cell);
        let mut families = installed_families(true);
        families.push("");
        for family in families {
            for (font_size, scale) in [
                (DEFAULT_FONT_SIZE, 1.0f32),
                (DEFAULT_FONT_SIZE, 1.25),
                (DEFAULT_FONT_SIZE, 1.5),
                (DEFAULT_FONT_SIZE, 2.0),
                (11.0, 1.0),
                (23.0, 1.25),
            ] {
                let metrics = CellMetrics::resolve(font_size, scale, family);
                assert_columns(
                    &metrics,
                    &cells,
                    &format!("{family:?} at {font_size}px, scale {scale}"),
                );
            }
        }
    }

    #[test]
    fn a_family_that_is_not_fixed_pitch_falls_back_instead_of_shearing_the_grid() {
        // The pitch comes from `M` alone, so a family whose other glyphs
        // advance less puts the Nth glyph of a coalesced run left of column N
        // while its background, selection, decorations and cursor stay on the
        // pitch. Nothing downstream can catch that: cosmic-text answers an
        // unknown or proportional family with an arbitrary installed face, so a
        // finite advance is no evidence of a usable grid.
        let Some(proportional) = installed_families(false)
            .into_iter()
            .find(|family| !family_is_fixed_pitch(DEFAULT_FONT_SIZE, family))
        else {
            // Nothing to exercise on a host with no proportional font.
            return;
        };
        let cells = cells("the quick brown fox 0123456789", |_, cell| cell);

        // The family really is hostile: measured as `measure` used to, with the
        // pitch taken from `M` and no fixed-pitch check, its glyphs leave their
        // columns by whole cells.
        let width = advance(ADVANCE_SAMPLE, DEFAULT_FONT_SIZE, proportional, Face::PLAIN)
            .expect("a resolvable family measures an advance")
            .round()
            .max(1.0);
        let unchecked = CellMetrics {
            width,
            height: (DEFAULT_FONT_SIZE * LINE_HEIGHT_RATIO).round(),
            faces: std::array::from_fn(|index| {
                let key = Face::from_index(index);
                advance(ADVANCE_SAMPLE, DEFAULT_FONT_SIZE, proportional, key)
                    .map_or(DEFAULT_FONT_SIZE, |base| DEFAULT_FONT_SIZE * width / base)
            }),
            family: proportional,
        };
        let (sheared, _) = shaped_positions(&unchecked, &cells, false);
        assert!(
            sheared
                .iter()
                .enumerate()
                .any(|(column, x)| (x - width * column as f32).abs() > width),
            "{proportional} was chosen for shearing the grid but did not"
        );

        // So it must not be drawn with at all.
        let metrics = CellMetrics::resolve(DEFAULT_FONT_SIZE, 1.0, proportional);
        assert_eq!(
            metrics,
            CellMetrics::resolve(DEFAULT_FONT_SIZE, 1.0, ""),
            "{proportional} must fall back to the system monospace whole"
        );
        assert_columns(&metrics, &cells, proportional);
    }

    #[test]
    fn a_style_change_inside_a_run_does_not_move_the_grid() {
        // Bold and italic faces have their own advances, which is why the
        // previous design had to split a run at every style change. The
        // monospace snap removes that constraint, so spans must now be able to
        // carry the style without moving a single column.
        let metrics = CellMetrics::resolve(DEFAULT_FONT_SIZE, 1.0, "");
        let cells = cells("mixed weight text", |index, mut cell| {
            cell.style.bold = index % 3 == 0;
            cell.style.italic = index % 4 == 0;
            cell
        });

        assert_columns(&metrics, &cells, "mixed styles");
    }

    #[test]
    fn faint_text_reaches_the_renderer_with_translucent_foreground_only() {
        let metrics = CellMetrics::resolve(DEFAULT_FONT_SIZE, 1.0, "");
        let cells = cells("ABC", |index, mut cell| {
            cell.foreground = Rgb {
                red: 200,
                green: 100,
                blue: 50,
            };
            cell.background = Rgb {
                red: 20,
                green: 40,
                blue: 60,
            };
            cell.style.faint = index != 0;
            cell.selected = index == 2;
            cell
        });
        let mut content = String::new();
        let mut spans = Vec::new();
        let mut guard = font_system()
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let buffer = shape_segment(
            guard.raw(),
            &metrics,
            &cells,
            &mut content,
            &mut spans,
            false,
        );
        drop(guard);
        let colors: Vec<_> = buffer
            .layout_runs()
            .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.color_opt.unwrap()))
            .collect();
        assert_eq!(
            colors,
            [
                cosmic_text::Color::rgba(200, 100, 50, 255),
                cosmic_text::Color::rgba(200, 100, 50, 128),
                cosmic_text::Color::rgba(20, 40, 60, 128),
            ]
        );
    }

    #[test]
    fn segments_coalesce_ascii_and_isolate_everything_else() {
        let plain = cells("hello world", |_, cell| cell);
        assert_eq!(
            segments(&plain).collect::<Vec<_>>(),
            vec![Segment {
                start: 0,
                len: 11,
                single: false
            }],
            "a plain ASCII row is one segment, spaces included"
        );

        // A blank ends a segment and draws nothing; a wide cluster is isolated
        // so that its two-column advance cannot displace the cells after it.
        let mixed = cells("ab漢cd", |index, mut cell| {
            if index == 1 {
                cell.text = CellText::new("");
            }
            cell
        });
        assert_eq!(
            segments(&mixed).collect::<Vec<_>>(),
            vec![
                Segment {
                    start: 0,
                    len: 1,
                    single: false
                },
                Segment {
                    start: 2,
                    len: 1,
                    single: true
                },
                Segment {
                    start: 3,
                    len: 2,
                    single: false
                },
            ]
        );

        // An invisible cell draws nothing, so it ends a segment too.
        let hidden = cells("abcd", |index, mut cell| {
            cell.style.invisible = index == 2;
            cell
        });
        assert_eq!(
            segments(&hidden)
                .map(|segment| (segment.start, segment.len))
                .collect::<Vec<_>>(),
            vec![(0, 2), (3, 1)]
        );
    }

    #[test]
    fn printed_links_use_cell_columns_and_preserve_balanced_url_punctuation() {
        let line = row("é (https://example.org/a_(b)). next");
        assert_eq!(
            printed_link_at(&line, 8).unwrap().as_str(),
            "https://example.org/a_(b)"
        );
        assert!(printed_link_at(&line, 1).is_none());
        assert!(printed_link_at(&row("javascript:alert(1)"), 5).is_none());
        assert_eq!(
            printed_link_at(&row("(file:///tmp/report%20one.html)."), 8)
                .unwrap()
                .to_file_path()
                .unwrap(),
            std::path::PathBuf::from("/tmp/report one.html")
        );
        assert!(printed_link_at(&row("file://remote/etc/passwd"), 5).is_none());
        assert_eq!(
            printed_link_at(&row("https://example.org/a?b=1&c=2"), 10)
                .unwrap()
                .query(),
            Some("b=1&c=2")
        );
    }

    fn row(text: &str) -> Arc<RowSnapshot> {
        Arc::new(RowSnapshot {
            wrapped: false,
            hyperlinks: Vec::new(),
            cells: cells(text, |_, cell| cell),
        })
    }

    #[test]
    fn copying_soft_wrapped_rows_preserves_spaces_and_hard_breaks() {
        let mut first = (*row("long ")).clone();
        first.wrapped = true;
        let rows = vec![Arc::new(first), row("path"), row("next")];
        let grid = selection_grid(&rows, 5);
        assert_eq!(
            selection_text(grid, (at(7, 0), at(9, 3))),
            "long path\nnext"
        );
    }

    #[test]
    fn shift_insert_pastes_without_ctrl_or_alt() {
        let key = keyboard::Key::Named(keyboard::key::Named::Insert);
        let physical = keyboard::key::Physical::Code(keyboard::key::Code::Insert);
        assert_eq!(
            clipboard_chord(&key, physical, keyboard::Modifiers::SHIFT),
            Some(ClipboardChord::Paste)
        );
        assert_eq!(
            clipboard_chord(
                &key,
                physical,
                keyboard::Modifiers::SHIFT | keyboard::Modifiers::ALT
            ),
            None
        );
    }

    #[test]
    fn links_use_osc8_wrapping_and_cwd_relative_file_positions() {
        let mut first = (*row("https://exam")).clone();
        first.wrapped = true;
        let snapshot = RenderSnapshot {
            lines: vec![Arc::new(first), row("ple.org/a?b=1")].into(),
            ..RenderSnapshot::empty()
        };
        assert_eq!(
            terminal_link_at(&snapshot, 1, 3, std::path::Path::new("/repo"))
                .unwrap()
                .as_str(),
            "https://example.org/a?b=1"
        );
        let snapshot = RenderSnapshot {
            lines: vec![row("src/lib.rs:42:5")].into(),
            ..RenderSnapshot::empty()
        };
        assert_eq!(
            terminal_link_at(&snapshot, 0, 4, std::path::Path::new("/repo"))
                .unwrap()
                .as_str(),
            "file:///repo/src/lib.rs#L42:5"
        );
        let mut linked = (*row("friendly label")).clone();
        linked
            .hyperlinks
            .push(crate::terminal::snapshot::HyperlinkSpan {
                start: 0,
                end: 14,
                uri: "https://example.org/path".into(),
            });
        assert_eq!(
            printed_link_at(&linked, 9).unwrap().as_str(),
            "https://example.org/path"
        );
    }

    fn shape_cache_geometry(family: &str) -> TerminalGeometry {
        TerminalGeometry::new(
            Rectangle {
                x: 0.0,
                y: 0.0,
                width: 900.0,
                height: 400.0,
            },
            DEFAULT_FONT_SIZE,
            1.0,
            family,
            LINE_HEIGHT_RATIO,
            4.0,
        )
    }

    /// The shaped buffer of every cached row.
    fn shaped_buffers(cache: &ShapeCache) -> Vec<Arc<cosmic_text::Buffer>> {
        cache
            .rows
            .iter()
            .map(|cached| {
                // A plain ASCII row coalesces into exactly one segment, so a
                // reshape that failed to drop the previous one shows up here.
                assert_eq!(
                    cached.segments.len(),
                    1,
                    "a reshaped row must replace its segments, not append to them"
                );
                Arc::clone(&cached.segments[0].buffer)
            })
            .collect()
    }

    #[test]
    fn shaped_rows_survive_equal_content_but_not_changed_content() {
        let geometry = shape_cache_geometry("");
        let mut cache = ShapeCache::default();
        let mut rows = vec![row("first row"), row("second row")];

        cache.refresh(&geometry, DEFAULT_FONT_SIZE, &rows, 80);
        let baseline = shaped_buffers(&cache);
        assert_eq!(baseline.len(), rows.len(), "every row shapes");

        // Nothing moved, so nothing reshapes and the font system is never
        // touched again.
        cache.refresh(&geometry, DEFAULT_FONT_SIZE, &rows, 80);
        for (before, after) in baseline.iter().zip(&shaped_buffers(&cache)) {
            assert!(
                Arc::ptr_eq(before, after),
                "an untouched row must keep its buffer"
            );
        }

        // IPC reconstructs moved rows in fresh allocations. Equal content
        // still has exactly the same glyphs and attributes.
        rows[1] = row("second row");
        cache.refresh(&geometry, DEFAULT_FONT_SIZE, &rows, 80);
        let replaced = shaped_buffers(&cache);
        assert!(
            Arc::ptr_eq(&baseline[0], &replaced[0]),
            "a row nobody touched must not reshape"
        );
        assert!(
            Arc::ptr_eq(&baseline[1], &replaced[1]),
            "equal content must retain its shaped buffer"
        );
        assert!(Arc::ptr_eq(
            cache.rows[1].source.as_ref().unwrap(),
            &rows[1]
        ));

        // And a row whose content actually changed, for the same reason.
        rows[0] = row("FIRST ROW");
        cache.refresh(&geometry, DEFAULT_FONT_SIZE, &rows, 80);
        let rewritten = shaped_buffers(&cache);
        assert!(!Arc::ptr_eq(&replaced[0], &rewritten[0]));
        assert!(
            Arc::ptr_eq(&replaced[1], &rewritten[1]),
            "one rewritten row must not cost the others their buffers"
        );
    }

    #[test]
    fn scrolling_reuses_overlapping_rows_reconstructed_by_ipc() {
        let geometry = shape_cache_geometry("");
        let mut cache = ShapeCache::default();
        cache.scroll_to(100);
        cache.refresh(
            &geometry,
            DEFAULT_FONT_SIZE,
            &[row("one"), row("two"), row("three")],
            80,
        );
        let before = shaped_buffers(&cache);
        cache.scroll_to(101);
        cache.refresh(
            &geometry,
            DEFAULT_FONT_SIZE,
            &[row("two"), row("three"), row("four")],
            80,
        );
        let down = shaped_buffers(&cache);
        assert!(Arc::ptr_eq(&before[1], &down[0]));
        assert!(Arc::ptr_eq(&before[2], &down[1]));
        assert!(!Arc::ptr_eq(&before[0], &down[2]));

        cache.scroll_to(100);
        // Concurrent output changed the middle row: scrolling cannot reuse it.
        cache.refresh(
            &geometry,
            DEFAULT_FONT_SIZE,
            &[row("one"), row("TWO"), row("three")],
            80,
        );
        let up = shaped_buffers(&cache);
        assert!(!Arc::ptr_eq(&down[0], &up[1]));
        assert!(Arc::ptr_eq(&down[1], &up[2]));

        cache.scroll_to(u64::MAX);
        cache.refresh(&geometry, DEFAULT_FONT_SIZE, &[row("new page")], 80);
        assert_eq!(shaped_buffers(&cache).len(), 1);
    }

    #[test]
    fn changing_line_height_reshapes_unchanged_text_to_the_new_cell_box() {
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 900.0,
            height: 400.0,
        };
        let compact = TerminalGeometry::new(bounds, 16.0, 1.25, "", 1.0, 8.0);
        let spacious = TerminalGeometry::new(bounds, 16.0, 1.25, "", 2.0, 8.0);
        let rows = vec![row("unchanged text")];
        let mut cache = ShapeCache::default();
        cache.refresh(&compact, 16.0, &rows, 80);
        assert_eq!(cache.rows[0].segments[0].buffer.metrics().line_height, 16.0);
        cache.refresh(&spacious, 16.0, &rows, 80);
        assert_eq!(cache.rows[0].segments[0].buffer.metrics().line_height, 32.0);
    }

    #[test]
    fn a_shape_parameter_change_invalidates_every_cached_row() {
        // The font size, the column limit and the family all change what a row
        // shapes to and none of them is visible in a row's pointer, so they are
        // compared separately and clear the cache whole.
        let geometry = shape_cache_geometry("");
        let rows = vec![row("first row"), row("second row")];
        let invalidates = |geometry: &TerminalGeometry, font_size: f32, columns: usize| {
            let mut cache = ShapeCache::default();
            cache.refresh(&shape_cache_geometry(""), DEFAULT_FONT_SIZE, &rows, 80);
            let before = shaped_buffers(&cache);
            cache.refresh(geometry, font_size, &rows, columns);
            before
                .iter()
                .zip(&shaped_buffers(&cache))
                .all(|(before, after)| !Arc::ptr_eq(before, after))
        };

        assert!(
            invalidates(&geometry, DEFAULT_FONT_SIZE + 2.0, 80),
            "a font size change must reshape every row"
        );
        assert!(
            invalidates(&geometry, DEFAULT_FONT_SIZE, 4),
            "a column limit change must reshape every row"
        );
        assert!(
            !invalidates(&geometry, DEFAULT_FONT_SIZE, 80),
            "unchanged parameters must reshape nothing"
        );

        // A family only counts as a change when the host has one the metrics
        // will actually keep.
        if let Some(family) = installed_families(true)
            .into_iter()
            .find(|family| CellMetrics::resolve(DEFAULT_FONT_SIZE, 1.0, family).family == *family)
        {
            assert!(
                invalidates(&shape_cache_geometry(family), DEFAULT_FONT_SIZE, 80),
                "a family change must reshape every row"
            );
        }
    }

    /// A grid whose first row sits well inside scrollback, so that anything
    /// confusing a viewport row for an absolute one fails here.
    fn selection_grid(rows: &[Arc<RowSnapshot>], columns: u16) -> SelectionGrid<'_> {
        SelectionGrid {
            rows,
            offset: 7,
            columns,
        }
    }

    fn at(row: u64, column: u16) -> SelectionCell {
        SelectionCell { row, column }
    }

    #[test]
    fn a_selection_covers_the_same_span_whichever_way_it_was_dragged() {
        let rows = vec![row("first row"), row("second row"), row("third row")];
        let grid = selection_grid(&rows, 12);
        let dragged = |anchor, head| {
            Selection {
                anchor,
                head,
                mode: SelectionMode::Character,
            }
            .span(grid)
        };

        let (top, bottom) = (at(7, 4), at(9, 2));
        assert_eq!(dragged(top, bottom), (top, bottom));
        assert_eq!(dragged(bottom, top), (top, bottom));
        assert_eq!(
            selection_text(grid, dragged(bottom, top)),
            selection_text(grid, dragged(top, bottom))
        );

        // And leftward within one row, where only the column differs.
        let (left, right) = (at(8, 1), at(8, 4));
        assert_eq!(dragged(right, left), (left, right));
    }

    #[test]
    fn selected_text_trims_each_row_and_joins_them_with_newlines() {
        let rows = vec![row("cargo check     "), row("ok              ")];
        let grid = selection_grid(&rows, 16);
        assert_eq!(
            selection_text(grid, (at(7, 0), at(8, 15))),
            "cargo check\nok"
        );
        // A partial first row still runs to the end of that row, and only the
        // last row stops at the head column.
        assert_eq!(selection_text(grid, (at(7, 6), at(8, 0))), "check\no");

        // A wide cluster's trailing cell carries no text, so the glyph its
        // first cell contributed is not repeated.
        let wide = Arc::new(RowSnapshot {
            wrapped: false,
            hyperlinks: Vec::new(),
            cells: vec![
                CellSnapshot {
                    text: CellText::new("日"),
                    ..CellSnapshot::default()
                },
                CellSnapshot::default(),
                CellSnapshot {
                    text: CellText::new("x"),
                    ..CellSnapshot::default()
                },
            ],
        });
        let rows = vec![wide];
        assert_eq!(
            selection_text(selection_grid(&rows, 3), (at(7, 0), at(7, 2))),
            "日x"
        );
    }

    #[test]
    fn copying_cursor_addressed_output_preserves_unwritten_spaces() {
        let rows = vec![Arc::new(RowSnapshot {
            wrapped: false,
            hyperlinks: Vec::new(),
            cells: vec![
                CellSnapshot {
                    text: CellText::new("a"),
                    ..CellSnapshot::default()
                },
                CellSnapshot::default(),
                CellSnapshot {
                    text: CellText::new("日"),
                    ..CellSnapshot::default()
                },
                CellSnapshot::default(),
                CellSnapshot::default(),
                CellSnapshot {
                    text: CellText::new("b"),
                    ..CellSnapshot::default()
                },
            ],
        })];
        assert_eq!(
            selection_text(selection_grid(&rows, 6), (at(7, 0), at(7, 5))),
            "a 日 b"
        );
    }

    #[test]
    fn blank_terminal_rows_and_right_padding_do_not_start_selection() {
        let rows = vec![
            row("  prompt   "),
            row("           "),
            Arc::new(RowSnapshot::default()),
        ];
        let grid = selection_grid(&rows, 11);
        assert_eq!(
            grid.selection_start(at(7, 0)),
            Some(at(7, 0)),
            "indentation belongs to output"
        );
        assert_eq!(grid.selection_start(at(7, 7)), Some(at(7, 7)));
        assert_eq!(
            grid.selection_start(at(7, 8)),
            None,
            "right padding is not text"
        );
        assert_eq!(grid.selection_start(at(8, 0)), None);
        assert_eq!(grid.selection_start(at(9, 0)), None);
        assert_eq!(grid.selection_start(at(10, 0)), None);
        assert_eq!(grid.selected_columns((at(7, 8), at(9, 10)), 7), None);
        assert_eq!(selection_text(grid, (at(8, 0), at(9, 10))), "");
    }

    #[test]
    fn selection_ends_at_output_but_preserves_interior_blank_lines_and_spaces() {
        let rows = vec![
            row("first line    "),
            row("              "),
            row("last   line   "),
            row("              "),
        ];
        let grid = selection_grid(&rows, 14);
        let span = (at(7, 0), at(10, 13));
        assert_eq!(selection_text(grid, span), "first line\n\nlast   line");
        assert_eq!(grid.selected_columns(span, 7), Some((0, 9)));
        assert_eq!(grid.selected_columns(span, 8), None);
        assert_eq!(grid.selected_columns(span, 9), Some((0, 10)));
        assert_eq!(grid.selected_columns(span, 10), None);
        assert_eq!(
            grid.selected_columns((at(9, 4), at(9, 6)), 9),
            Some((4, 6)),
            "interior spaces can be selected"
        );
        assert_eq!(
            selection_text(grid, (at(0, 0), at(u64::MAX, 13))),
            "first line\n\nlast   line",
            "capture bounds limit old scrollback selections"
        );
    }

    #[test]
    fn selections_cover_whole_wide_and_combining_graphemes() {
        let rows = vec![Arc::new(RowSnapshot {
            wrapped: false,
            hyperlinks: Vec::new(),
            cells: vec![
                CellSnapshot {
                    text: CellText::new("e\u{301}"),
                    ..CellSnapshot::default()
                },
                CellSnapshot {
                    text: CellText::new("日"),
                    ..CellSnapshot::default()
                },
                CellSnapshot::default(),
                CellSnapshot::default(),
            ],
        })];
        let grid = selection_grid(&rows, 4);
        assert_eq!(
            grid.selection_start(at(7, 2)),
            Some(at(7, 2)),
            "wide tail remains a distinct gesture anchor"
        );
        assert_eq!(grid.selection_start(at(7, 3)), None);
        assert_eq!(grid.selected_columns((at(7, 1), at(7, 1)), 7), Some((1, 2)));
        assert_eq!(selection_text(grid, (at(7, 2), at(7, 2))), "日");
        assert_eq!(selection_text(grid, (at(7, 0), at(7, 3))), "e\u{301}日");
        let mut drag = WidgetState::default();
        drag.begin_selection(grid, at(7, 2), Point::new(20.0, 0.0), false);
        drag.extend_selection(at(7, 1));
        let selection = drag.selection.unwrap();
        assert_ne!(
            selection.anchor, selection.head,
            "dragging left over one wide glyph is not a click"
        );
        assert_eq!(selection_text(grid, selection.span(grid)), "日");
    }

    #[test]
    fn agent_clicks_are_deferred_but_drags_never_reach_the_child() {
        let rows = vec![row("agent output"), row("            ")];
        let grid = selection_grid(&rows, 12);
        let position = Point::new(30.0, 10.0);
        let mut clicked = WidgetState::default();
        clicked.begin_selection(grid, at(7, 2), position, true);
        assert!(clicked.selecting);
        assert_eq!(
            clicked.selection_click.take(),
            Some(position),
            "release can forward one intact click"
        );
        let mut dragged = WidgetState::default();
        dragged.begin_selection(grid, at(7, 2), position, true);
        dragged.extend_selection(at(7, 6));
        assert_eq!(
            dragged.selection_click.take(),
            None,
            "a text drag cannot also click the application"
        );
        assert_eq!(
            selection_text(grid, dragged.selection.unwrap().span(grid)),
            "ent o"
        );
        let mut blank = WidgetState::default();
        blank.begin_selection(grid, at(8, 2), position, false);
        assert!(!blank.selecting);
        assert!(blank.selection.is_none());
        blank.begin_selection(grid, at(8, 2), position, true);
        assert!(
            blank.selection.is_none(),
            "agent blank-space clicks do not invent a selection"
        );
        assert_eq!(
            blank.selection_click.take(),
            Some(position),
            "blank-space clicks still reach agent controls"
        );
    }

    #[test]
    fn mouse_routing_preserves_generic_tuis_and_explicit_agent_mouse_input() {
        let plain = KeyModifiers::default();
        assert_eq!(
            selection_routing(false, false, false, plain),
            SelectionRouting::Local
        );
        assert_eq!(
            selection_routing(false, true, false, plain),
            SelectionRouting::Application,
            "ordinary TUIs retain mouse reporting"
        );
        assert_eq!(
            selection_routing(false, true, true, plain),
            SelectionRouting::ClickOrSelect,
            "confirmed idle or active agents allow local drags"
        );
        for agent in [false, true] {
            assert_eq!(
                selection_routing(
                    false,
                    true,
                    agent,
                    KeyModifiers {
                        shift: true,
                        ..plain
                    }
                ),
                SelectionRouting::Local
            );
            assert_eq!(
                selection_routing(false, true, agent, KeyModifiers { alt: true, ..plain }),
                SelectionRouting::Application
            );
            assert_eq!(
                selection_routing(
                    false,
                    true,
                    agent,
                    KeyModifiers {
                        control: true,
                        ..plain
                    }
                ),
                SelectionRouting::Application
            );
        }
        assert_eq!(
            selection_routing(true, true, false, plain),
            SelectionRouting::Local,
            "retained output stays selectable after exit"
        );
    }

    #[test]
    fn a_double_click_takes_the_whole_token_and_a_blank_run_whole_too() {
        let token = "src/app/chat_ui.rs:488";
        let rows = vec![row(&format!("see {token}   next"))];
        let grid = selection_grid(&rows, 40);
        let clicked = |column, mode| Selection::new(at(7, column), mode).span(grid);

        // A path with a line number in it is one thing the user meant to
        // grab, not five tokens separated by punctuation.
        let start = "see ".len() as u16;
        let span = clicked(start + 6, SelectionMode::Word);
        assert_eq!(span, (at(7, start), at(7, start + token.len() as u16 - 1)));
        assert_eq!(selection_text(grid, span), token);

        let blanks = start + token.len() as u16;
        assert_eq!(
            clicked(blanks + 1, SelectionMode::Word),
            (at(7, blanks), at(7, blanks + 2))
        );

        // A triple click takes the row whole, however far into it it landed.
        assert_eq!(clicked(5, SelectionMode::Line), (at(7, 0), at(7, 39)));
    }

    /// One command the widget handed its terminal, in the order it did.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Sent {
        Scroll(ViewportScroll),
        Report(MouseButton),
        Key(TerminalKey),
    }

    /// Replays what `update` does with wheel, drag and frame events so that a
    /// test can count the commands a gesture actually produces. `Widget::update`
    /// itself needs a live renderer, which no unit test can build.
    struct ScrollPump {
        state: WidgetState,
        scroll: ScrollSnapshot,
        scroll_size: isize,
        scroll_speed: f32,
        sent: Vec<Sent>,
    }

    impl ScrollPump {
        fn new(scroll: ScrollSnapshot) -> Self {
            Self {
                state: WidgetState::default(),
                scroll,
                scroll_size: DEFAULT_ROWS_PER_WHEEL_NOTCH,
                scroll_speed: 1.0,
                sent: Vec::new(),
            }
        }

        fn with_size(mut self, scroll_size: isize) -> Self {
            self.scroll_size = scroll_size.max(1);
            self
        }

        fn with_speed(mut self, scroll_speed: f32) -> Self {
            self.scroll_speed = scroll_speed;
            self
        }

        fn shifted(mut self) -> Self {
            self.state.modifiers.shift = true;
            self
        }

        /// One wheel event of `notches` detents.
        fn wheel(&mut self, notches: f32) {
            self.feed(WheelTravel::Notches(notches));
        }

        /// One trackpad event, already divided down to `rows` of the grid.
        fn trackpad(&mut self, rows: f32) {
            self.feed(WheelTravel::Rows(rows));
        }
        fn feed(&mut self, travel: WheelTravel) {
            match self
                .state
                .wheel_effect(travel, self.scroll, self.scroll_size, self.scroll_speed)
            {
                WheelEffect::Idle => {}
                WheelEffect::Viewport(delta) => self.owe(ViewportScroll::Lines(delta)),
                WheelEffect::Report { button, notches } => {
                    self.sent.extend((0..notches).map(|_| Sent::Report(button)))
                }
                WheelEffect::Keys { key, presses } => {
                    self.sent.extend((0..presses).map(|_| Sent::Key(key)));
                }
            }
        }

        fn drag_to(&mut self, row: u64) {
            self.owe(ViewportScroll::Row(row));
        }

        fn owe(&mut self, next: ViewportScroll) {
            if let Some(superseded) = self.state.owe_scroll(next) {
                self.sent.push(Sent::Scroll(superseded));
            }
        }

        fn frame(&mut self) {
            if let Some(owed) = self.state.take_owed_scroll() {
                self.sent.push(Sent::Scroll(owed));
            }
        }
    }

    const SCROLLBACK: ScrollSnapshot = ScrollSnapshot {
        total: 1000,
        offset: 500,
        len: 24,
        alternate_screen: false,
        mouse_tracking: false,
        bracketed_paste: false,
    };

    #[test]
    fn a_frame_of_wheel_events_is_one_viewport_command_but_still_one_report_per_notch() {
        // Eight notches inside one frame move the viewport eight notches'
        // worth of rows in one command; the seven positions in between are
        // never presented, so applying them would buy nothing and cost eight
        // of everything.
        let mut pump = ScrollPump::new(SCROLLBACK);
        for _ in 0..8 {
            pump.wheel(1.0);
        }
        assert!(
            pump.sent.is_empty(),
            "a viewport move may not be sent before its frame"
        );
        pump.frame();
        // A frame with nothing owed must stay silent, or an idle terminal
        // would scroll itself.
        pump.frame();
        assert_eq!(
            pump.sent,
            vec![Sent::Scroll(ViewportScroll::Lines(
                -8 * DEFAULT_ROWS_PER_WHEEL_NOTCH
            ))]
        );

        // One detent moves two rows by default, so scrollback and an
        // alternate-screen application both move at the same pace. A mouse
        // report stays at one because the application counts detents itself
        // and the protocol means exactly one report per detent.
        let mut scrollback = ScrollPump::new(SCROLLBACK);
        scrollback.wheel(1.0);
        scrollback.frame();
        assert_eq!(
            scrollback.sent,
            vec![Sent::Scroll(ViewportScroll::Lines(
                -DEFAULT_ROWS_PER_WHEEL_NOTCH
            ))]
        );
        let mut fast = ScrollPump::new(SCROLLBACK).with_size(4);
        fast.wheel(1.0);
        fast.frame();
        assert_eq!(fast.sent, vec![Sent::Scroll(ViewportScroll::Lines(-4))]);
        let mut accelerated = ScrollPump::new(SCROLLBACK).with_size(1).with_speed(2.0);
        accelerated.wheel(1.0);
        accelerated.frame();
        assert_eq!(
            accelerated.sent,
            vec![Sent::Scroll(ViewportScroll::Lines(-2))]
        );

        let mut full_screen = ScrollPump::new(ScrollSnapshot {
            alternate_screen: true,
            ..SCROLLBACK
        });
        full_screen.wheel(1.0);
        full_screen.frame();
        assert_eq!(
            full_screen.sent,
            vec![Sent::Key(TerminalKey::ArrowUp); DEFAULT_ROWS_PER_WHEEL_NOTCH as usize]
        );

        let mut tracked = ScrollPump::new(ScrollSnapshot {
            mouse_tracking: true,
            bracketed_paste: false,
            ..SCROLLBACK
        });
        tracked.wheel(1.0);
        tracked.frame();
        assert_eq!(tracked.sent, vec![Sent::Report(MouseButton::WheelUp)]);

        // Reports are counted in detents and sent one by one: three downward
        // detents are three downward reports, in gesture order, none of it
        // deferred to a frame.
        let mut reporting = ScrollPump::new(ScrollSnapshot {
            mouse_tracking: true,
            bracketed_paste: false,
            ..SCROLLBACK
        });
        for _ in 0..3 {
            reporting.wheel(-1.0);
        }
        reporting.frame();
        assert_eq!(
            reporting.sent,
            vec![Sent::Report(MouseButton::WheelDown); 3]
        );

        // Shift claims the gesture for the viewport even where the child is
        // reading the wheel, and then it coalesces like any other.
        let mut shifted = ScrollPump::new(ScrollSnapshot {
            mouse_tracking: true,
            bracketed_paste: false,
            ..SCROLLBACK
        })
        .shifted();
        for _ in 0..4 {
            shifted.wheel(1.0);
        }
        shifted.frame();
        assert_eq!(
            shifted.sent,
            vec![Sent::Scroll(ViewportScroll::Lines(
                -4 * DEFAULT_ROWS_PER_WHEEL_NOTCH
            ))]
        );
    }

    #[test]
    fn scroll_speed_scales_rows_before_rounding_and_never_changes_mouse_reports() {
        let mut fractional = ScrollPump::new(SCROLLBACK).with_size(4).with_speed(0.25);
        fractional.wheel(1.0);
        fractional.frame();
        assert_eq!(
            fractional.sent,
            vec![Sent::Scroll(ViewportScroll::Lines(-1))]
        );
        for speed in [0.25, 4.0] {
            let mut reporting = ScrollPump::new(ScrollSnapshot {
                mouse_tracking: true,
                bracketed_paste: false,
                ..SCROLLBACK
            })
            .with_speed(speed);
            reporting.trackpad(1.0);
            reporting.wheel(1.0);
            reporting.frame();
            assert_eq!(reporting.sent, vec![Sent::Report(MouseButton::WheelUp); 2]);
        }
    }

    #[test]
    fn a_trackpad_keeps_its_fraction_and_an_absolute_target_orders_against_deltas() {
        // Sub-row trackpad deltas accumulate instead of rounding away, so a
        // high-resolution trackpad keeps moving, and only ever by whole rows.
        // The notch size is not applied here: a finger reports the distance it
        // actually travelled and the content has to stay under it.
        let mut pump = ScrollPump::new(SCROLLBACK);
        for _ in 0..5 {
            pump.trackpad(0.5);
        }
        pump.frame();
        assert_eq!(pump.sent, vec![Sent::Scroll(ViewportScroll::Lines(-2))]);
        // The half row left over is carried across the frame boundary rather
        // than discarded, so one more tick of the same size completes a row.
        pump.trackpad(0.5);
        pump.frame();
        assert_eq!(pump.sent[1], Sent::Scroll(ViewportScroll::Lines(-1)));

        // A scrollbar drag names an absolute row, which discards the deltas it
        // supersedes.
        let mut drag = ScrollPump::new(SCROLLBACK);
        drag.wheel(3.0);
        drag.drag_to(120);
        drag.frame();
        assert_eq!(drag.sent, vec![Sent::Scroll(ViewportScroll::Row(120))]);

        // A delta after an absolute row cannot be folded into it, because only
        // the terminal knows which row that one clamped to, so the two are
        // sent in gesture order.
        let mut mixed = ScrollPump::new(SCROLLBACK);
        mixed.drag_to(120);
        mixed.wheel(3.0);
        mixed.frame();
        assert_eq!(
            mixed.sent,
            vec![
                Sent::Scroll(ViewportScroll::Row(120)),
                Sent::Scroll(ViewportScroll::Lines(-3 * DEFAULT_ROWS_PER_WHEEL_NOTCH)),
            ]
        );
    }
}
