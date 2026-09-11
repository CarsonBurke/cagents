//! Native editor with an in-viewport gutter and scrollbar.
//!
//! Editing/IME handling is adapted from iced_widget's text_editor at libcosmic
//! caec74c2559924443f12fc6faf97a5bcefe6271d (MIT, Copyright 2019 Iced contributors).
//! The upstream Content hides the renderer buffer and offers no viewport hook;
//! keeping its state machine here lets chrome use the actual scroll position,
//! including caret reveal, without duplicating text or shaping the whole file.
//
// Copyright 2019 Héctor Ramón, Iced contributors
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
// FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
// COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
// IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
// CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
use cosmic::iced::advanced::text::{
    editor::Editor as _,
    highlighter::{self, Highlighter},
};
use cosmic::iced::advanced::{
    InputMethod, Shell,
    clipboard::{self, Clipboard},
    input_method,
    layout::{self, Layout},
    mouse, renderer,
    text::{self, Ellipsize, LineHeight, Text, Wrapping},
    widget::{self, Widget, operation},
};
#[cfg(test)]
pub use cosmic::iced::widget::text_editor::Motion;
pub use cosmic::iced::widget::text_editor::{
    Action, Cursor, Edit, Line, LineEnding, Position, Selection,
};
use cosmic::iced::widget::text_editor::{Binding, Catalog, KeyPress, Status, Style, StyleFn};
use cosmic::iced::{
    Border, Element, Event, Length, Padding, Pixels, Point, Rectangle, Size, Vector, alignment,
    keyboard, window,
};
use std::{
    borrow::Cow,
    cell::RefCell,
    fmt,
    ops::{self, DerefMut},
    sync::Arc,
    time::{Duration, Instant},
};
type NativeEditor = <cosmic::Renderer as text::Renderer>::Editor;
pub struct TextEditor<'a, Highlighter, Message, Theme = cosmic::Theme, Renderer = cosmic::Renderer>
where
    Highlighter: text::Highlighter,
    Theme: Catalog,
    Renderer: text::Renderer<Font = cosmic::iced::Font, Editor = NativeEditor>,
{
    id: Option<widget::Id>,
    content: &'a Content<Renderer>,
    line_digits: u32,
    additions: &'a [usize],
    removal_boundaries: &'a [usize],
    placeholder: Option<text::Fragment<'a>>,
    font: Option<Renderer::Font>,
    text_size: Option<Pixels>,
    line_height: LineHeight,
    width: Length,
    height: Length,
    min_height: f32,
    max_height: f32,
    padding: Padding,
    wrapping: Wrapping,
    ellipsize: text::Ellipsize,
    class: Theme::Class<'a>,
    key_binding: Option<Box<dyn Fn(KeyPress) -> Option<Binding<Message>> + 'a>>,
    on_edit: Option<Box<dyn Fn(Action) -> Message + 'a>>,
    highlighter_settings: Highlighter::Settings,
    highlighter_format: fn(&Highlighter::Highlight, &Theme) -> highlighter::Format<Renderer::Font>,
    last_status: Option<Status>,
}

impl<'a, Message, Theme, Renderer> TextEditor<'a, highlighter::PlainText, Message, Theme, Renderer>
where
    Theme: Catalog,
    Renderer: text::Renderer<Font = cosmic::iced::Font, Editor = NativeEditor>,
{
    /// Creates new [`TextEditor`] with the given [`Content`].
    pub fn new(content: &'a Content<Renderer>) -> Self {
        Self {
            id: None,
            content,
            line_digits: content.line_count().max(1).ilog10() + 1,
            additions: &[],
            removal_boundaries: &[],
            placeholder: None,
            font: None,
            text_size: Some(Pixels(13.0)),
            line_height: LineHeight::default(),
            width: Length::Fill,
            height: Length::Shrink,
            min_height: 0.0,
            max_height: f32::INFINITY,
            padding: Padding::new(5.0),
            wrapping: Wrapping::None,
            class: <Theme as Catalog>::default(),
            ellipsize: Ellipsize::default(),
            key_binding: None,
            on_edit: None,
            highlighter_settings: (),
            highlighter_format: |_highlight, _theme| highlighter::Format::default(),
            last_status: None,
        }
    }

    /// Sets the [`Id`](widget::Id) of the [`TextEditor`].
    pub fn id(mut self, id: impl Into<widget::Id>) -> Self {
        self.id = Some(id.into());
        self
    }
}

impl<'a, Highlighter, Message, Theme, Renderer>
    TextEditor<'a, Highlighter, Message, Theme, Renderer>
where
    Highlighter: text::Highlighter,
    Theme: Catalog,
    Renderer: text::Renderer<Font = cosmic::iced::Font, Editor = NativeEditor>,
{
    /// Sets the height of the [`TextEditor`].
    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }
    /// Sets the message that should be produced when some action is performed in
    /// the [`TextEditor`].
    ///
    /// If this method is not called, the [`TextEditor`] will be disabled.
    pub fn on_action(mut self, on_edit: impl Fn(Action) -> Message + 'a) -> Self {
        self.on_edit = Some(Box::new(on_edit));
        self
    }

    /// Zero-based source lines, sorted once when the diff completes. Deleted
    /// text never enters the editor buffer; its boundary is only a decoration.
    pub fn diff_lines(mut self, additions: &'a [usize], removals: &'a [usize]) -> Self {
        self.additions = additions;
        self.removal_boundaries = removals;
        self
    }

    /// Sets the [`Font`] of the [`TextEditor`].
    ///
    /// [`Font`]: text::Renderer::Font
    pub fn font(mut self, font: impl Into<Renderer::Font>) -> Self {
        self.font = Some(font.into());
        self
    }

    /// Sets the text size of the [`TextEditor`].
    pub fn size(mut self, size: impl Into<Pixels>) -> Self {
        self.text_size = Some(size.into());
        self
    }

    /// Sets the [`text::LineHeight`] of the [`TextEditor`].
    pub fn line_height(mut self, line_height: impl Into<text::LineHeight>) -> Self {
        self.line_height = line_height.into();
        self
    }

    /// Sets the [`Padding`] of the [`TextEditor`].
    pub fn padding(mut self, padding: impl Into<Padding>) -> Self {
        self.padding = padding.into();
        self
    }

    /// Sets the [`Wrapping`] strategy of the [`TextEditor`].
    pub fn wrapping(mut self, wrapping: Wrapping) -> Self {
        self.wrapping = wrapping;
        self
    }
    /// Highlights the [`TextEditor`] with the given [`Highlighter`] and
    /// a strategy to turn its highlights into some text format.
    pub fn highlight_with<H: text::Highlighter>(
        self,
        settings: H::Settings,
        to_format: fn(&H::Highlight, &Theme) -> highlighter::Format<Renderer::Font>,
    ) -> TextEditor<'a, H, Message, Theme, Renderer> {
        TextEditor {
            id: self.id,
            content: self.content,
            line_digits: self.line_digits,
            additions: self.additions,
            removal_boundaries: self.removal_boundaries,
            placeholder: self.placeholder,
            font: self.font,
            text_size: self.text_size,
            line_height: self.line_height,
            width: self.width,
            height: self.height,
            min_height: self.min_height,
            max_height: self.max_height,
            padding: self.padding,
            wrapping: self.wrapping,
            ellipsize: self.ellipsize,
            class: self.class,
            key_binding: self.key_binding,
            on_edit: self.on_edit,
            highlighter_settings: settings,
            highlighter_format: to_format,
            last_status: self.last_status,
        }
    }

    /// Sets the closure to produce key bindings on key presses.
    ///
    /// See [`Binding`] for the list of available bindings.
    pub fn key_binding(
        mut self,
        key_binding: impl Fn(KeyPress) -> Option<Binding<Message>> + 'a,
    ) -> Self {
        self.key_binding = Some(Box::new(key_binding));
        self
    }

    /// Sets the style of the [`TextEditor`].
    #[must_use]
    pub fn style(mut self, style: impl Fn(&Theme, Status) -> Style + 'a) -> Self
    where
        Theme::Class<'a>: From<StyleFn<'a, Theme>>,
    {
        self.class = (Box::new(style) as StyleFn<'a, Theme>).into();
        self
    }

    fn text_padding(&self) -> Padding {
        Padding {
            left: self.padding.left
                + self.line_digits as f32 * self.text_size.unwrap_or(Pixels(16.0)).0 * 0.7
                + 16.0,
            right: self.padding.right + crate::design::SCROLLBAR_GUTTER,
            ..self.padding
        }
    }

    fn scroll_geometry(&self, bounds: Rectangle) -> ScrollGeometry {
        let internal = self.content.0.borrow();
        let buffer = internal.editor.buffer();
        let text_bounds = bounds.shrink(self.text_padding());
        let line_height = buffer.metrics().line_height.max(1.0);
        let scroll = buffer.scroll();
        let wrapped_rows = buffer
            .lines
            .get(scroll.line)
            .and_then(|line| line.layout_opt())
            .map_or(1, Vec::len)
            .max(1) as f32;
        let first = scroll.line as f32 + scroll.vertical / (line_height * wrapped_rows);
        let mut visible = 0.0;
        for run in buffer.layout_runs() {
            let rows = buffer.lines[run.line_i]
                .layout_opt()
                .map_or(1, Vec::len)
                .max(1) as f32;
            let pixels = ((run.line_top + run.line_height).min(text_bounds.height)
                - run.line_top.max(0.0))
            .max(0.0);
            visible += pixels / (line_height * rows);
        }
        let visible = visible.max(1.0 / wrapped_rows);
        ScrollGeometry::new(
            bounds,
            text_bounds,
            buffer.lines.len().max(1) as f32,
            first,
            visible,
        )
    }

    fn input_method<'b>(
        &self,
        state: &'b State<Highlighter>,
        renderer: &Renderer,
        layout: Layout<'_>,
    ) -> InputMethod<&'b str> {
        let Some(Focus {
            is_window_focused: true,
            ..
        }) = &state.focus
        else {
            return InputMethod::Disabled;
        };

        let bounds = layout.bounds();
        let internal = self.content.0.borrow_mut();

        let text_bounds = bounds.shrink(self.text_padding());
        let translation = text_bounds.position() - Point::ORIGIN
            + Vector::new(internal.horizontal_translation(), 0.0);

        let cursor = match internal.selection() {
            Selection::Caret(position) => position,
            Selection::Range(ranges) => ranges.first().cloned().unwrap_or_default().position(),
        };

        let line_height = self
            .line_height
            .to_absolute(self.text_size.unwrap_or_else(|| renderer.default_size()));

        let position = cursor + translation;

        InputMethod::Enabled {
            cursor: Rectangle::new(position, Size::new(1.0, f32::from(line_height))),
            purpose: input_method::Purpose::Normal,
            preedit: state.preedit.as_ref().map(input_method::Preedit::as_ref),
        }
    }
}

