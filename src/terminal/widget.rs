use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    marker::PhantomData,
    ops::Range,
    sync::{Arc, LazyLock, Mutex},
};

use cosmic::iced::advanced::Renderer as _;
use cosmic::iced::advanced::graphics::text::{self as graphics_text, cosmic_text, font_system};
use cosmic::iced::advanced::text::Paragraph as _;
use cosmic::{
    Element, Renderer, Theme,
    iced::{
        Border, Color, Event, Font, Length, Pixels, Point, Rectangle, Shadow, Size,
        advanced::{
            Clipboard, Layout, Shell, Widget, layout, mouse, renderer, text,
            widget::{Tree, tree},
        },
        keyboard,
        mouse::{Button as IcedMouseButton, ScrollDelta},
    },
};

use super::{
    CellSnapshot, CursorShape, KeyAction, KeyInput, KeyModifiers, MouseAction, MouseButton,
    MouseInput, RenderSnapshot, Rgb, RowSnapshot, TerminalKey, TerminalSendError, TerminalSize,
    TerminalSurface, Underline,
};

use crate::settings::TerminalCursorStyle;

const DEFAULT_PADDING: f32 = 8.0;

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
        let content_width = (bounds.width - padding * 2.0).max(0.0);
        let content_height = (bounds.height - padding * 2.0).max(0.0);
        let columns = ((content_width * scale_factor / cell_width_px as f32).floor() as u16).max(1);
        let rows = ((content_height * scale_factor / cell_height_px as f32).floor() as u16).max(1);

        // Absolute snapping still gives drawing and input a pixel-aligned
        // origin and a conservative clip. The fixed-size grid may lose a
        // fractional edge pixel to this clip, but its PTY extent stays fixed.
        let left = ((bounds.x + padding) * scale_factor).ceil() / scale_factor;
        let top = ((bounds.y + padding) * scale_factor).ceil() / scale_factor;
        let right = ((bounds.x + bounds.width - padding) * scale_factor).floor() / scale_factor;
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

/// An overlay: its generous hit target never subtracts space from the grid.
#[derive(Clone, Copy, Debug)]
struct TerminalScrollbar {
    track: Rectangle,
    thumb: Rectangle,
    max_offset: u64,
}

