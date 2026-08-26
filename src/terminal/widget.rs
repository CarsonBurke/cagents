use std::{
    collections::HashMap,
    marker::PhantomData,
    sync::{Arc, LazyLock, Mutex},
};

use cosmic::iced::advanced::Renderer as _;
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
    CellSnapshot, CursorShape, KeyAction, KeyInput, KeyModifiers, MouseAction,
    MouseButton, MouseInput, RenderSnapshot, Rgb, TerminalKey, TerminalSendError, TerminalSize,
    TerminalSurface, Underline,
};

const CONTENT_PADDING_X: f32 = 4.0;
const CONTENT_PADDING_Y: f32 = 4.0;

/// Text metrics for one terminal grid at one scale factor.
///
/// The grid pitch is snapped to whole physical pixels so that backgrounds, the
/// cursor and mouse hit-testing all land on device pixels. A shaped text run,
/// however, advances by the font's own fractional advance, so a run drawn as a
/// single primitive drifts off its columns unless the two agree. `faces`
/// reconciles them per face: it carries the font size at which that face's
/// advance is closest to the pitch, and the longest run for which the residual
/// difference still cannot reach half a physical pixel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
    faces: [FaceMetrics; FACE_COUNT],
}


/// The two style bits that select a font face, and therefore the only ones a
/// shaped text run may not span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Face {
    bold: bool,
    italic: bool,
}

impl Face {
    const fn index(self) -> usize {
        (self.bold as usize) | ((self.italic as usize) << 1)
    }
}

/// How one face must be drawn to stay on the grid.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FaceMetrics {
    /// Size at which this face's advance is closest to the grid pitch.
    font_size: f32,
    /// Longest run this face may cover with one shaped primitive.
    max_run: u16,
}


const FACE_COUNT: usize = 4;

/// Cell height as a multiple of the font size. Terminals pick a fixed ratio
/// rather than the font's own line height so that a font fallback for one row
/// cannot change the grid pitch. 1.286 keeps the historical 18px box at 14pt.
const LINE_HEIGHT_RATIO: f32 = 1.286;

/// The drift a coalesced run is allowed to accumulate, in physical pixels.
/// Half a pixel is the point at which a glyph would snap to the wrong column.
const MAX_RUN_DRIFT_PX: f32 = 0.5;

pub const DEFAULT_FONT_SIZE: f32 = 14.0;

/// The `ADVANCE_SAMPLE` string, repeated so that the per-glyph advance is the
/// measured width divided by the sample length. That averages away the
/// sub-pixel rounding a single glyph's bounds would carry.
const ADVANCE_SAMPLE: &str = "MMMMMMMMMMMMMMMM";

impl CellMetrics {
    /// Returns the metrics for `font_size` at `scale_factor`, measuring at most
    /// once per distinct pair for the lifetime of the process.
    ///
    /// Measuring is four shaping calls and the installed faces cannot change at
    /// runtime, so the result is a pure function of its inputs. Memoising it
    /// lets `layout` and `draw` run per frame without paying for text layout.
    pub fn resolve(font_size: f32, scale_factor: f32) -> Self {
        static MEASURED: LazyLock<Mutex<HashMap<(u32, u32), CellMetrics>>> =
            LazyLock::new(|| Mutex::new(HashMap::new()));

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
        let key = (font_size.to_bits(), scale_factor.to_bits());
        let mut measured = MEASURED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *measured
            .entry(key)
            .or_insert_with(|| Self::measure(font_size, scale_factor))
    }

    fn measure(font_size: f32, scale_factor: f32) -> Self {
        let plain = Face {
            bold: false,
            italic: false,
        };
        let base = advance(font_size, plain).unwrap_or((font_size * 0.6).max(1.0));
        let width = (base * scale_factor).round().max(1.0) / scale_factor;
        let height = (font_size * LINE_HEIGHT_RATIO * scale_factor)
            .round()
            .max(1.0)
            / scale_factor;

        let faces = std::array::from_fn(|index| {
            let key = Face {
                bold: index & 1 != 0,
                italic: index & 2 != 0,
            };
            // Scaling the size by pitch/advance lands the face on the pitch
            // when its advance is linear in the size, which is the normal case.
            // The re-measurement below is what makes the result correct when it
            // is not, rather than assuming it: a face that refuses to land on
            // the pitch gets a short `max_run` and is drawn in more pieces.
            let Some(face_base) = advance(font_size, key) else {
                return FaceMetrics {
                    font_size,
                    max_run: 1,
                };
            };
            let adjusted = (font_size * width / face_base).clamp(1.0, 400.0);
            let actual = advance(adjusted, key).unwrap_or(face_base);
            FaceMetrics {
                font_size: adjusted,
                max_run: run_ceiling((actual - width).abs() * scale_factor),
            }
        });

        Self {
            width,
            height,
            faces,
        }
    }