/// A small logical anchor independent of the document's text allocation.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ScrollPosition {
    pub line: usize,
    pub vertical: f32,
    pub horizontal: f32,
}

#[derive(Debug, Clone, Copy)]
enum ScrollTarget {
    Fraction { fraction: f32, visible: f32 },
    Position(ScrollPosition),
}

/// The native renderer exposes relative visual-row scrolling, not a mutable
/// buffer or logical seek. Shape and observe each step until the logical target
/// is reached. Each step consumes at least the rest of the current wrapped line
/// without scanning unshaped prefixes or moving the caret/selection.
fn seek_scroll(
    editor: &mut NativeEditor,
    target: ScrollTarget,
    mut shape: impl FnMut(&mut NativeEditor),
) {
    let position = match target {
        ScrollTarget::Fraction { fraction, visible } => {
            (editor.buffer().lines.len() as f32 - visible).max(0.0) * fraction.clamp(0.0, 1.0)
        }
        ScrollTarget::Position(position) => position.line as f32,
    };
    let target_line =
        (position.floor() as usize).min(editor.buffer().lines.len().saturating_sub(1));
    let mut previous = None;
    loop {
        let buffer = editor.buffer();
        let scroll = buffer.scroll();
        let before = (scroll.line, scroll.vertical);
        let line_height = buffer.metrics().line_height.max(1.0);
        let current_row = scroll.vertical / line_height;
        let rows = buffer
            .lines
            .get(scroll.line)
            .and_then(|line| line.layout_opt())
            .map_or(1, Vec::len)
            .max(1) as f32;
        let delta = if matches!(target, ScrollTarget::Fraction { fraction, .. } if fraction <= 0.0)
        {
            -i32::MAX
        } else if matches!(target, ScrollTarget::Fraction { fraction, .. } if fraction >= 1.0) {
            i32::MAX
        } else if scroll.line < target_line {
            // Every intervening logical line occupies at least one row.
            // Unlike summing guessed layout sizes, this is a progress bound,
            // not an assumption that the first step reaches the destination.
            (rows - current_row + (target_line - scroll.line - 1) as f32)
                .ceil()
                .max(1.0)
                .min(i32::MAX as f32) as i32
        } else if scroll.line > target_line {
            -(current_row + (scroll.line - target_line) as f32)
                .ceil()
                .max(1.0)
                .min(i32::MAX as f32) as i32
        } else {
            let target_row = match target {
                ScrollTarget::Fraction { .. } => position.fract() * rows,
                ScrollTarget::Position(position) => position.vertical / line_height,
            }
            .floor()
            .min(rows - 1.0);
            (target_row - current_row).round() as i32
        };
        if delta == 0 {
            return;
        }
        editor.perform(Action::Scroll { lines: delta });
        shape(editor);
        let after = editor.buffer().scroll();
        // Native endpoint clamping cannot advance, and fractional pixel
        // rounding must not trap a seek in a two-position cycle.
        if (after.line, after.vertical) == before || previous == Some((after.line, after.vertical))
        {
            return;
        }
        previous = Some(before);
    }
}

/// The content of a [`TextEditor`].
pub struct Content<R = cosmic::Renderer>(RefCell<Internal<R>>)
where
    R: text::Renderer<Font = cosmic::iced::Font, Editor = NativeEditor>;

struct Internal<R>
where
    R: text::Renderer<Font = cosmic::iced::Font, Editor = NativeEditor>,
{
    editor: R::Editor,
    pending_scroll: Option<ScrollPosition>,
    // Native glyphs/hit tests use document X, but its scroll setter is private.
    // Own the viewport over a real unwrapped canvas so renderer damage bounds
    // cover visible glyphs even when the viewport is far along a long line.
    horizontal: f32,
    reveal_horizontal: bool,
    canvas_width: f32,
    wrapping: Wrapping,
}

impl<R> Internal<R>
where
    R: text::Renderer<Font = cosmic::iced::Font, Editor = NativeEditor>,
{
    fn horizontal_translation(&self) -> f32 {
        let displayed = if self.wrapping == Wrapping::None {
            self.horizontal
        } else {
            0.0
        };
        -displayed
    }

    fn caret_position(&self) -> Option<Point> {
        use cosmic::iced::advanced::graphics::text::cosmic_text;
        let position = self.editor.cursor().position;
        let cursor = cosmic_text::Cursor::new_with_affinity(
            position.line,
            position.column,
            cosmic_text::Affinity::Before,
        );
        self.editor.buffer().layout_runs().find_map(|run| {
            run.cursor_position(&cursor)
                .map(|x| Point::new(x, run.line_top))
        })
    }

    fn selection(&self) -> Selection {
        let selection = self.editor.selection();
        // Avoid upstream's viewport-width clamp; use the same raw glyph
        // coordinate for the caret, reveal, hit testing, and IME placement.
        if matches!(selection, Selection::Caret(_))
            && let Some(point) = self.caret_position()
        {
            return Selection::Caret(point);
        }
        selection
    }
}