impl TerminalScrollbar {
    fn new(bounds: Rectangle, scroll: super::ScrollSnapshot, focused: bool) -> Option<Self> {
        if !focused || !scroll.scrollable() || bounds.height <= 0.0 || bounds.width <= 0.0 {
            return None;
        }
        let track = Rectangle {
            x: bounds.x + (bounds.width - 12.0).max(0.0),
            width: bounds.width.min(12.0),
            ..bounds
        };
        let height = (track.height * (scroll.len as f64 / scroll.total as f64) as f32)
            .max(24.0)
            .min(track.height);
        let max_offset = scroll.max_offset();
        let thumb = Rectangle {
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
}

#[derive(Default)]
struct WidgetState {
    last_size: Option<TerminalSize>,
    focused: bool,
    focus_requested: bool,
    pressed_buttons: u8,
    modifiers: KeyModifiers,
    last_cursor: Option<Point>,
    scrollbar_drag: Option<f32>,
    wheel_remainder: f32,
    /// Shaped text, reused across frames. Mutated through `&self` because
    /// `draw` is the only place that knows which rows are actually visible.
    shaped: RefCell<ShapeCache>,
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
/// The daemon reuses the `Arc` of every row a repaint leaves unchanged, so
/// pointer identity is an exact content key: holding the `Arc` keeps the
/// address from being recycled under a stale entry.
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
    rows: Vec<ShapedRow>,
    /// Scratch reused while shaping, so a cache miss allocates nothing extra.
    content: String,
    spans: Vec<(Range<usize>, cosmic_text::Attrs<'static>)>,
}

impl ShapeCache {
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

        let stale = rows.iter().zip(self.rows.iter()).any(|(row, cached)| {
            cached
                .source
                .as_ref()
                .is_none_or(|source| !Arc::ptr_eq(source, row))
        });
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

/// A lock-free iced view over a single immutable terminal snapshot.
///
/// Construct this in the application's `view` function. It loads ArcSwap once,
/// holds only owned data while drawing, and sends bounded commands back to the
/// worker without ever borrowing or locking Ghostty state.
pub struct TerminalWidget<Message> {
    handle: TerminalSurface,
    snapshot: Arc<RenderSnapshot>,
    font_size: f32,
    font_family: &'static str,
    line_height: f32,
    padding: f32,
    cursor_style: TerminalCursorStyle,
    scale_factor: f32,
    model_focused: bool,
    on_focus: Option<Message>,
    on_command_error: Option<Arc<ErrorMapper<Message>>>,
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
            handle,
            snapshot,
            font_size: DEFAULT_FONT_SIZE,
            font_family: "",
            line_height: LINE_HEIGHT_RATIO,
            padding: DEFAULT_PADDING,
            cursor_style: TerminalCursorStyle::default(),
            scale_factor: 1.0,
            model_focused: false,
            on_focus: None,
            on_command_error: None,
            _message: PhantomData,
        }
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

    fn synchronize_focus(&self, state: &mut WidgetState, shell: &mut Shell<'_, Message>) {
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
        let size = geometry.terminal_size();
        if state.last_size != Some(size) {
            match self.handle.resize(size) {
                Ok(()) => state.last_size = Some(size),
                Err(error) => self.publish_error(error, shell),
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

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
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
        self.synchronize_size(state, geometry, shell);
        self.synchronize_focus(state, shell);
        let scrollbar = TerminalScrollbar::new(bounds, self.snapshot.scroll, self.model_focused);
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
                    Some(bar.row_at(position.y, state.scrollbar_drag.unwrap()))
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
                if let Err(error) = self.handle.scroll_to_row(row) {
                    self.publish_error(error, shell);
                }
                shell.capture_event();
                return;
            }
        }

        match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modifiers,
                text,
                repeat,
                ..
            }) if self.model_focused => {
                state.modifiers = map_iced_modifiers(*modifiers);
                let input = KeyInput {
                    action: if *repeat {
                        KeyAction::Repeat
                    } else {
                        KeyAction::Press
                    },
                    key: map_iced_key(key),
                    modifiers: map_iced_modifiers(*modifiers),
                    text: text.as_ref().map(ToString::to_string),
                    unshifted_codepoint: character_from_key(key),
                };
                match self.handle.key(input) {
                    Ok(()) => shell.capture_event(),
                    Err(error) => self.publish_error(error, shell),
                }
            }
            Event::Keyboard(keyboard::Event::KeyReleased { key, modifiers, .. })
                if self.model_focused =>
            {
                state.modifiers = map_iced_modifiers(*modifiers);
                let input = KeyInput {
                    action: KeyAction::Release,
                    key: map_iced_key(key),
                    modifiers: map_iced_modifiers(*modifiers),
                    text: None,
                    unshifted_codepoint: character_from_key(key),
                };
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
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if let Some(position) = geometry.cursor_position(cursor) {
                    state.last_cursor = Some(position);
                    self.send_mouse(state, geometry, position, MouseAction::Motion, None, shell);
                }
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if cursor.is_over(bounds) => {
                let lines = match delta {
                    ScrollDelta::Lines { y, .. } => *y,
                    ScrollDelta::Pixels { y, .. } => *y / geometry.cell_height,
                };
                state.wheel_remainder -= lines;
                let delta = (state.wheel_remainder.trunc() as isize).clamp(-128, 128);
                state.wheel_remainder -= delta as f32;
                if delta != 0 {
                    if self.snapshot.scroll.mouse_tracking && !state.modifiers.shift {
                        if let Some(position) = geometry.cursor_position(cursor) {
                            for _ in 0..delta.unsigned_abs() {
                                self.send_mouse(
                                    state,
                                    geometry,
                                    position,
                                    MouseAction::Press,
                                    Some(if delta < 0 {
                                        MouseButton::WheelUp
                                    } else {
                                        MouseButton::WheelDown
                                    }),
                                    shell,
                                );
                            }
                        }
                    } else if self.snapshot.scroll.alternate_screen && !state.modifiers.shift {
                        for _ in 0..delta.unsigned_abs() {
                            let input = KeyInput {
                                action: KeyAction::Press,
                                key: if delta < 0 {
                                    TerminalKey::ArrowUp
                                } else {
                                    TerminalKey::ArrowDown
                                },
                                modifiers: state.modifiers,
                                text: None,
                                unshifted_codepoint: None,
                            };
                            if let Err(error) = self.handle.key(input) {
                                self.publish_error(error, shell);
                                break;
                            }
                        }
                    } else if let Err(error) = self.handle.scroll_lines(delta) {
                        self.publish_error(error, shell);
                    }
                }
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
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

                // Glyphs. One shaped buffer per segment, reused for as long as
                // the daemon leaves the row alone, so a still terminal reshapes
                // nothing and a scrolling one reshapes only the rows that moved.
                let state = tree.state.downcast_ref::<WidgetState>();
                let mut shaped = state.shaped.borrow_mut();
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

                if let Some((cursor, cell_bounds)) = self
                    .snapshot
                    .cursor
                    .filter(|cursor| cursor.visible)
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
                    let border = if shape == CursorShape::HollowBlock {
                        Border {
                            color: color(cursor.color),
                            width: 1.0 / geometry.scale_factor,
                            radius: 0.0.into(),
                        }
                    } else {
                        Border::default()
                    };
                    let fill = if shape == CursorShape::HollowBlock {
                        Color::TRANSPARENT
                    } else {
                        color(cursor.color).scale_alpha(0.65)
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
            if let Some(bar) =
                TerminalScrollbar::new(bounds, self.snapshot.scroll, self.model_focused)
            {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: bar.track.x + (bar.track.width - 4.0).max(0.0),
                            width: bar.track.width.min(3.0),
                            ..bar.thumb
                        },
                        border: Border {
                            radius: 2.0.into(),
                            ..Border::default()
                        },
                        shadow: Shadow::default(),
                        snap: true,
                    },
                    color(self.snapshot.foreground).scale_alpha(0.45),
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
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if TerminalScrollbar::new(layout.bounds(), self.snapshot.scroll, self.model_focused)
            .is_some_and(|bar| cursor.is_over(bar.track))
        {
            return mouse::Interaction::Pointer;
        }
        if cursor.is_over(layout.bounds()) {
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
        )
    }) {
        let (foreground, underline, strikethrough, overline) = run.key;
        if underline == Underline::None && !strikethrough && !overline {
            continue;
        }
        let Some(bounds) = geometry.run_bounds(run.start, run.len, row) else {
            continue;
        };
        draw_decorations(
            renderer,
            bounds,
            foreground,
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
            .color(cosmic_text::Color::rgb(
                foreground.red,
                foreground.green,
                foreground.blue,
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
    foreground: Rgb,
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
            color(foreground),
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

fn character_from_key(key: &keyboard::Key) -> Option<char> {
    let keyboard::Key::Character(text) = key else {
        return None;
    };
    let mut characters = text.chars();
    let character = characters.next()?;
    characters.next().is_none().then_some(character)
}

fn map_iced_key(key: &keyboard::Key) -> TerminalKey {
    match key.as_ref() {
        keyboard::Key::Character(text) => text
            .chars()
            .next()
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

#[cfg(test)]
mod tests {
    use crate::terminal::CellText;

    use super::*;

    #[test]
    fn scrollbar_requires_focused_history_and_drag_clamps_to_real_offsets() {
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
        };
        assert!(TerminalScrollbar::new(bounds, scroll, false).is_none());
        assert!(
            TerminalScrollbar::new(
                bounds,
                super::super::ScrollSnapshot {
                    alternate_screen: true,
                    ..scroll
                },
                true
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
                true
            )
            .is_none()
        );
        let bar = TerminalScrollbar::new(bounds, scroll, true).unwrap();
        assert_eq!(bar.thumb.height, 60.0);
        assert_eq!(bar.row_at(bar.thumb.y + 10.0, 10.0), 40);
        assert_eq!(bar.row_at(-500.0, 10.0), 0);
        assert_eq!(bar.row_at(900.0, 10.0), 80);
        assert_eq!(bar.track.x + bar.track.width, bounds.x + bounds.width);
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

        assert_eq!(baseline.columns, 54);
        assert_eq!(baseline.rows, 20);
        assert_eq!(baseline.cell_width_px, 11);
        assert_eq!(baseline.cell_height_px, 23);
        assert_eq!(baseline.screen_width_px(), 594);
        assert_eq!(baseline.screen_height_px(), 460);

        assert!(
            geometry_at(0.2).content_bounds.width < baseline.content_bounds.width,
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
            ((bounds.width - 32.0) * 1.25 / size.cell_width_px as f32).floor() as u16
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

    fn row(text: &str) -> Arc<RowSnapshot> {
        Arc::new(RowSnapshot {
            cells: cells(text, |_, cell| cell),
        })
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
    fn a_shaped_row_survives_exactly_as_long_as_its_snapshot_pointer() {
        // The cache keys a row's buffers on the `Arc` the daemon handed out and
        // holds it, so the address cannot be recycled under a stale entry.
        // Comparing anything weaker - a length, or the cells themselves - would
        // leave a live terminal drawing glyphs of text it no longer holds.
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

        // Equal content in a fresh allocation still reshapes: the daemon reuses
        // the `Arc` of every row a repaint leaves alone, so a new pointer means
        // the row was rewritten and pointer identity is the whole key.
        rows[1] = row("second row");
        cache.refresh(&geometry, DEFAULT_FONT_SIZE, &rows, 80);
        let replaced = shaped_buffers(&cache);
        assert!(
            Arc::ptr_eq(&baseline[0], &replaced[0]),
            "a row nobody touched must not reshape"
        );
        assert!(
            !Arc::ptr_eq(&baseline[1], &replaced[1]),
            "a replaced row must reshape even when its content is equal"
        );

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
}