    fn face(&self, key: Face) -> FaceMetrics {
        self.faces[key.index()]
    }
}

impl Default for CellMetrics {
    fn default() -> Self {
        Self::resolve(DEFAULT_FONT_SIZE, 1.0)
    }
}

/// The longest run whose accumulated drift stays under half a physical pixel.
///
/// The first cell of a run is anchored exactly, so `n` cells accumulate
/// `(n - 1) * drift`.
fn run_ceiling(drift_px: f32) -> u16 {
    if !drift_px.is_finite() {
        return 1;
    }
    if drift_px <= f32::EPSILON {
        return u16::MAX;
    }
    let cells = (MAX_RUN_DRIFT_PX / drift_px).floor();
    if cells >= f32::from(u16::MAX) {
        u16::MAX
    } else {
        (cells as u16).saturating_add(1).max(1)
    }
}

/// The shaped horizontal advance of one monospace cell, or `None` when no face
/// resolved and the measurement cannot be trusted.
fn advance(font_size: f32, key: Face) -> Option<f32> {
    let paragraph = <Renderer as text::Renderer>::Paragraph::with_text(text::Text {
        content: ADVANCE_SAMPLE,
        bounds: Size::new(f32::INFINITY, f32::INFINITY),
        size: Pixels(font_size),
        line_height: text::LineHeight::Relative(1.0),
        font: font(key),
        align_x: text::Alignment::Left,
        align_y: cosmic::iced::alignment::Vertical::Top,
        shaping: text::Shaping::Basic,
        wrapping: text::Wrapping::None,
        ellipsize: text::Ellipsize::None,
    });
    let measured = paragraph.min_bounds().width / ADVANCE_SAMPLE.len() as f32;
    (measured.is_finite() && measured >= 1.0).then_some(measured)
}