impl<R> Content<R>
where
    R: text::Renderer<Font = cosmic::iced::Font, Editor = NativeEditor>,
{
    /// Creates an empty [`Content`].
    pub fn new() -> Self {
        Self::with_text("")
    }

    /// Creates a [`Content`] with the given text.
    pub fn with_text(text: &str) -> Self {
        Self(RefCell::new(Internal {
            editor: R::Editor::with_text(text),
            pending_scroll: None,
            horizontal: 0.0,
            reveal_horizontal: true,
            canvas_width: 0.0,
            wrapping: Wrapping::None,
        }))
    }

    /// Performs an [`Action`] on the [`Content`].
    pub fn perform(&mut self, mut action: Action) {
        let internal = self.0.get_mut();
        internal.pending_scroll = None;
        match &mut action {
            Action::Click(position) | Action::Drag(position) => {
                position.x -= internal.horizontal_translation();
            }
            _ => {}
        }
        if matches!(
            action,
            Action::Move(_) | Action::Select(_) | Action::Edit(_)
        ) {
            internal.reveal_horizontal = true;
        }

        internal.editor.perform(action);
    }

    /// Moves the current cursor to reflect the given one.
    pub fn move_to(&mut self, cursor: Cursor) {
        let internal = self.0.get_mut();
        internal.pending_scroll = None;
        internal.reveal_horizontal = true;

        internal.editor.move_to(cursor);
    }

    /// Reads the native viewport without copying or shaping document text.
    pub fn scroll_position(&self) -> ScrollPosition {
        let internal = self.0.borrow();
        internal.pending_scroll.unwrap_or_else(|| {
            let scroll = internal.editor.buffer().scroll();
            ScrollPosition {
                line: scroll.line,
                vertical: scroll.vertical,
                horizontal: internal.horizontal,
            }
        })
    }

    /// Restores after layout knows the current font, wrapping, and viewport.
    /// A subsequent explicit edit/caret move takes precedence over restoration.
    pub fn restore_scroll_position(&mut self, position: ScrollPosition) {
        self.0.get_mut().pending_scroll = Some(ScrollPosition {
            line: position.line,
            vertical: if position.vertical.is_finite() {
                position.vertical.max(0.0)
            } else {
                0.0
            },
            horizontal: if position.horizontal.is_finite() {
                position.horizontal.max(0.0)
            } else {
                0.0
            },
        });
    }

    /// Returns the current cursor position of the [`Content`].
    pub fn cursor(&self) -> Cursor {
        self.0.borrow().editor.cursor()
    }

    /// Returns the amount of lines of the [`Content`].
    pub fn line_count(&self) -> usize {
        self.0.borrow().editor.line_count()
    }
    /// Diff rows count LF-delimited source lines; the native buffer also splits
    /// bare CR. Map source lines without serializing or cloning line text.
    pub fn source_line_ranges(&self) -> Vec<std::ops::Range<usize>> {
        let internal = self.0.borrow();
        let count = internal.editor.line_count();
        let mut ranges = Vec::new();
        let mut start = 0;
        for index in 0..count {
            let Some(line) = internal.editor.line(index) else {
                break;
            };
            if index + 1 == count || line.ending.as_str().contains('\n') {
                ranges.push(start..index + 1);
                start = index + 1;
            }
        }
        ranges
    }

    /// Returns the text of the line at the given index, if it exists.
    pub fn line(&self, index: usize) -> Option<Line<'_>> {
        let internal = self.0.borrow();
        let line = internal.editor.line(index)?;

        Some(Line {
            text: Cow::Owned(line.text.into_owned()),
            ending: line.ending,
        })
    }

    /// Returns an iterator of the text of the lines in the [`Content`].
    pub fn lines(&self) -> impl Iterator<Item = Line<'_>> {
        (0..)
            .map(|i| self.line(i))
            .take_while(Option::is_some)
            .flatten()
    }

    /// Returns the text of the [`Content`].
    pub fn text(&self) -> String {
        let internal = self.0.borrow();
        let count = internal.editor.line_count();
        let mut contents = String::new();
        for index in 0..count {
            let Some(line) = internal.editor.line(index) else {
                break;
            };
            contents.push_str(&line.text);
            if index + 1 < count {
                contents.push_str(if line.ending == LineEnding::None {
                    LineEnding::default().as_str()
                } else {
                    line.ending.as_str()
                });
            }
        }

        contents
    }

    /// Returns the selected text of the [`Content`].
    pub fn selection(&self) -> Option<String> {
        self.0.borrow().editor.copy()
    }

    /// Returns the kind of [`LineEnding`] used for separating lines in the [`Content`].
    pub fn line_ending(&self) -> Option<LineEnding> {
        Some(self.line(0)?.ending)
    }

    /// Returns whether or not the the [`Content`] is empty.
    pub fn is_empty(&self) -> bool {
        self.0.borrow().editor.is_empty()
    }
}

impl<Renderer> Clone for Content<Renderer>
where
    Renderer: text::Renderer<Font = cosmic::iced::Font, Editor = NativeEditor>,
{
    fn clone(&self) -> Self {
        Self::with_text(&self.text())
    }
}

impl<Renderer> Default for Content<Renderer>
where
    Renderer: text::Renderer<Font = cosmic::iced::Font, Editor = NativeEditor>,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<Renderer> fmt::Debug for Content<Renderer>
where
    Renderer: text::Renderer<Font = cosmic::iced::Font, Editor = NativeEditor>,
    Renderer::Editor: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let internal = self.0.borrow();

        f.debug_struct("Content")
            .field("editor", &internal.editor)
            .finish()
    }
}

/// The state of a [`TextEditor`].
#[derive(Debug)]
pub struct State<Highlighter: text::Highlighter> {
    focus: Option<Focus>,
    preedit: Option<input_method::Preedit>,
    last_click: Option<mouse::Click>,
    drag_click: Option<mouse::click::Kind>,
    partial_scroll: f32,
    scrollbar_grab: Option<f32>,
    scrollbar_target: Option<ScrollTarget>,
    last_theme: RefCell<Option<String>>,
    highlighter: RefCell<Highlighter>,
    highlighter_settings: Highlighter::Settings,
    highlighter_format_address: usize,
    context_menu_position: Option<Point>,
    pending_edit: Option<Action>,
}

#[derive(Debug, Clone)]
struct Focus {
    updated_at: Instant,
    now: Instant,
    is_window_focused: bool,
}

impl Focus {
    const CURSOR_BLINK_INTERVAL_MILLIS: u128 = 500;

    fn now() -> Self {
        let now = Instant::now();

        Self {
            updated_at: now,
            now,
            is_window_focused: true,
        }
    }

    fn is_cursor_visible(&self) -> bool {
        self.is_window_focused
            && ((self.now - self.updated_at).as_millis() / Self::CURSOR_BLINK_INTERVAL_MILLIS)
                .is_multiple_of(2)
    }
}

impl<Highlighter: text::Highlighter> State<Highlighter> {
    /// Clears focus, selection, and all interaction state.
    pub fn clear_focus(&mut self) {
        self.focus = None;
        self.drag_click = None;
        self.context_menu_position = None;
        self.pending_edit = Some(Action::ClearSelection);
    }
}

impl<Highlighter: text::Highlighter> operation::Focusable for State<Highlighter> {
    fn is_focused(&self) -> bool {
        self.focus.is_some()
    }

    fn focus(&mut self) {
        self.focus = Some(Focus::now());
    }

    fn unfocus(&mut self) {
        self.clear_focus();
    }
}

impl<Highlighter, Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for TextEditor<'_, Highlighter, Message, Theme, Renderer>
where
    Highlighter: text::Highlighter,
    Theme: Catalog,
    Renderer: text::Renderer<Font = cosmic::iced::Font, Editor = NativeEditor>,
{
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<State<Highlighter>>()
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(State {
            focus: None,
            preedit: None,
            last_click: None,
            drag_click: None,
            partial_scroll: 0.0,
            scrollbar_grab: None,
            scrollbar_target: None,
            last_theme: RefCell::default(),
            highlighter: RefCell::new(Highlighter::new(&self.highlighter_settings)),
            highlighter_settings: self.highlighter_settings.clone(),
            highlighter_format_address: self.highlighter_format as usize,
            context_menu_position: None,
            pending_edit: None,
        })
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: self.height,
        }
    }

    fn layout(
        &mut self,
        tree: &mut widget::Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let mut internal = self.content.0.borrow_mut();
        let state = tree.state.downcast_mut::<State<Highlighter>>();

        if state.highlighter_format_address != self.highlighter_format as usize {
            state.highlighter.borrow_mut().change_line(0);

            state.highlighter_format_address = self.highlighter_format as usize;
        }

        if state.highlighter_settings != self.highlighter_settings {
            state
                .highlighter
                .borrow_mut()
                .update(&self.highlighter_settings);

            state.highlighter_settings = self.highlighter_settings.clone();
        }

        let limits = limits
            .width(self.width)
            .height(self.height)
            .min_height(self.min_height)
            .max_height(self.max_height);

        let restored = internal.pending_scroll.take().map(ScrollTarget::Position);
        let target = state.scrollbar_target.take().or(restored);
        let viewport = limits.shrink(self.text_padding()).max();
        let unwrapped = self.wrapping == Wrapping::None;
        if let Some(ScrollTarget::Position(position)) = restored {
            internal.horizontal = position.horizontal;
            internal.reveal_horizontal = false;
        }
        internal.wrapping = self.wrapping;
        let mut canvas = viewport;
        if unwrapped {
            canvas.width = internal
                .canvas_width
                .max((viewport.width + internal.horizontal).min(f32::MAX));
        }
        let shape = |editor: &mut NativeEditor, bounds| {
            editor.update(
                bounds,
                self.font.unwrap_or_else(|| renderer.default_font()),
                self.text_size.unwrap_or_else(|| renderer.default_size()),
                self.line_height,
                self.wrapping,
                state.highlighter.borrow_mut().deref_mut(),
            );
        };
        shape(&mut internal.editor, canvas);
        if let Some(target) = target {
            seek_scroll(&mut internal.editor, target, |editor| shape(editor, canvas));
        }
        if unwrapped {
            // Grow only for newly visited wider lines; never scan/shape the
            // document's unvisited prefix or copy its text to measure it.
            let width = internal
                .editor
                .buffer()
                .layout_runs()
                .map(|run| run.line_w + 1.0)
                .fold(canvas.width, f32::max);
            if width > canvas.width {
                canvas.width = width;
                shape(&mut internal.editor, canvas);
            }
            internal.canvas_width = canvas.width;
            if internal.reveal_horizontal
                && let Some(caret) = internal.caret_position()
            {
                internal.horizontal = internal
                    .horizontal
                    .min(caret.x)
                    .max(caret.x + 1.0 - viewport.width)
                    .max(0.0);
            }
        } else if internal.reveal_horizontal {
            internal.horizontal = 0.0;
        }
        internal.reveal_horizontal = false;

        match self.height {
            Length::Fill | Length::FillPortion(_) | Length::Fixed(_) => {
                layout::Node::new(limits.max())
            }
            Length::Shrink => {
                let min_bounds = internal.editor.min_bounds();

                layout::Node::new(
                    limits
                        .height(min_bounds.height)
                        .max()
                        .expand(Size::new(0.0, self.text_padding().y())),
                )
            }
        }
    }

    fn update(
        &mut self,
        tree: &mut widget::Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let geometry = self.scroll_geometry(layout.bounds());
        let state = tree.state.downcast_mut::<State<Highlighter>>();
        if !geometry.scrollable {
            state.scrollbar_grab = None;
            state.scrollbar_target = None;
        }
        let pointer = cursor.position().filter(|point| _viewport.contains(*point));
        let drag = match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if self.on_edit.is_some()
                    && geometry.scrollable
                    && pointer.is_some_and(|point| geometry.track.contains(point)) =>
            {
                let point = pointer.unwrap();
                let grab = if geometry.thumb.contains(point) {
                    point.y - geometry.thumb.y
                } else {
                    geometry.thumb.height / 2.0
                };
                state.scrollbar_grab = Some(grab);
                Some((point.y, grab))
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.scrollbar_grab.is_some() => {
                cursor
                    .position()
                    .map(|point| (point.y, state.scrollbar_grab.unwrap()))
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                if state.scrollbar_grab.take().is_some() =>
            {
                shell.capture_event();
                shell.request_redraw();
                return;
            }
            Event::Window(window::Event::Unfocused) => {
                state.scrollbar_grab = None;
                None
            }
            _ => None,
        };
        if let Some((y, grab)) = drag {
            state.scrollbar_target = Some(ScrollTarget::Fraction {
                fraction: geometry.fraction_at(y, grab),
                visible: geometry.visible,
            });
            shell.invalidate_layout();
            shell.capture_event();
            shell.request_redraw();
            return;
        }
        // Handle context menu and focus-loss on clicks outside.
        {
            let state = tree.state.downcast_mut::<State<Highlighter>>();
            let text_bounds = layout.bounds();

            if matches!(
                event,
                Event::Mouse(mouse::Event::ButtonPressed(_))
                    | Event::Touch(cosmic::iced::touch::Event::FingerPressed { .. })
            ) && cursor.position_over(text_bounds).is_none()
            {
                state.clear_focus();
            }

            match event {
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                    if let Some(pos) = cursor.position_over(text_bounds) {
                        if state.focus.is_none() {
                            state.focus = Some(Focus::now());
                        }
                        state.context_menu_position = Some(pos);
                        shell.capture_event();
                        return;
                    }
                }
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                    if state.context_menu_position.take().is_some() {
                        shell.capture_event();
                        return;
                    }
                }
                _ => {}
            }
        }

        // Process pending edits from context menu
        // publishes via on_edit so the app calls content.perform().
        {
            let state = tree.state.downcast_mut::<State<Highlighter>>();
            if let Some(action) = state.pending_edit.take() {
                if let Some(on_edit) = self.on_edit.as_ref() {
                    shell.publish((on_edit)(action));
                }
            }
        }

        let Some(on_edit) = self.on_edit.as_ref() else {
            return;
        };

        let state = tree.state.downcast_mut::<State<Highlighter>>();
        let is_redraw = matches!(event, Event::Window(window::Event::RedrawRequested(_now)),);

        match event {
            Event::Window(window::Event::Unfocused) => {
                if let Some(focus) = &mut state.focus {
                    focus.is_window_focused = false;
                }
            }
            Event::Window(window::Event::Focused) => {
                if let Some(focus) = &mut state.focus {
                    focus.is_window_focused = true;
                    focus.updated_at = Instant::now();

                    shell.request_redraw();
                }
            }
            Event::Window(window::Event::RedrawRequested(now)) => {
                if let Some(focus) = &mut state.focus
                    && focus.is_window_focused
                {
                    focus.now = *now;

                    let millis_until_redraw = Focus::CURSOR_BLINK_INTERVAL_MILLIS
                        - (focus.now - focus.updated_at).as_millis()
                            % Focus::CURSOR_BLINK_INTERVAL_MILLIS;

                    shell.request_redraw_at(
                        focus.now + Duration::from_millis(millis_until_redraw as u64),
                    );
                }
            }
            _ => {}
        }

        if let Some(update) = Update::from_event(
            event,
            state,
            layout.bounds(),
            self.text_padding(),
            cursor,
            self.key_binding.as_deref(),
        ) {
            match update {
                Update::Click(click) => {
                    let action = match click.kind() {
                        mouse::click::Kind::Single => Action::Click(click.position()),
                        mouse::click::Kind::Double => Action::SelectWord,
                        mouse::click::Kind::Triple => Action::SelectLine,
                    };

                    state.focus = Some(Focus::now());
                    state.last_click = Some(click);
                    state.drag_click = Some(click.kind());

                    shell.publish(on_edit(action));
                    shell.capture_event();
                }
                Update::Drag(position) => {
                    shell.publish(on_edit(Action::Drag(position)));
                }
                Update::Release => {
                    state.drag_click = None;
                }
                Update::Scroll(lines) => {
                    let bounds = self.content.0.borrow().editor.bounds();

                    if bounds.height >= i32::MAX as f32 {
                        return;
                    }

                    let lines = lines + state.partial_scroll;
                    state.partial_scroll = lines.fract();

                    shell.publish(on_edit(Action::Scroll {
                        lines: lines as i32,
                    }));
                    shell.capture_event();
                }
                Update::InputMethod(update) => match update {
                    Ime::Toggle(is_open) => {
                        state.preedit = is_open.then(input_method::Preedit::new);

                        shell.request_redraw();
                    }
                    Ime::Preedit { content, selection } => {
                        state.preedit = Some(input_method::Preedit {
                            content,
                            selection,
                            text_size: self.text_size,
                        });

                        shell.request_redraw();
                    }
                    Ime::Commit(text) => {
                        shell.publish(on_edit(Action::Edit(Edit::Paste(Arc::new(text)))));
                    }
                },
                Update::Binding(binding) => {
                    fn apply_binding<
                        H: text::Highlighter,
                        R: text::Renderer<Font = cosmic::iced::Font, Editor = NativeEditor>,
                        Message,
                    >(
                        binding: Binding<Message>,
                        content: &Content<R>,
                        state: &mut State<H>,
                        on_edit: &dyn Fn(Action) -> Message,
                        clipboard: &mut dyn Clipboard,
                        shell: &mut Shell<'_, Message>,
                    ) {
                        let mut publish = |action| shell.publish(on_edit(action));

                        match binding {
                            Binding::Unfocus => {
                                state.focus = None;
                                state.drag_click = None;
                            }
                            Binding::Copy => {
                                if let Some(selection) = content.selection() {
                                    clipboard.write(clipboard::Kind::Standard, selection);
                                }
                            }
                            Binding::Cut => {
                                if let Some(selection) = content.selection() {
                                    clipboard.write(clipboard::Kind::Standard, selection);

                                    publish(Action::Edit(Edit::Delete));
                                }
                            }
                            Binding::Paste => {
                                if let Some(contents) = clipboard.read(clipboard::Kind::Standard) {
                                    publish(Action::Edit(Edit::Paste(Arc::new(contents))));
                                }
                            }
                            Binding::Move(motion) => {
                                publish(Action::Move(motion));
                            }
                            Binding::Select(motion) => {
                                publish(Action::Select(motion));
                            }
                            Binding::SelectWord => {
                                publish(Action::SelectWord);
                            }
                            Binding::SelectLine => {
                                publish(Action::SelectLine);
                            }
                            Binding::SelectAll => {
                                publish(Action::SelectAll);
                            }
                            Binding::Insert(c) => {
                                publish(Action::Edit(Edit::Insert(c)));
                            }
                            Binding::Enter => {
                                publish(Action::Edit(Edit::Enter));
                            }
                            Binding::Backspace => {
                                publish(Action::Edit(Edit::Backspace));
                            }
                            Binding::Delete => {
                                publish(Action::Edit(Edit::Delete));
                            }
                            Binding::Sequence(sequence) => {
                                for binding in sequence {
                                    apply_binding(
                                        binding, content, state, on_edit, clipboard, shell,
                                    );
                                }
                            }
                            Binding::Custom(message) => {
                                shell.publish(message);
                            }
                        }
                    }

                    if !matches!(binding, Binding::Unfocus) {
                        shell.capture_event();
                    }

                    apply_binding(binding, self.content, state, on_edit, clipboard, shell);

                    if let Some(focus) = &mut state.focus {
                        focus.updated_at = Instant::now();
                    }
                }
            }
        }

        let status = {
            let is_disabled = self.on_edit.is_none();
            let is_hovered = cursor.is_over(layout.bounds());

            if is_disabled {
                Status::Disabled
            } else if state.focus.is_some() {
                Status::Focused { is_hovered }
            } else if is_hovered {
                Status::Hovered
            } else {
                Status::Active
            }
        };

        if is_redraw {
            self.last_status = Some(status);

            shell.request_input_method(&self.input_method(state, renderer, layout));
        } else if self
            .last_status
            .is_some_and(|last_status| status != last_status)
        {
            shell.request_redraw();
        }
    }

    fn draw(
        &self,
        tree: &widget::Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        _defaults: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();

        let mut internal = self.content.0.borrow_mut();
        let state = tree.state.downcast_ref::<State<Highlighter>>();

        let font = self.font.unwrap_or_else(|| renderer.default_font());

        let theme_name = theme.name();

        if state
            .last_theme
            .borrow()
            .as_ref()
            .is_none_or(|last_theme| last_theme != theme_name)
        {
            state.highlighter.borrow_mut().change_line(0);
            let _ = state.last_theme.borrow_mut().replace(theme_name.to_owned());
        }

        internal.editor.highlight(
            font,
            state.highlighter.borrow_mut().deref_mut(),
            |highlight| (self.highlighter_format)(highlight, theme),
        );

        let style = theme.style(&self.class, self.last_status.unwrap_or(Status::Active));

        renderer.fill_quad(
            renderer::Quad {
                bounds,
                border: style.border,
                ..renderer::Quad::default()
            },
            style.background,
        );

        let text_bounds = bounds.shrink(self.text_padding());
        // Paint changes under selection and glyphs, using only shaped visible
        // runs. Wrapped continuations share their logical source annotation.
        let buffer = internal.editor.buffer();
        let scroll = buffer.scroll();
        let mut previous_line = None;
        let mut logical_top = 0.0;
        for run in internal.editor.buffer().layout_runs() {
            if previous_line != Some(run.line_i) {
                logical_top = if run.line_i == scroll.line {
                    -scroll.vertical
                } else {
                    run.line_top
                };
                previous_line = Some(run.line_i);
            }
            if self.additions.binary_search(&run.line_i).is_ok() {
                let row = Rectangle::new(
                    Point::new(text_bounds.x, text_bounds.y + run.line_top),
                    Size::new(text_bounds.width, run.line_height),
                );
                if let Some(bounds) = text_bounds.intersection(&row) {
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds,
                            ..renderer::Quad::default()
                        },
                        cosmic::iced::Color::from_rgba8(46, 160, 93, 0.12),
                    );
                }
            }
            let eof = run.line_i + 1 == buffer.lines.len();
            let rows = buffer.lines[run.line_i]
                .layout_opt()
                .map_or(1, Vec::len)
                .max(1);
            let first_run = (run.line_top - logical_top).abs() < 0.1;
            let last_run =
                run.line_top + run.line_height >= logical_top + rows as f32 * run.line_height - 0.1;
            let before = first_run && self.removal_boundaries.binary_search(&run.line_i).is_ok();
            let after = eof
                && last_run
                && self
                    .removal_boundaries
                    .binary_search(&(run.line_i + 1))
                    .is_ok();
            for (visible, offset) in [(before, 0.0), (after, run.line_height - 2.0)] {
                if visible {
                    let boundary = Rectangle::new(
                        Point::new(text_bounds.x, text_bounds.y + run.line_top + offset),
                        Size::new(text_bounds.width, 2.0),
                    );
                    if let Some(bounds) = text_bounds.intersection(&boundary) {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds,
                                ..renderer::Quad::default()
                            },
                            cosmic::iced::Color::from_rgba8(220, 75, 75, 0.65),
                        );
                    }
                }
            }
        }

        let horizontal = Vector::new(internal.horizontal_translation(), 0.0);
        let translation = text_bounds.position() - Point::ORIGIN + horizontal;
        let selection = state.focus.as_ref().map(|_| internal.selection());
        if let Some(Selection::Range(ranges)) = &selection {
            let mut selection_color = style.selection;
            selection_color.a *= 0.24;
            for range in ranges
                .iter()
                .filter_map(|range| text_bounds.intersection(&(*range + translation)))
            {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: range,
                        ..renderer::Quad::default()
                    },
                    selection_color,
                );
            }
        }

        if internal.editor.is_empty() {
            if let Some(placeholder) = self.placeholder.clone() {
                renderer.fill_text(
                    Text {
                        content: placeholder.into_owned(),
                        bounds: text_bounds.size(),
                        size: self.text_size.unwrap_or_else(|| renderer.default_size()),
                        line_height: self.line_height,
                        font,
                        align_x: text::Alignment::Default,
                        align_y: alignment::Vertical::Top,
                        shaping: text::Shaping::Advanced,
                        wrapping: self.wrapping,
                        ellipsize: self.ellipsize,
                    },
                    text_bounds.position(),
                    style.placeholder,
                    text_bounds,
                );
            }
        } else {
            renderer.fill_editor(
                &internal.editor,
                text_bounds.position() + horizontal,
                style.value,
                text_bounds,
            );
        }

        if let (Some(focus), Some(selection)) = (state.focus.as_ref(), selection) {
            match selection {
                Selection::Caret(position) if focus.is_cursor_visible() => {
                    let cursor = Rectangle::new(
                        position + translation,
                        Size::new(
                            1.0,
                            self.line_height
                                .to_absolute(
                                    self.text_size.unwrap_or_else(|| renderer.default_size()),
                                )
                                .into(),
                        ),
                    );

                    if let Some(clipped_cursor) = text_bounds.intersection(&cursor) {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: clipped_cursor,
                                ..renderer::Quad::default()
                            },
                            style.value,
                        );
                    }
                }
                Selection::Range(_) | Selection::Caret(_) => {}
            }
        }

        let gutter_right = text_bounds.x - 12.0;
        let gutter_clip = Rectangle {
            x: bounds.x,
            width: (gutter_right - bounds.x).max(0.0),
            y: text_bounds.y,
            height: text_bounds.height,
        };
        let mut previous = None;
        let mut number_color = style.value;
        number_color.a *= 0.48;
        for run in internal.editor.buffer().layout_runs() {
            if previous == Some(run.line_i) {
                continue;
            }
            previous = Some(run.line_i);
            renderer.fill_text(
                Text {
                    content: (run.line_i + 1).to_string(),
                    bounds: Size::new(gutter_clip.width, run.line_height),
                    size: self.text_size.unwrap_or_else(|| renderer.default_size()),
                    line_height: self.line_height,
                    font,
                    align_x: text::Alignment::Right,
                    align_y: alignment::Vertical::Top,
                    shaping: text::Shaping::Basic,
                    wrapping: Wrapping::None,
                    ellipsize: Ellipsize::None,
                },
                Point::new(gutter_right, text_bounds.y + run.line_top),
                number_color,
                gutter_clip,
            );
        }
        drop(internal);
        let geometry = self.scroll_geometry(bounds);
        if !geometry.scrollable {
            return;
        }
        let mut track_color = style.value;
        track_color.a *= 0.06;
        renderer.fill_quad(
            renderer::Quad {
                bounds: geometry.track,
                border: Border {
                    radius: 4.0.into(),
                    ..Border::default()
                },
                ..renderer::Quad::default()
            },
            track_color,
        );
        let mut thumb_color = style.value;
        thumb_color.a *= if state.scrollbar_grab.is_some() || _cursor.is_over(geometry.track) {
            0.55
        } else {
            0.3
        };
        renderer.fill_quad(
            renderer::Quad {
                bounds: geometry.thumb,
                border: Border {
                    radius: 4.0.into(),
                    ..Border::default()
                },
                ..renderer::Quad::default()
            },
            thumb_color,
        );
    }

    fn mouse_interaction(
        &self,
        _tree: &widget::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let is_disabled = self.on_edit.is_none();
        if !cursor
            .position()
            .is_some_and(|point| _viewport.contains(point))
        {
            return mouse::Interaction::default();
        }
        let geometry = self.scroll_geometry(layout.bounds());
        if cursor.is_over(geometry.track) {
            return if !is_disabled && geometry.scrollable {
                mouse::Interaction::Pointer
            } else {
                mouse::Interaction::default()
            };
        }
        if !cursor.is_over(layout.bounds().shrink(self.text_padding())) {
            return mouse::Interaction::default();
        }
        if cursor.is_over(layout.bounds()) {
            if is_disabled {
                mouse::Interaction::NotAllowed
            } else {
                mouse::Interaction::Text
            }
        } else {
            mouse::Interaction::default()
        }
    }

    fn operate(
        &mut self,
        tree: &mut widget::Tree,
        layout: Layout<'_>,
        _renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        let state = tree.state.downcast_mut::<State<Highlighter>>();

        operation.focusable(self.id.as_ref(), layout.bounds(), state);
    }

    fn id(&self) -> Option<widget::Id> {
        self.id.clone()
    }
    fn set_id(&mut self, id: widget::Id) {
        self.id = Some(id);
    }
}