fn font(key: Face) -> Font {
    Font {
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
    fn new(bounds: Rectangle, font_size: f32, scale_factor: f32) -> Self {
        let scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        // `resolve` already snapped the pitch to whole physical pixels at this
        // scale, so these conversions are exact rather than a second rounding.
        let metrics = CellMetrics::resolve(font_size, scale_factor);
        let cell_width_px = (metrics.width * scale_factor).round().max(1.0) as u32;
        let cell_height_px = (metrics.height * scale_factor).round().max(1.0) as u32;
        let cell_width = cell_width_px as f32 / scale_factor;
        let cell_height = cell_height_px as f32 / scale_factor;

        // Size the PTY from the card's stable logical extent. Snapping both
        // absolute edges would make the available physical width depend on
        // the card's fractional x/y position and reflow while panning.
        let content_width = (bounds.width - CONTENT_PADDING_X * 2.0).max(0.0);
        let content_height = (bounds.height - CONTENT_PADDING_Y * 2.0).max(0.0);
        let columns = ((content_width * scale_factor / cell_width_px as f32).floor() as u16).max(1);
        let rows = ((content_height * scale_factor / cell_height_px as f32).floor() as u16).max(1);

        // Absolute snapping still gives drawing and input a pixel-aligned
        // origin and a conservative clip. The fixed-size grid may lose a
        // fractional edge pixel to this clip, but its PTY extent stays fixed.
        let left = ((bounds.x + CONTENT_PADDING_X) * scale_factor).ceil() / scale_factor;
        let top = ((bounds.y + CONTENT_PADDING_Y) * scale_factor).ceil() / scale_factor;
        let right =
            ((bounds.x + bounds.width - CONTENT_PADDING_X) * scale_factor).floor() / scale_factor;
        let bottom =
            ((bounds.y + bounds.height - CONTENT_PADDING_Y) * scale_factor).floor() / scale_factor;
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

    fn text_origin(self, cell_bounds: Rectangle) -> Point {
        // `Vertical::Center` interprets the draw position as the vertical
        // anchor, not the top-left corner. Anchoring at the cell top lets
        // glyph ascenders escape above row zero and be clipped.
        Point::new(cell_bounds.x, cell_bounds.y + cell_bounds.height / 2.0)
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
            scale_factor: 1.0,
            model_focused: false,
            on_focus: None,
            on_command_error: None,
            _message: PhantomData,
        }
    }

    /// Sets the terminal font size.
    ///
    /// This is the only knob: the cell box is measured from the renderer's real
    /// monospace advance at this size, so the grid, the PTY's reported pixel
    /// size, and the drawn glyphs cannot disagree.
    pub fn font_size(mut self, font_size: f32) -> Self {
        self.font_size = if font_size.is_finite() {
            font_size.clamp(1.0, 400.0)
        } else {
            DEFAULT_FONT_SIZE
        };
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
        let geometry = TerminalGeometry::new(bounds, self.font_size, self.scale_factor);
        let state = tree.state.downcast_mut::<WidgetState>();
        self.synchronize_size(state, geometry, shell);
        self.synchronize_focus(state, shell);

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
                let delta = -(lines.round() as isize);
                if delta != 0 {
                    match self.handle.scroll_lines(delta) {
                        Ok(()) => shell.capture_event(),
                        Err(error) => self.publish_error(error, shell),
                    }
                }
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
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
        let geometry = TerminalGeometry::new(bounds, self.font_size, self.scale_factor);
        let Some(widget_clip) = bounds.intersection(viewport) else {
            self.handle.acknowledge_redraw(self.snapshot.generation);
            return;
        };
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
                let mut run_content = String::new();
                for (row_index, row) in self.snapshot.lines.iter().take(row_limit).enumerate() {
                    let cells = &row.cells[..row.cells.len().min(column_limit)];
                    draw_row(
                        renderer,
                        &geometry,
                        row_index as u16,
                        cells,
                        self.snapshot.background,
                        content_clip,
                        &mut run_content,
                    );
                }

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
                    let stroke = (2.0 / geometry.scale_factor)
                        .max(1.0 / geometry.scale_factor)
                        .min(cell_bounds.width)
                        .min(cell_bounds.height);
                    let cursor_bounds = match cursor.shape {
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
                    let border = if cursor.shape == CursorShape::HollowBlock {
                        Border {
                            color: color(cursor.color),
                            width: 1.0 / geometry.scale_factor,
                            radius: 0.0.into(),
                        }
                    } else {
                        Border::default()
                    };
                    let fill = if cursor.shape == CursorShape::HollowBlock {
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
        });

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

/// Draws one grid row using as few renderer primitives as the row's styling
/// allows.
///
/// A cell-at-a-time loop cost one background quad plus one shaped text
/// primitive plus up to three decoration quads *per column*: for a 200-column
/// grid that is tens of thousands of primitives and ten thousand string
/// allocations every frame. Terminal rows are overwhelmingly uniform, so each
/// of the three passes below emits one primitive per maximal run of cells that
/// share the attribute it draws.
///
/// Text runs carry the extra requirement that every cell advance by exactly one
/// cell width, because only the run's first cell is positioned explicitly.
/// Two things enforce that: the run must be single-width ASCII, since a wide or
/// multi-scalar cluster is shaped by whatever fallback face covers it and its
/// advance is not ours to predict; and the run is capped at the face's
/// `max_run`, beyond which the residual difference between the shaped advance
/// and the pixel-snapped grid pitch could reach half a physical pixel. Cells
/// that fail the first requirement are drawn individually, which is exactly the
/// per-cell behaviour that preceded coalescing.
#[allow(clippy::too_many_arguments)]
fn draw_row(
    renderer: &mut Renderer,
    geometry: &TerminalGeometry,
    row: u16,
    cells: &[CellSnapshot],
    default_background: Rgb,
    clip: Rectangle,
    content: &mut String,
) {
    // Pass 1: backgrounds. Cells matching the terminal default are already
    // covered by the widget-wide fill, so only runs that differ are drawn.
    // Quads are positioned individually, so these runs need no length cap.
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

    // Pass 2: glyphs. Keyed on only what the shaped text actually depends on —
    // colour and face — so an underline or blink change does not split a run
    // that would render identically. `None` marks a cell that must not join a
    // run at all: empty, invisible, or not single-width ASCII.
    for run in runs(cells, |cell| {
        (!cell.text.is_empty() && !cell.style.invisible && cell.text.is_narrow_ascii()).then(|| {
            (
                resolved(cell).0,
                Face {
                    bold: cell.style.bold,
                    italic: cell.style.italic,
                },
            )
        })
    }) {
        let Some((foreground, face_key)) = run.key else {
            // Not coalescable: draw each cell on its own so that the font's own
            // advance, not our cell pitch, positions the cluster.
            for offset in 0..run.len {
                let cell = &cells[usize::from(run.start + offset)];
                if cell.text.is_empty() || cell.style.invisible {
                    continue;
                }
                let Some(bounds) = geometry.run_bounds(run.start + offset, 1, row) else {
                    continue;
                };
                content.clear();
                content.push_str(cell.text.as_str());
                let (foreground, _) = resolved(cell);
                let face_key = Face {
                    bold: cell.style.bold,
                    italic: cell.style.italic,
                };
                fill_run(
                    renderer,
                    geometry,
                    bounds,
                    content.clone(),
                    geometry.metrics.face(face_key).font_size,
                    face_key,
                    foreground,
                    // A wide or multi-scalar cluster needs script detection and
                    // font fallback; `Basic` would drop it to a tofu box.
                    text::Shaping::Advanced,
                    clip,
                );
            }
            continue;
        };
        let face = geometry.metrics.face(face_key);
        for chunk in run.chunks(face.max_run) {
            let Some(bounds) = geometry.run_bounds(chunk.start, chunk.len, row) else {
                continue;
            };
            content.clear();
            for offset in 0..chunk.len {
                content.push_str(cells[usize::from(chunk.start + offset)].text.as_str());
            }
            fill_run(
                renderer,
                geometry,
                bounds,
                content.clone(),
                face.font_size,
                face_key,
                foreground,
                // The run is single-width ASCII by construction, so skipping
                // script detection and fallback selection is safe and avoids
                // redoing both for every run on every frame.
                text::Shaping::Basic,
                clip,
            );
        }
    }

    // Pass 3: decorations. Keyed on colour plus only the line-drawing bits, so
    // a bold/italic change inside an underlined span does not split the rule.
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

/// A span of columns, without the key that produced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    start: u16,
    len: u16,
}

impl<Key> Run<Key> {
    /// Splits this run into spans of at most `limit` cells.
    ///
    /// A single shaped primitive may only cover as many cells as the face's
    /// advance stays on the grid for, so a long uniform run becomes a few
    /// primitives rather than one that drifts.
    fn chunks(&self, limit: u16) -> impl Iterator<Item = Span> {
        let limit = limit.max(1);
        let (start, len) = (self.start, self.len);
        let mut drawn = 0u16;
        std::iter::from_fn(move || {
            if drawn >= len {
                return None;
            }
            let span = Span {
                start: start + drawn,
                len: limit.min(len - drawn),
            };
            drawn += span.len;
            Some(span)
        })
    }
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

#[allow(clippy::too_many_arguments)]
fn fill_run(
    renderer: &mut Renderer,
    geometry: &TerminalGeometry,
    bounds: Rectangle,
    content: String,
    font_size: f32,
    key: Face,
    foreground: Rgb,
    shaping: text::Shaping,
    clip: Rectangle,
) {
    text::Renderer::fill_text(
        renderer,
        text::Text {
            content,
            bounds: bounds.size(),
            size: Pixels(font_size),
            // Centering one absolute line box gives every glyph the same
            // baseline while retaining its font-provided left and right
            // bearings.
            line_height: text::LineHeight::Absolute(Pixels(geometry.cell_height)),
            font: font(key),
            align_x: text::Alignment::Left,
            align_y: cosmic::iced::alignment::Vertical::Center,
            shaping,
            wrapping: text::Wrapping::None,
            ellipsize: text::Ellipsize::None,
        },
        geometry.text_origin(bounds),
        color(foreground),
        clip,
    );
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
    fn fractional_scale_grid_and_resize_share_one_geometry() {
        let bounds = Rectangle {
            x: 20.0,
            y: 44.0,
            width: 311.3,
            height: 193.7,
        };
        let geometry = TerminalGeometry::new(bounds, DEFAULT_FONT_SIZE, 1.25);
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
            assert_eq!(
                panned.grid_bounds.x,
                ((x + CONTENT_PADDING_X) * 1.25).ceil() / 1.25
            );
        }
    }

    #[test]
    fn last_cell_and_clip_stay_inside_terminal_body_and_column() {
        let bounds = Rectangle {
            x: 400.0,
            y: 44.0,
            width: 317.3,
            height: 401.2,
        };
        let geometry = TerminalGeometry::new(bounds, DEFAULT_FONT_SIZE, 1.25);
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
    fn centered_text_anchor_stays_inside_first_grid_row() {
        let geometry = TerminalGeometry::new(
            Rectangle {
                x: 20.0,
                y: 44.0,
                width: 300.0,
                height: 200.0,
            },
            DEFAULT_FONT_SIZE,
            1.25,
        );
        let first_cell = geometry.cell_bounds(0, 0).unwrap();
        let origin = geometry.text_origin(first_cell);

        assert_eq!(origin.x, first_cell.x);
        assert_eq!(origin.y, first_cell.y + first_cell.height / 2.0);
        assert!(origin.y > geometry.content_bounds.y);
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
    fn a_wide_or_invisible_cell_is_excluded_from_text_runs() {
        // Only single-width ASCII may share a shaped run, because the cell
        // pitch is the only advance the run has.
        let row = cells("ab\u{1f600}cd", |index, mut cell| {
            cell.style.invisible = index == 0;
            cell
        });
        let text = runs(&row, |cell| {
            (!cell.text.is_empty() && !cell.style.invisible && cell.text.is_narrow_ascii())
                .then(|| (cell.foreground, cell.style))
        })
        .collect::<Vec<_>>();

        let coalescable = text
            .iter()
            .filter(|run| run.key.is_some())
            .map(|run| (run.start, run.len))
            .collect::<Vec<_>>();
        assert_eq!(
            coalescable,
            vec![(1, 1), (3, 2)],
            "the invisible cell and the emoji must each break the run"
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

    fn cells(
        text: &str,
        style: impl Fn(usize, CellSnapshot) -> CellSnapshot,
    ) -> Vec<CellSnapshot> {
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
    fn a_coalesced_run_never_drifts_off_its_columns() {
        // The invariant the whole run-coalescing design rests on: drawing N
        // cells as one shaped primitive must land the Nth glyph within half a
        // physical pixel of the Nth column. Checked against the real text
        // stack, at every face, for the scale factors a fractional display
        // actually reports.
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let metrics = CellMetrics::resolve(DEFAULT_FONT_SIZE, scale);
            assert!(
                (metrics.width * scale - (metrics.width * scale).round()).abs() <= 1e-3,
                "the grid pitch must be a whole number of physical pixels"
            );
            for bold in [false, true] {
                for italic in [false, true] {
                    let key = Face { bold, italic };
                    let face = metrics.face(key);
                    let shaped = advance(face.font_size, key)
                        .expect("a monospace face must resolve in the test environment");
                    let cells = u32::from(face.max_run);
                    let drift =
                        (shaped - metrics.width).abs() * scale * cells.saturating_sub(1) as f32;
                    assert!(
                        drift <= MAX_RUN_DRIFT_PX + 1e-3,
                        "scale {scale} bold {bold} italic {italic}: {cells} cells drift {drift}px"
                    );
                }
            }
        }
    }

    #[test]
    fn matching_the_pitch_buys_runs_long_enough_to_be_worth_coalescing() {
        // If the font-size adjustment failed, `max_run` would collapse to a
        // handful of cells and coalescing would buy nothing. A terminal row is
        // 80+ columns, so anything below that is a regression in the fix, not
        // just in the optimisation.
        let metrics = CellMetrics::resolve(DEFAULT_FONT_SIZE, 1.0);
        let plain = metrics.face(Face {
            bold: false,
            italic: false,
        });
        assert!(
            plain.max_run >= 80,
            "plain text must coalesce a whole row, got {}",
            plain.max_run
        );
    }

    #[test]
    fn a_run_is_chunked_to_the_face_ceiling_without_gaps_or_overlap() {
        let run = Run {
            start: 3,
            len: 10,
            key: (),
        };
        assert_eq!(
            run.chunks(4).collect::<Vec<_>>(),
            vec![
                Span { start: 3, len: 4 },
                Span { start: 7, len: 4 },
                Span { start: 11, len: 2 },
            ]
        );
        assert_eq!(
            run.chunks(u16::MAX).collect::<Vec<_>>(),
            vec![Span { start: 3, len: 10 }]
        );
        // A degenerate ceiling must still make progress, one cell at a time,
        // rather than loop forever on a zero-length span.
        assert_eq!(run.chunks(0).count(), 10);
    }

    #[test]
    fn a_degenerate_advance_measurement_yields_a_single_cell_ceiling() {
        assert_eq!(run_ceiling(f32::NAN), 1);
        assert_eq!(run_ceiling(f32::INFINITY), 1);
        assert_eq!(run_ceiling(0.0), u16::MAX);
        // Half a pixel of error per cell allows the anchored first cell plus
        // one more before the budget is spent.
        assert_eq!(run_ceiling(MAX_RUN_DRIFT_PX), 2);
        assert_eq!(run_ceiling(1.0), 1);
    }
}