impl<'a, Highlighter, Message, Theme, Renderer>
    From<TextEditor<'a, Highlighter, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Highlighter: text::Highlighter,
    Message: 'a,
    Theme: Catalog + 'a,
    Renderer: text::Renderer<Font = cosmic::iced::Font, Editor = NativeEditor>,
{
    fn from(text_editor: TextEditor<'a, Highlighter, Message, Theme, Renderer>) -> Self {
        Self::new(text_editor)
    }
}

enum Update<Message> {
    Click(mouse::Click),
    Drag(Point),
    Release,
    Scroll(f32),
    InputMethod(Ime),
    Binding(Binding<Message>),
}

enum Ime {
    Toggle(bool),
    Preedit {
        content: String,
        selection: Option<ops::Range<usize>>,
    },
    Commit(String),
}

impl<Message> Update<Message> {
    fn from_event<H: Highlighter>(
        event: &Event,
        state: &State<H>,
        bounds: Rectangle,
        padding: Padding,
        cursor: mouse::Cursor,
        key_binding: Option<&dyn Fn(KeyPress) -> Option<Binding<Message>>>,
    ) -> Option<Self> {
        let binding = |binding| Some(Update::Binding(binding));

        match event {
            Event::Mouse(event) => match event {
                mouse::Event::ButtonPressed(mouse::Button::Left) => {
                    if let Some(cursor_position) = cursor.position_in(bounds) {
                        let cursor_position =
                            cursor_position - Vector::new(padding.left, padding.top);

                        let click = mouse::Click::new(
                            cursor_position,
                            mouse::Button::Left,
                            state.last_click,
                        );

                        Some(Update::Click(click))
                    } else if state.focus.is_some() {
                        binding(Binding::Unfocus)
                    } else {
                        None
                    }
                }
                mouse::Event::ButtonReleased(mouse::Button::Left) => Some(Update::Release),
                mouse::Event::CursorMoved { .. } => match state.drag_click {
                    Some(mouse::click::Kind::Single) => {
                        let cursor_position =
                            cursor.position_in(bounds)? - Vector::new(padding.left, padding.top);

                        Some(Update::Drag(cursor_position))
                    }
                    _ => None,
                },
                mouse::Event::WheelScrolled { delta } if cursor.is_over(bounds) => {
                    Some(Update::Scroll(match delta {
                        mouse::ScrollDelta::Lines { y, .. } => {
                            if y.abs() > 0.0 {
                                y.signum() * -(y.abs() * 4.0).max(1.0)
                            } else {
                                0.0
                            }
                        }
                        mouse::ScrollDelta::Pixels { y, .. } => -y / 4.0,
                    }))
                }
                _ => None,
            },
            Event::InputMethod(event) => match event {
                input_method::Event::Opened | input_method::Event::Closed => Some(
                    Update::InputMethod(Ime::Toggle(matches!(event, input_method::Event::Opened))),
                ),
                input_method::Event::Preedit(content, selection) if state.focus.is_some() => {
                    Some(Update::InputMethod(Ime::Preedit {
                        content: content.clone(),
                        selection: selection.clone(),
                    }))
                }
                input_method::Event::Commit(content) if state.focus.is_some() => {
                    Some(Update::InputMethod(Ime::Commit(content.clone())))
                }
                _ => None,
            },
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modified_key,
                physical_key,
                modifiers,
                text,
                ..
            }) => {
                let status = if state.focus.is_some() {
                    Status::Focused {
                        is_hovered: cursor.is_over(bounds),
                    }
                } else {
                    Status::Active
                };

                let key_press = KeyPress {
                    key: key.clone(),
                    modified_key: modified_key.clone(),
                    physical_key: *physical_key,
                    modifiers: *modifiers,
                    text: text.clone(),
                    status,
                };

                if let Some(key_binding) = key_binding {
                    key_binding(key_press)
                } else {
                    Binding::from_key_press(key_press)
                }
                .map(Self::Binding)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ScrollGeometry {
    track: Rectangle,
    thumb: Rectangle,
    visible: f32,
    scrollable: bool,
}

impl ScrollGeometry {
    fn new(bounds: Rectangle, text: Rectangle, total: f32, first: f32, visible: f32) -> Self {
        let track = Rectangle {
            x: bounds.x + bounds.width - crate::design::SCROLLBAR_GUTTER
                + crate::design::SCROLLBAR_MARGIN,
            y: text.y,
            width: crate::design::SCROLLBAR_THUMB_WIDTH,
            height: text.height.max(0.0),
        };
        let height = (track.height * visible / total).max(24.0).min(track.height);
        let maximum = (total - visible).max(0.0);
        let fraction = if maximum > 0.0 {
            (first / maximum).clamp(0.0, 1.0)
        } else {
            0.0
        };
        Self {
            track,
            thumb: Rectangle {
                y: track.y + (track.height - height) * fraction,
                height,
                ..track
            },
            visible,
            scrollable: maximum > 0.0,
        }
    }

    fn fraction_at(self, y: f32, grab: f32) -> f32 {
        let travel = self.track.height - self.thumb.height;
        if travel > 0.0 {
            ((y - self.track.y - grab) / travel).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_drag_and_ime_paste_preserve_unselected_source_and_line_endings() {
        let mut content = Content::with_text("alpha beta\r\ngamma\n");
        layout_content(&mut content, 500.0, 200.0);
        content.perform(Action::Click(Point::new(0.0, 10.0)));
        content.perform(Action::Drag(Point::new(500.0, 10.0)));
        assert_eq!(content.selection().as_deref(), Some("alpha beta"));
        content.perform(Action::Edit(Edit::Paste(Arc::new("δ".into()))));
        assert_eq!(content.text(), "δ\r\ngamma\n");
    }

    #[test]
    fn source_rows_map_bare_carriage_returns_without_changing_saved_bytes() {
        let source = "one\rtwo\r\nthree\n";
        let content: Content = Content::with_text(source);
        assert_eq!(content.text(), source);
        assert_eq!(content.source_line_ranges(), vec![0..2, 2..3, 3..4]);
    }

    fn layout_content(content: &mut Content, width: f32, height: f32) {
        content.0.get_mut().editor.update(
            Size::new(width, height),
            cosmic::iced::Font::MONOSPACE,
            Pixels(16.0),
            LineHeight::Absolute(Pixels(20.0)),
            Wrapping::Word,
            &mut highlighter::PlainText::new(&()),
        );
    }

    fn seek_content(content: &mut Content, target: ScrollTarget, width: f32, height: f32) -> usize {
        let mut shapes = 0;
        let mut highlighter = highlighter::PlainText::new(&());
        seek_scroll(&mut content.0.get_mut().editor, target, |editor| {
            shapes += 1;
            editor.update(
                Size::new(width, height),
                cosmic::iced::Font::MONOSPACE,
                Pixels(16.0),
                LineHeight::Absolute(Pixels(20.0)),
                Wrapping::Word,
                &mut highlighter,
            );
        });
        shapes
    }

    #[test]
    fn scrollbar_follows_native_scroll_without_changing_selection_or_text() {
        let source = (1..=200)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let mut content = Content::with_text(&source);
        layout_content(&mut content, 500.0, 200.0);
        let before = content.cursor();
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(600.0, 210.0));
        let initial = {
            let editor: TextEditor<'_, highlighter::PlainText, ()> = TextEditor::new(&content);
            editor.scroll_geometry(bounds)
        };
        content.perform(Action::Scroll { lines: 80 });
        layout_content(&mut content, 500.0, 200.0);
        let editor: TextEditor<'_, highlighter::PlainText, ()> = TextEditor::new(&content);
        let scrolled = editor.scroll_geometry(bounds);
        assert!(scrolled.thumb.y > initial.thumb.y);
        assert_eq!(content.cursor(), before);
        assert_eq!(content.text(), source);
        drop(editor);
        seek_content(
            &mut content,
            ScrollTarget::Fraction {
                fraction: 0.0,
                visible: scrolled.visible,
            },
            500.0,
            200.0,
        );
        let editor: TextEditor<'_, highlighter::PlainText, ()> = TextEditor::new(&content);
        assert_eq!(editor.scroll_geometry(bounds).thumb.y, initial.thumb.y);
    }

    #[test]
    fn wrapped_single_line_can_be_scrolled_and_keeps_its_logical_line_number() {
        let mut content = Content::with_text(&"wrapped words ".repeat(200));
        layout_content(&mut content, 100.0, 100.0);
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(180.0, 110.0));
        let editor: TextEditor<'_, highlighter::PlainText, ()> = TextEditor::new(&content);
        let geometry = editor.scroll_geometry(bounds);
        assert!(geometry.scrollable);
        drop(editor);
        seek_content(
            &mut content,
            ScrollTarget::Fraction {
                fraction: 0.5,
                visible: geometry.visible,
            },
            100.0,
            100.0,
        );
        let internal = content.0.borrow();
        let buffer = internal.editor.buffer();
        assert!(buffer.scroll().vertical > 0.0);
        assert!(buffer.layout_runs().all(|run| run.line_i == 0));
    }

    #[test]
    fn first_thumb_drag_reaches_a_distant_unshaped_wrapped_line_without_moving_selection() {
        let source = (0..1000)
            .map(|line| format!("{line}: {}\n", "wrapped words ".repeat(32)))
            .collect::<String>();
        let mut content = Content::with_text(&source);
        content.move_to(Cursor {
            position: Position { line: 0, column: 8 },
            selection: Some(Position { line: 0, column: 3 }),
        });
        layout_content(&mut content, 180.0, 200.0);
        let cursor = content.cursor();
        let selection = content.selection();
        let geometry = {
            let editor: TextEditor<'_, highlighter::PlainText, ()> = TextEditor::new(&content);
            editor.scroll_geometry(Rectangle::new(Point::ORIGIN, Size::new(260.0, 210.0)))
        };
        let target = ScrollTarget::Fraction {
            fraction: 0.5,
            visible: geometry.visible,
        };
        let target_line = ((content.line_count() as f32 - geometry.visible) * 0.5).floor() as usize;
        assert!(
            content.0.borrow().editor.buffer().lines[target_line]
                .layout_opt()
                .is_none()
        );
        let started = Instant::now();
        let shapes = seek_content(&mut content, target, 180.0, 200.0);
        eprintln!(
            "first distant wrapped seek: {shapes} shape passes in {:?}",
            started.elapsed()
        );
        let internal = content.0.borrow();
        let buffer = internal.editor.buffer();
        assert_eq!(buffer.scroll().line, target_line);
        assert_eq!(
            buffer.layout_runs().next().map(|run| run.line_i),
            Some(target_line)
        );
        assert!(
            buffer.lines[900].layout_opt().is_none(),
            "seeking must not shape the unvisited tail"
        );
        drop(internal);
        assert_eq!(content.cursor(), cursor);
        assert_eq!(content.selection(), selection);
        assert_eq!(content.text(), source);
        assert!(
            shapes <= target_line + 2,
            "every intermediate seek step must cross a logical line"
        );
    }

    #[test]
    fn logical_scroll_anchors_restore_wrapped_rows_without_moving_selection() {
        let source = (0..300)
            .map(|line| format!("{line}: {}\n", "wrapped words ".repeat(24)))
            .collect::<String>();
        let mut content = Content::with_text(&source);
        content.move_to(Cursor {
            position: Position { line: 0, column: 8 },
            selection: Some(Position { line: 0, column: 3 }),
        });
        layout_content(&mut content, 180.0, 200.0);
        let cursor = content.cursor();
        let selection = content.selection();
        let anchor = ScrollPosition {
            line: 120,
            vertical: 60.0,
            horizontal: 0.0,
        };
        seek_content(&mut content, ScrollTarget::Position(anchor), 180.0, 200.0);
        assert_eq!(content.scroll_position(), anchor);
        seek_content(
            &mut content,
            ScrollTarget::Fraction {
                fraction: 0.0,
                visible: 10.0,
            },
            180.0,
            200.0,
        );
        seek_content(&mut content, ScrollTarget::Position(anchor), 180.0, 200.0);
        assert_eq!(content.scroll_position(), anchor);
        assert_eq!(content.cursor(), cursor);
        assert_eq!(content.selection(), selection);
        assert_eq!(content.text(), source);
    }

    #[test]
    fn horizontal_restore_keeps_glyph_hit_testing_caret_and_wrapping_in_agreement() {
        use cosmic::iced::advanced::renderer::Headless;
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(cosmic::Renderer::new(
                cosmic::iced::Font::MONOSPACE,
                Pixels(16.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let layout = |content: &Content, wrapping| {
            let mut widget: TextEditor<'_, highlighter::PlainText, ()> = TextEditor::new(content)
                .font(cosmic::iced::Font::MONOSPACE)
                .size(16.0)
                .wrapping(wrapping)
                .height(Length::Fill);
            let mut tree =
                widget::Tree::new(&widget as &dyn Widget<(), cosmic::Theme, cosmic::Renderer>);
            widget.layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(260.0, 200.0)),
            );
        };
        let assert_damage_covers_viewport = |content: &Content| {
            let widget: TextEditor<'_, highlighter::PlainText, ()> =
                TextEditor::new(content).size(16.0);
            let clip = Rectangle::new(Point::ORIGIN, Size::new(260.0, 200.0))
                .shrink(widget.text_padding());
            let internal = content.0.borrow();
            let text = cosmic::iced::advanced::graphics::Text::Editor {
                editor: internal.editor.downgrade(),
                position: clip.position() + Vector::new(internal.horizontal_translation(), 0.0),
                color: cosmic::iced::Color::BLACK,
                clip_bounds: clip,
                transformation: Default::default(),
            };
            assert_eq!(text.visible_bounds(), Some(clip));
        };
        let source = "abcdefghij".repeat(30);
        let mut original = Content::with_text(&source);
        original.move_to(Cursor {
            position: Position {
                line: 0,
                column: 100,
            },
            selection: None,
        });
        layout(&original, Wrapping::None);
        let anchor = original.scroll_position();
        assert!(
            anchor.horizontal > 260.0,
            "caret-follow must pan beyond one viewport"
        );

        let mut restored = Content::with_text(&source);
        restored.move_to(Cursor {
            position: Position { line: 0, column: 3 },
            selection: Some(Position { line: 0, column: 0 }),
        });
        let cursor = restored.cursor();
        let selection = restored.selection();
        restored.restore_scroll_position(anchor);
        layout(&restored, Wrapping::None);
        assert_eq!(restored.scroll_position(), anchor);
        assert_damage_covers_viewport(&restored);
        assert_eq!(restored.cursor(), cursor);
        assert_eq!(restored.selection(), selection);
        layout(&restored, Wrapping::WordOrGlyph);
        assert_eq!(restored.0.borrow().horizontal_translation(), 0.0);
        layout(&restored, Wrapping::None);
        assert_eq!(restored.scroll_position(), anchor);

        let (column, document_x, pointer_x, end_column, drag_x) = {
            let internal = restored.0.borrow();
            let run = internal.editor.buffer().layout_runs().next().unwrap();
            let glyph = run
                .glyphs
                .iter()
                .find(|glyph| glyph.x > anchor.horizontal + 20.0)
                .unwrap();
            let end = run
                .glyphs
                .iter()
                .find(|end| end.start >= glyph.start + 3)
                .unwrap();
            (
                glyph.start,
                glyph.x,
                glyph.x + glyph.w * 0.25 - anchor.horizontal,
                end.start,
                end.x + end.w * 0.25 - anchor.horizontal,
            )
        };
        restored.perform(Action::Click(Point::new(pointer_x, 5.0)));
        layout(&restored, Wrapping::None);
        assert_eq!(restored.cursor().position.column, column);
        assert_eq!(
            restored.scroll_position(),
            anchor,
            "click must not jump the restored viewport"
        );
        let internal = restored.0.borrow();
        let Selection::Caret(caret) = internal.selection() else {
            panic!("click must leave a caret");
        };
        assert!((caret.x - document_x).abs() < 0.01);
        assert!(
            (caret.x + internal.horizontal_translation() - (document_x - anchor.horizontal)).abs()
                < 0.01
        );
        drop(internal);
        restored.perform(Action::Drag(Point::new(drag_x, 5.0)));
        layout(&restored, Wrapping::None);
        assert_eq!(
            restored.selection().as_deref(),
            Some(&source[column..end_column])
        );
        assert_eq!(
            restored.scroll_position(),
            anchor,
            "drag must keep its starting viewport"
        );
        restored.perform(Action::ClearSelection);
        restored.perform(Action::Edit(Edit::Insert('Z')));
        layout(&restored, Wrapping::None);
        assert_eq!(
            restored.scroll_position(),
            anchor,
            "editing a visible caret must not jump X"
        );
        assert_damage_covers_viewport(&restored);
        let mut edited = source.clone();
        edited.insert(end_column, 'Z');
        restored.perform(Action::Move(Motion::Home));
        layout(&restored, Wrapping::None);
        assert_eq!(
            restored.scroll_position().horizontal,
            0.0,
            "keyboard navigation reveals the caret"
        );
        assert_eq!(restored.text(), edited);
        restored.restore_scroll_position(ScrollPosition {
            line: 0,
            vertical: f32::INFINITY,
            horizontal: f32::NAN,
        });
        layout(&restored, Wrapping::None);
        assert_eq!(restored.scroll_position(), ScrollPosition::default());
    }

    #[test]
    fn dragging_short_viewports_is_bounded_and_fit_content_is_inert() {
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(200.0, 20.0));
        let fit = ScrollGeometry::new(bounds, bounds, 1.0, 0.0, 1.0);
        assert!(!fit.scrollable);
        assert_eq!(fit.fraction_at(500.0, 0.0), 0.0);
        assert!(fit.thumb.height <= bounds.height);
        let bounds = Rectangle {
            height: 200.0,
            ..bounds
        };
        let scrolling = ScrollGeometry::new(bounds, bounds, 100.0, 40.0, 10.0);
        assert_eq!(scrolling.fraction_at(-500.0, 0.0), 0.0);
        assert_eq!(scrolling.fraction_at(500.0, 0.0), 1.0);
    }
}
