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
use crate::editor_history::{self as history, Excerpt, TextChange};
use cosmic::iced::advanced::text::{
    editor::Editor as _,
    highlighter::{self, Highlighter},
};
use cosmic::iced::advanced::{
    InputMethod, Shell,
    clipboard::{self, Clipboard},
    input_method,
    layout::{self, Layout},
    mouse, overlay, renderer,
    text::{self, Ellipsize, LineHeight, Text, Wrapping},
    widget::{self, Widget, operation},
};
pub use cosmic::iced::widget::text_editor::{
    Action, Binding, Cursor, Edit, KeyPress, Line, LineEnding, Motion, Position, Selection,
};
use cosmic::iced::widget::text_editor::{Catalog, Status, Style, StyleFn};
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

#[path = "editor_widget/snapshot.rs"]
mod snapshot;
#[cfg(test)]
pub use snapshot::SnapshotMemory;
pub use snapshot::TextSnapshot;
#[path = "editor_widget/match_index.rs"]
mod match_index;
pub(crate) use match_index::MatchBuilder;
pub use match_index::{MatchIndex, Matches};

#[cfg(test)]
thread_local! { static TEXT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
pub(crate) fn take_text_call_count() -> usize {
    TEXT_CALLS.with(|count| count.replace(0))
}

/// A diagnostic mapped once to the renderer's byte-addressed line positions.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub start: Position,
    pub end: Position,
    pub severity: crate::lsp::LspSeverity,
}

/// Immutable native lines captured in constant time, without taking a renderer
/// or font-system lock. Search walks these lines on its background worker;
/// only an explicit replacement needs the complete source string.
#[derive(Debug)]
pub(crate) struct SearchSnapshot {
    source: TextSnapshot,
}

impl SearchSnapshot {
    fn lines(&self) -> impl Iterator<Item = Cow<'_, str>> {
        (0..self.source.line_count())
            .filter_map(|index| self.source.line(index).map(|line| line.text))
    }

    pub(crate) fn find_matches(
        &self,
        query: &str,
        cancelled: impl Fn() -> bool,
    ) -> Option<MatchIndex> {
        find_in_lines(self.lines(), query, cancelled)
    }

    pub(crate) fn find_matches_with(
        &self,
        query: &str,
        options: crate::editor_search::FindOptions,
        cancelled: impl Fn() -> bool,
    ) -> Result<Option<MatchIndex>, String> {
        crate::editor_search::find_in_lines(self.lines(), query, options, cancelled)
    }

    /// Validates sparse replacements against one immutable revision before
    /// any native mutation. Higher ranges can then be applied first.
    pub(crate) fn replacement_edits(
        &self,
        matches: impl IntoIterator<Item = impl std::borrow::Borrow<(Position, Position)>>,
        query: &str,
        replacement: &str,
        options: crate::editor_search::FindOptions,
        preserve: bool,
    ) -> Result<Vec<(Position, Position, String)>, String> {
        self.replacement_edits_bounded(
            matches,
            query,
            replacement,
            options,
            preserve,
            crate::editor_search::replacement_budget(self.source.len()),
        )
    }

    fn replacement_edits_bounded(
        &self,
        matches: impl IntoIterator<Item = impl std::borrow::Borrow<(Position, Position)>>,
        query: &str,
        replacement: &str,
        options: crate::editor_search::FindOptions,
        preserve: bool,
        limit: usize,
    ) -> Result<Vec<(Position, Position, String)>, String> {
        let replacement =
            crate::editor_search::Replacement::new(query, replacement, options, preserve)?;
        let matches = matches.into_iter();
        let mut edits = Vec::with_capacity(matches.size_hint().0);
        let mut copied = 0;
        let mut output_bytes = 0;
        for item in matches {
            let &(start, end) = std::borrow::Borrow::borrow(&item);
            if start.line != end.line {
                return Err("A search match crosses a line boundary.".into());
            }
            let line = self
                .source
                .line(start.line)
                .ok_or("The selected match is outside the document.")?;
            let from = self
                .source
                .byte_offset(start)
                .ok_or("Invalid search position.")?;
            let to = self
                .source
                .byte_offset(end)
                .ok_or("Invalid search position.")?;
            if to < from || from < copied {
                return Err("Search matches overlap.".into());
            }
            output_bytes =
                crate::editor_search::checked_output_size(output_bytes, from - copied, limit)?;
            let inserted = replacement.expand_bounded(
                &line.text,
                start.column,
                end.column,
                limit - output_bytes,
            )?;
            output_bytes =
                crate::editor_search::checked_output_size(output_bytes, inserted.len(), limit)?;
            if line.text.get(start.column..end.column) != Some(inserted.as_str()) {
                edits.push((start, end, inserted));
            }
            copied = to;
        }
        crate::editor_search::checked_output_size(output_bytes, self.source.len() - copied, limit)?;
        Ok(edits)
    }

    pub(crate) fn replace_matches_with(
        &self,
        matches: impl IntoIterator<Item = impl std::borrow::Borrow<(Position, Position)>>,
        query: &str,
        replacement: &str,
        options: crate::editor_search::FindOptions,
        preserve: bool,
    ) -> Result<String, String> {
        self.replace_matches_with_cancellation(
            matches,
            query,
            replacement,
            options,
            preserve,
            || false,
        )
    }

    pub(crate) fn replace_matches_with_cancellation(
        &self,
        matches: impl IntoIterator<Item = impl std::borrow::Borrow<(Position, Position)>>,
        query: &str,
        replacement: &str,
        options: crate::editor_search::FindOptions,
        preserve: bool,
        cancelled: impl Fn() -> bool,
    ) -> Result<String, String> {
        self.replace_matches_bounded(
            matches,
            query,
            replacement,
            options,
            preserve,
            cancelled,
            crate::editor_search::replacement_budget(self.source.len()),
        )
    }

    fn replace_matches_bounded(
        &self,
        matches: impl IntoIterator<Item = impl std::borrow::Borrow<(Position, Position)>>,
        query: &str,
        replacement: &str,
        options: crate::editor_search::FindOptions,
        preserve: bool,
        cancelled: impl Fn() -> bool,
        limit: usize,
    ) -> Result<String, String> {
        if cancelled() {
            return Err("Replacement cancelled.".into());
        }
        let replacement =
            crate::editor_search::Replacement::new(query, replacement, options, preserve)?;
        let text = self.source.text();
        let mut result = String::new();
        let mut copied = 0;
        let mut cached_line: Option<(usize, Line<'_>, usize)> = None;
        for (index, item) in matches.into_iter().enumerate() {
            if index % 256 == 0 && cancelled() {
                return Err("Replacement cancelled.".into());
            }
            let &(start, end) = std::borrow::Borrow::borrow(&item);
            if start.line != end.line {
                return Err("A search match crosses a line boundary.".into());
            }
            if cached_line
                .as_ref()
                .is_none_or(|(number, _, _)| *number != start.line)
            {
                let line = self
                    .source
                    .line(start.line)
                    .ok_or("The selected match is outside the document.")?;
                let offset = self
                    .source
                    .byte_offset(Position {
                        line: start.line,
                        column: 0,
                    })
                    .ok_or("Invalid search position.")?;
                cached_line = Some((start.line, line, offset));
            }
            let (_, line, offset) = cached_line.as_ref().unwrap();
            line.text
                .get(start.column..end.column)
                .ok_or("Invalid search position.")?;
            let from = offset + start.column;
            let to = offset + end.column;
            if to < from || from < copied {
                return Err("Search matches overlap.".into());
            }
            crate::editor_search::append_output(
                &mut result,
                text.get(copied..from).ok_or("Invalid search position.")?,
                limit,
            )?;
            replacement.append_bounded(&line.text, start.column, end.column, &mut result, limit)?;
            copied = to;
        }
        crate::editor_search::append_output(
            &mut result,
            text.get(copied..).ok_or("Invalid search position.")?,
            limit,
        )?;
        Ok(result)
    }

    /// Build a replacement in source order: each source byte is copied once.
    pub(crate) fn replace_matches(
        &self,
        matches: impl IntoIterator<Item = impl std::borrow::Borrow<(Position, Position)>>,
        replacement: &str,
    ) -> Option<String> {
        let text = self.source.text();
        let mut result = String::with_capacity(text.len());
        let mut copied = 0;
        for item in matches {
            let &(start, end) = std::borrow::Borrow::borrow(&item);
            let start = self.source.byte_offset(start)?;
            let end = self.source.byte_offset(end)?;
            if end < start || start < copied {
                return None;
            }
            result.push_str(text.get(copied..start)?);
            result.push_str(replacement);
            copied = end;
        }
        result.push_str(text.get(copied..)?);
        Some(result)
    }
}

pub(crate) fn find_in_lines<T: AsRef<str>>(
    lines: impl Iterator<Item = T>,
    query: &str,
    cancelled: impl Fn() -> bool,
) -> Option<MatchIndex> {
    let mut matches = MatchBuilder::default();
    visit_matches(lines, query, cancelled, |line, _, start, end| {
        matches.push(
            Position {
                line,
                column: start,
            },
            Position { line, column: end },
        )
    })
    .then(|| matches.finish())
}

pub(crate) fn visit_matches<T: AsRef<str>>(
    lines: impl Iterator<Item = T>,
    query: &str,
    cancelled: impl Fn() -> bool,
    mut emit: impl FnMut(usize, &str, usize, usize),
) -> bool {
    use unicode_casefold::UnicodeCaseFold;
    if query.is_empty() {
        return true;
    }
    let query: String = query.case_fold().collect();
    // Reuse scratch space rather than allocating two buffers per source line.
    let mut folded = String::new();
    let mut boundaries = Vec::new();
    for (line, text) in lines.enumerate() {
        let text = text.as_ref();
        if cancelled() {
            return false;
        }
        folded.clear();
        boundaries.clear();
        if text.is_ascii() {
            // ASCII case folding preserves every byte boundary. Most source
            // lines take this path and need no boundary map at all.
            folded.push_str(text);
            folded.make_ascii_lowercase();
            for (index, (offset, _)) in folded.match_indices(&query).enumerate() {
                if index % 1024 == 0 && cancelled() {
                    return false;
                }
                emit(line, text, offset, offset + query.len());
            }
            continue;
        }
        for (index, (offset, character)) in text.char_indices().enumerate() {
            if index % 1024 == 0 && cancelled() {
                return false;
            }
            let start = folded.len();
            folded.extend(std::iter::once(character).case_fold());
            boundaries.resize(folded.len(), None);
            // NonZero's niche keeps each optional source boundary one
            // machine word, rather than a separate usize plus discriminant.
            boundaries[start] = std::num::NonZeroUsize::new(offset + 1);
        }
        boundaries.push(std::num::NonZeroUsize::new(text.len() + 1));
        let mut cursor = 0;
        let mut candidates = 0;
        while cursor + query.len() <= folded.len() {
            if candidates % 1024 == 0 && cancelled() {
                return false;
            }
            candidates += 1;
            // Reject impossible source boundaries before building a substring
            // searcher, including repeated partial expansion hits on long lines.
            if boundaries[cursor].is_none() || boundaries[cursor + query.len()].is_none() {
                cursor += folded[cursor..].chars().next().unwrap().len_utf8();
                continue;
            }
            let Some(found) = folded[cursor..].find(&query) else {
                break;
            };
            let offset = cursor + found;
            let end = offset + query.len();
            if let (Some(start), Some(end_column)) = (boundaries[offset], boundaries[end]) {
                emit(line, text, start.get() - 1, end_column.get() - 1);
                cursor = end;
            } else {
                // Rejected partial case-fold expansions must not consume a
                // later whole-character match that overlaps their folded bytes.
                cursor = offset + folded[offset..].chars().next().unwrap().len_utf8();
            }
        }
    }
    true
}
type NativeEditor = <cosmic::Renderer as text::Renderer>::Editor;

/// Each fragment ends at one native line boundary. Recorded boundaries can
/// intentionally separate CR and LF that a fresh parser would pair together.
fn paste_fragments<'a>(
    text: &'a str,
    breaks: Option<&'a [usize]>,
) -> impl Iterator<Item = &'a str> {
    use cosmic::iced::advanced::graphics::text::cosmic_text;
    let mut parsed = cosmic_text::LineIter::new(text);
    let mut recorded = breaks
        .unwrap_or_default()
        .iter()
        .copied()
        .chain([text.len()]);
    let mut previous = 0;
    std::iter::from_fn(move || {
        if breaks.is_none() {
            return parsed
                .next()
                .map(|(range, ending)| &text[range.start..range.end + ending.as_str().len()]);
        }
        loop {
            let end = recorded.next()?;
            if end > previous {
                let fragment = &text[previous..end];
                previous = end;
                return Some(fragment);
            }
        }
    })
}

fn append_fragment(editor: &mut NativeEditor, text: &str, ending: &str) {
    if text.is_empty() && ending.is_empty() {
        return;
    }
    let mut fragment = String::with_capacity(text.len() + ending.len());
    fragment.push_str(text);
    fragment.push_str(ending);
    editor.perform(Action::Edit(Edit::Paste(Arc::new(fragment))));
}

fn mark_changed_from(editor: &mut NativeEditor, position: Position) {
    let cursor = editor.cursor();
    editor.move_to(Cursor {
        position,
        selection: None,
    });
    editor.perform(Action::Edit(Edit::Paste(Arc::new(String::new()))));
    editor.move_to(cursor);
}
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
    diagnostics: &'a [Diagnostic],
    popup: Option<Element<'a, Message, Theme, Renderer>>,
    popup_at: Option<Position>,
    on_hover: Option<Box<dyn Fn(Option<Position>) -> Message + 'a>>,
    on_definition: Option<Box<dyn Fn(Position) -> Message + 'a>>,
    find_matches: Option<&'a MatchIndex>,
    current_match: usize,
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
            diagnostics: &[],
            popup: None,
            popup_at: None,
            on_hover: None,
            on_definition: None,
            find_matches: None,
            current_match: 0,
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

    /// Zero-based source lines the unsaved buffer has added, sorted once when
    /// the working diff completes.
    pub fn added_lines(mut self, additions: &'a [usize]) -> Self {
        self.additions = additions;
        self
    }

    pub fn diagnostics(mut self, diagnostics: &'a [Diagnostic]) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    pub fn popup(
        mut self,
        popup: Option<Element<'a, Message, Theme, Renderer>>,
        at: Option<Position>,
    ) -> Self {
        self.popup = popup;
        self.popup_at = at;
        self
    }

    pub fn on_hover(mut self, on_hover: impl Fn(Option<Position>) -> Message + 'a) -> Self {
        self.on_hover = Some(Box::new(on_hover));
        self
    }

    pub fn on_definition(mut self, on_definition: impl Fn(Position) -> Message + 'a) -> Self {
        self.on_definition = Some(Box::new(on_definition));
        self
    }

    /// Sets the [`Font`] of the [`TextEditor`].
    ///
    /// [`Font`]: text::Renderer::Font
    pub fn font(mut self, font: impl Into<Renderer::Font>) -> Self {
        self.font = Some(font.into());
        self
    }

    pub fn find_matches(mut self, matches: Option<&'a MatchIndex>, current: usize) -> Self {
        self.find_matches = matches;
        self.current_match = current;
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
            diagnostics: self.diagnostics,
            popup: self.popup,
            popup_at: self.popup_at,
            on_hover: self.on_hover,
            on_definition: self.on_definition,
            find_matches: self.find_matches,
            current_match: self.current_match,
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
        let (canvas_width, offset) = if internal.wrapping == Wrapping::None {
            (internal.canvas_width, internal.horizontal)
        } else {
            // Wrapped text has no canvas wider than its viewport, so there is
            // nothing to pan and nothing to draw a track for.
            (0.0, 0.0)
        };
        ScrollGeometry::new(
            bounds,
            text_bounds,
            buffer.lines.len().max(1) as f32,
            first,
            visible,
            canvas_width,
            offset,
        )
    }

    /// Pans the unwrapped canvas by `delta` logical pixels, answering whether
    /// the offset moved.
    ///
    /// The reachable range is the canvas the layout pass has already measured,
    /// which covers every line the viewport has shown. Offering more would
    /// mean shaping the document's unvisited lines just to learn how wide they
    /// are, which is exactly what the canvas exists to avoid.
    fn pan_horizontally(&self, bounds: Rectangle, delta: f32) -> bool {
        if self.wrapping != Wrapping::None || !delta.is_finite() {
            return false;
        }
        let viewport = bounds.shrink(self.text_padding());
        let mut internal = self.content.0.borrow_mut();
        let maximum = (internal.canvas_width - viewport.width).max(0.0);
        let horizontal = (internal.horizontal + delta).clamp(0.0, maximum);
        if horizontal == internal.horizontal {
            return false;
        }
        internal.horizontal = horizontal;
        // The caret did not move, so nothing should pull the view back to it.
        internal.reveal_horizontal = false;
        true
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

/// Ctrl+Delete / Ctrl+Backspace delete the adjacent word, then the usual
/// iced editor chords (copy/cut/paste/select-all and caret motion).
pub fn default_bindings<Message>(press: KeyPress) -> Option<Binding<Message>> {
    // Every editor on screen sees every key press. Only the focused one may
    // act on it, or Ctrl+Backspace typed anywhere else would delete a word
    // from each unfocused buffer too.
    if !matches!(press.status, Status::Focused { .. }) {
        return None;
    }
    if press.modifiers.command() && !press.modifiers.alt() && !press.modifiers.shift() {
        match press.key.as_ref() {
            keyboard::Key::Named(keyboard::key::Named::Delete) => {
                return Some(Binding::Sequence(vec![
                    Binding::Select(Motion::WordRight),
                    Binding::Delete,
                ]));
            }
            keyboard::Key::Named(keyboard::key::Named::Backspace) => {
                return Some(Binding::Sequence(vec![
                    Binding::Select(Motion::WordLeft),
                    Binding::Delete,
                ]));
            }
            _ => {}
        }
    }
    Binding::from_key_press(press)
}

/// A step through the edit history that a chord asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryCommand {
    Undo,
    Redo,
}

/// Recognizes Ctrl+Z as undo, and both Ctrl+Shift+Z and Ctrl+Y as redo, in
/// the focused editor.
///
/// Letters are read through the physical key as well, so the chords stay
/// where they are on a non-Latin keyboard layout.
pub fn history_command(press: &KeyPress) -> Option<HistoryCommand> {
    if !matches!(press.status, Status::Focused { .. })
        || !press.modifiers.command()
        || press.modifiers.alt()
    {
        return None;
    }
    match press.key.to_latin(press.physical_key)?.to_ascii_lowercase() {
        'z' if press.modifiers.shift() => Some(HistoryCommand::Redo),
        'z' => Some(HistoryCommand::Undo),
        'y' if !press.modifiers.shift() => Some(HistoryCommand::Redo),
        _ => None,
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

/// Tolerance absorbing f32 residue where a captured offset meets a visual-row
/// boundary: our own captures read back ~1e-4px below exact row multiples,
/// and flooring that noise aims a whole line off and traps the seek in a
/// two-position oscillation. Hundreds of times below a visible pixel.
const SEEK_ROW_EPSILON: f32 = 0.05;

/// Canonicalize a scroll position so captures round-trip: scrolling by lines
/// accumulates ~1e-4px residue that flips line attribution at boundaries
/// (reporting line 24 + 19.5999px instead of line 25 + 0px for the same
/// viewport). Snapping sub-pixel straddles to the boundary they round to
/// keeps restore, capture, and read-back on one stable representation;
/// genuine mid-row offsets are untouched.
fn canonical_scroll(
    position: ScrollPosition,
    line_height: f32,
    single_row: bool,
) -> ScrollPosition {
    let line_height = line_height.max(1.0);
    let mut line = position.line;
    let mut vertical = position.vertical;
    if vertical.is_finite() && vertical >= 0.0 {
        if single_row {
            // No visual rows to index: a whole line height of offset is
            // smooth scroll resting between lines — carry it into the line.
            let carry = ((vertical + SEEK_ROW_EPSILON) / line_height)
                .floor()
                .max(0.0);
            line = line.saturating_add(carry as usize);
            vertical -= carry * line_height;
            if vertical.abs() < SEEK_ROW_EPSILON {
                vertical = 0.0;
            }
        } else {
            // Wrapped lines: the offset indexes visual rows, so only absorb
            // residue onto the row grid and never carry into the line.
            let row = vertical / line_height;
            let snapped = row.round();
            if (row - snapped).abs() * line_height < SEEK_ROW_EPSILON {
                vertical = snapped * line_height;
            }
        }
    }
    ScrollPosition {
        line,
        vertical,
        horizontal: position.horizontal,
    }
}

/// The native renderer exposes relative visual-row scrolling, not a mutable
/// buffer or logical seek. Shape and observe each step until the logical target
/// is reached. Each step consumes at least the rest of the current wrapped line
/// without scanning unshaped prefixes or moving the caret/selection.
fn seek_scroll(
    editor: &mut NativeEditor,
    target: ScrollTarget,
    wrapping: Wrapping,
    mut shape: impl FnMut(&mut NativeEditor),
) {
    let line_height = editor.buffer().metrics().line_height.max(1.0);
    // Canonicalize the target the same way captures are canonicalized: a
    // vertical offset at or past a full line height addresses the following
    // line, and without the carry the seek aims a whole line above the
    // captured viewport and then oscillates against it.
    let target = match target {
        ScrollTarget::Position(position) => {
            let single_row = wrapping == Wrapping::None;
            ScrollTarget::Position(canonical_scroll(position, line_height, single_row))
        }
        other => other,
    };
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
        // Shaping is lazy: lines below the shaped frontier have no layout yet.
        // Stepping estimates can assume a single row, but the target row must
        // not clamp to unknown information — an unshaped wrapped line reads
        // as one row and would pin every restore to its top.
        let shaped_rows = buffer
            .lines
            .get(scroll.line)
            .and_then(|line| line.layout_opt())
            .map(Vec::len);
        let rows = shaped_rows.unwrap_or(1).max(1) as f32;
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
            let mut target_row = match target {
                ScrollTarget::Fraction { .. } => position.fract() * rows,
                ScrollTarget::Position(position) => {
                    (position.vertical + SEEK_ROW_EPSILON) / line_height
                }
            }
            .floor();
            if let Some(shaped) = shaped_rows {
                target_row = target_row.min(shaped.max(1) as f32 - 1.0);
            }
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

/// Give a fresh buffer real bounds and the prior viewport before native
/// hit-testing or caret reveal. The final caret remains pending, just as after
/// native Paste; queued Scroll/Click/Drag therefore retain native ordering.
fn seed_splice_viewport(
    editor: &mut NativeEditor,
    previous: ScrollPosition,
    wrapping: Wrapping,
    mut shape: impl FnMut(&mut NativeEditor),
) {
    let cursor = editor.cursor();
    editor.move_to(Cursor {
        position: Position {
            line: previous.line.min(editor.line_count().saturating_sub(1)),
            column: 0,
        },
        selection: None,
    });
    shape(editor);
    seek_scroll(
        editor,
        ScrollTarget::Position(previous),
        wrapping,
        &mut shape,
    );
    // None preserves the native selection mode and anchor. Restoring the
    // reported Some(anchor) would turn a Word/Line selection into Normal.
    editor.move_to(Cursor {
        position: cursor.position,
        selection: None,
    });
}

fn shape_after_splice(
    editor: &mut NativeEditor,
    previous: Option<ScrollPosition>,
    scroll_lines: i32,
    wrapping: Wrapping,
    mut shape: impl FnMut(&mut NativeEditor),
) {
    if let Some(previous) = previous {
        seed_splice_viewport(editor, previous, wrapping, &mut shape);
        if scroll_lines != 0 {
            editor.perform(Action::Scroll {
                lines: scroll_lines,
            });
        }
    }
    shape(editor);
}

#[derive(Clone, Copy)]
struct EditorLayout {
    bounds: Size,
    font: cosmic::iced::Font,
    size: Pixels,
    line_height: LineHeight,
    wrapping: Wrapping,
}

impl EditorLayout {
    fn prepare(&self, editor: &mut NativeEditor, previous: ScrollPosition) {
        seed_splice_viewport(editor, previous, self.wrapping, |editor| {
            editor.update(
                self.bounds,
                self.font,
                self.size,
                self.line_height,
                self.wrapping,
                &mut highlighter::PlainText::new(&()),
            );
        });
        // PlainText consumed native's changed-line notification. The actual
        // widget highlighter must still invalidate its full prior document.
        mark_changed_from(editor, Position { line: 0, column: 0 });
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
    snapshot: TextSnapshot,
    snapshot_edit_depth: usize,
    layout: Option<EditorLayout>,
    reset_highlighter: bool,
    pending_scroll: Option<ScrollPosition>,
    // A rebuilt buffer first needs the old viewport, then ordinary caret
    // reveal. Seeding this after real metrics are known avoids eager shaping.
    pending_edit_scroll: Option<ScrollPosition>,
    pending_edit_scroll_lines: i32,
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
    fn finish_layout(&mut self, highlighter: &mut impl Highlighter) {
        // Native edits overwrite their changed-line marker. A queued small
        // edit must not hide the full invalidation of a rebuilt document.
        if std::mem::take(&mut self.reset_highlighter) {
            highlighter.change_line(0);
        }
    }

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
        // The native constructor shapes all text before it has a viewport.
        // Insert native line fragments instead: Paste does not shape, and
        // appending one fragment avoids its quadratic interior Vec insertions
        // for a large multiline paste. Use the same parser as Buffer::set_text
        // so mixed and paired line endings retain their exact native identity.
        let mut editor = R::Editor::with_text("");
        for fragment in paste_fragments(text, None) {
            append_fragment(&mut editor, fragment, "");
        }
        editor.perform(Action::ClearSelection);
        editor.move_to(Cursor {
            position: Position { line: 0, column: 0 },
            selection: None,
        });
        // Native edits remember the latest changed line for highlighting.
        // Renew that marker at the start after the append-only load, without
        // changing text, so the first viewport highlights from line zero.
        mark_changed_from(&mut editor, Position { line: 0, column: 0 });
        let count = editor.line_count();
        let snapshot = TextSnapshot::from_lines((0..count).map(|index| {
            let ending = editor.line(index).expect("native line exists").ending;
            (
                editor.buffer().lines[index].text(),
                ending,
                terminator(ending, index + 1 == count),
            )
        }));
        Self(RefCell::new(Internal {
            editor,
            snapshot,
            snapshot_edit_depth: 0,
            layout: None,
            reset_highlighter: true,
            pending_scroll: None,
            pending_edit_scroll: None,
            pending_edit_scroll_lines: 0,
            horizontal: 0.0,
            reveal_horizontal: true,
            canvas_width: 0.0,
            wrapping: Wrapping::None,
        }))
    }

    /// Performs an [`Action`] on the [`Content`].
    pub fn perform(&mut self, action: Action) {
        let editing = matches!(action, Action::Edit(_));
        let window = (editing && self.0.borrow().snapshot_edit_depth == 0)
            .then(|| self.snapshot_edit_window());
        if editing {
            self.0.get_mut().snapshot_edit_depth += 1;
        }
        self.perform_native(action);
        if editing {
            self.0.get_mut().snapshot_edit_depth -= 1;
        }
        if let Some(window) = window {
            self.refresh_snapshot(window);
        }
    }

    fn perform_native(&mut self, mut action: Action) {
        match &action {
            Action::Edit(Edit::Paste(text)) if self.try_splice(text, None) => return,
            Action::Edit(Edit::Delete | Edit::Backspace)
                if self.cursor().selection.is_some() && self.try_splice("", None) =>
            {
                return;
            }
            _ => {}
        }
        let internal = self.0.get_mut();
        internal.pending_scroll = None;
        if let Action::Scroll { lines } = action
            && internal.pending_edit_scroll.is_some()
        {
            internal.pending_edit_scroll_lines =
                internal.pending_edit_scroll_lines.saturating_add(lines);
            return;
        }
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

    fn snapshot_edit_window(&self) -> (usize, usize, usize) {
        let cursor = self.cursor();
        let anchor = cursor.selection.unwrap_or(cursor.position);
        let count = self.line_count();
        (
            cursor.position.line.min(anchor.line).saturating_sub(1),
            (cursor.position.line.max(anchor.line) + 1).min(count.saturating_sub(1)),
            count,
        )
    }

    fn refresh_snapshot(&mut self, (first, last, before_count): (usize, usize, usize)) {
        let internal = self.0.get_mut();
        if internal.snapshot_edit_depth != 0 {
            return;
        }
        let after_count = internal.editor.line_count();
        let after_last = (last + after_count)
            .saturating_sub(before_count)
            .max(first)
            .min(after_count.saturating_sub(1));
        let editor = &internal.editor;
        internal.snapshot.replace_lines(
            first,
            last + 1,
            (first..=after_last).map(|index| {
                let ending = editor.line(index).expect("native line exists").ending;
                (
                    editor.buffer().lines[index].text(),
                    ending,
                    terminator(ending, index + 1 == after_count),
                )
            }),
        );
    }

    /// Clone an immutable, Send snapshot in constant time. Its complete text
    /// is materialized once on whichever worker actually needs it.
    pub fn text_snapshot(&self) -> TextSnapshot {
        self.0.borrow().snapshot.clone()
    }

    /// Native multiline insertion and removal shift row vectors repeatedly.
    /// Bound those operations; large splices rebuild from native fragments in
    /// one pass, while an append near EOF can keep its existing styled prefix.
    fn try_splice(&mut self, replacement: &str, breaks: Option<&[usize]>) -> bool {
        const MAX_NATIVE_ROWS: usize = 128;
        const MAX_STREAMED_TAIL: usize = 32;
        let cursor = self.cursor();
        let anchor = cursor.selection.unwrap_or(cursor.position);
        let inserted_rows = paste_fragments(replacement, breaks)
            .filter(|fragment| {
                fragment
                    .as_bytes()
                    .last()
                    .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
            })
            .count();
        if inserted_rows <= MAX_NATIVE_ROWS
            && cursor.position.line.abs_diff(anchor.line) <= MAX_NATIVE_ROWS
        {
            return false;
        }
        let count = self.line_count();
        let selection = if cursor.selection.is_some() {
            let first = cursor.position.line.min(anchor.line).saturating_sub(1);
            let last = (cursor.position.line.max(anchor.line) + 1).min(count.saturating_sub(1));
            self.selected_span(&self.excerpt(first, last), cursor)
        } else {
            cursor
        };
        let anchor = selection.selection.unwrap_or(selection.position);
        let (start, end) = if (anchor.line, anchor.column)
            <= (selection.position.line, selection.position.column)
        {
            (anchor, selection.position)
        } else {
            (selection.position, anchor)
        };
        let internal = self.0.get_mut();
        internal.pending_scroll = None;
        internal.reveal_horizontal = true;
        if end.line - start.line <= MAX_NATIVE_ROWS
            && count.saturating_sub(end.line + 1) <= MAX_STREAMED_TAIL
            && end.column == internal.editor.buffer().lines[end.line].text().len()
        {
            let mut fragments = paste_fragments(replacement, breaks).peekable();
            if fragments.peek().is_none() {
                internal
                    .editor
                    .perform(Action::Edit(Edit::Paste(Arc::new(String::new()))));
            } else {
                for fragment in fragments {
                    append_fragment(&mut internal.editor, fragment, "");
                }
            }
            mark_changed_from(&mut internal.editor, start);
            return true;
        }

        let scroll = internal.editor.buffer().scroll();
        let previous_view = ScrollPosition {
            line: scroll.line,
            vertical: scroll.vertical,
            horizontal: internal.horizontal,
        };
        let mut editor = NativeEditor::with_text("");
        let lines = &internal.editor.buffer().lines;
        for line in &lines[..start.line] {
            append_fragment(&mut editor, line.text(), line.ending().as_str());
        }
        append_fragment(&mut editor, &lines[start.line].text()[..start.column], "");
        for fragment in paste_fragments(replacement, breaks) {
            append_fragment(&mut editor, fragment, "");
        }
        let caret = editor.cursor();
        let suffix = &lines[end.line];
        append_fragment(
            &mut editor,
            &suffix.text()[end.column..],
            suffix.ending().as_str(),
        );
        for line in &lines[end.line + 1..] {
            append_fragment(&mut editor, line.text(), line.ending().as_str());
        }
        editor.move_to(caret);
        mark_changed_from(&mut editor, Position { line: 0, column: 0 });
        if let Some(layout) = internal.layout {
            layout.prepare(
                &mut editor,
                internal.pending_edit_scroll.unwrap_or(previous_view),
            );
            internal.pending_edit_scroll = None;
        } else {
            internal.pending_edit_scroll.get_or_insert(previous_view);
        }
        internal.editor = editor;
        internal.reset_highlighter = true;
        true
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
        internal
            .pending_scroll
            .or(internal.pending_edit_scroll)
            .unwrap_or_else(|| {
                let scroll = internal.editor.buffer().scroll();
                let line_height = internal.editor.buffer().metrics().line_height;
                canonical_scroll(
                    ScrollPosition {
                        line: scroll.line,
                        vertical: scroll.vertical,
                        horizontal: internal.horizontal,
                    },
                    line_height,
                    internal.wrapping == Wrapping::None,
                )
            })
    }

    /// Restores after layout knows the current font, wrapping, and viewport.
    /// A subsequent explicit edit/caret move takes precedence over restoration.
    pub fn restore_scroll_position(&mut self, position: ScrollPosition) {
        self.0.get_mut().pending_edit_scroll_lines = 0;
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

    /// Borrows one line's text in place, together with its line ending.
    ///
    /// [`Content::line`] has to clone the line into a `Cow::Owned` because the
    /// borrow it would otherwise hand out lives inside this type's `RefCell`.
    /// Callers that only read the text — mapping a caret to a byte offset, for
    /// instance — walk every line of the document, so that clone is one heap
    /// allocation and one copy per line of the file on every keystroke. A
    /// closure keeps the borrow alive for exactly as long as it is needed and
    /// allocates nothing.
    ///
    /// Returns `None` for an out-of-range index, like [`Content::line`].
    pub fn with_line<T>(&self, index: usize, f: impl FnOnce(&str, LineEnding) -> T) -> Option<T> {
        let internal = self.0.borrow();
        let line = internal.editor.line(index)?;
        Some(f(&line.text, line.ending))
    }

    /// Search Unicode text a line at a time, retaining source byte positions.
    pub fn find_matches(&self, query: &str) -> MatchIndex {
        let internal = self.0.borrow();
        // NativeEditor borrows these lines in place; no whole-buffer snapshot
        // is needed for the synchronous, bounded small-file search.
        let lines =
            (0..internal.editor.line_count()).filter_map(|index| internal.editor.line(index));
        find_in_lines(lines.map(|line| line.text), query, || false).unwrap_or_default()
    }

    pub fn find_matches_with(
        &self,
        query: &str,
        options: crate::editor_search::FindOptions,
    ) -> Result<MatchIndex, String> {
        let internal = self.0.borrow();
        let lines =
            (0..internal.editor.line_count()).filter_map(|index| internal.editor.line(index));
        crate::editor_search::find_in_lines(lines.map(|line| line.text), query, options, || false)
            .map(Option::unwrap_or_default)
    }

    pub(crate) fn search_snapshot(&self) -> SearchSnapshot {
        SearchSnapshot {
            source: self.text_snapshot(),
        }
    }

    /// Byte length of the text [`Content::text`] would produce, without
    /// building it.
    ///
    /// This is a length, not a fingerprint: equal lengths do not imply equal
    /// text, so it can only rule a change *in*, never out.
    pub fn text_len(&self) -> usize {
        self.0.borrow().snapshot.len()
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
        #[cfg(test)]
        TEXT_CALLS.with(|count| count.set(count.get() + 1));
        let internal = self.0.borrow();
        let count = internal.editor.line_count();
        let mut contents = String::new();
        for index in 0..count {
            let Some(line) = internal.editor.line(index) else {
                break;
            };
            contents.push_str(&line.text);
            contents.push_str(terminator(line.ending, index + 1 == count));
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

    /// Performs one edit and reports exactly what it replaced, or `None`
    /// when it changed nothing, such as Backspace at the start of the file.
    ///
    /// The change is measured rather than predicted: the lines the edit can
    /// reach — those under the caret and selection, and one either side for
    /// a delete that joins lines — are read before and after, and compared.
    /// That costs a copy of those few lines, not of the document.
    pub fn perform_edit(&mut self, edit: Edit) -> Option<RecordedEdit> {
        let cursor = self.cursor();
        let anchor = cursor.selection.unwrap_or(cursor.position);
        let count = self.line_count();
        let first = cursor.position.line.min(anchor.line).saturating_sub(1);
        let last = (cursor.position.line.max(anchor.line) + 1).min(count.saturating_sub(1));
        let before = self.excerpt(first, last);
        let selection = self.selected_span(&before, cursor);
        // The edit begins no later than the selection or the caret —
        // Backspace one character before it — which settles where a change
        // beside a repeated character is recorded.
        let offset = |position| before.offset(position).unwrap_or(usize::MAX);
        let caret_at = offset(selection.position);
        let changed_from = match selection.selection {
            Some(anchor) => caret_at.min(offset(anchor)),
            None if matches!(edit, Edit::Backspace) => before
                .text()
                .get(..caret_at)
                .and_then(|text| text.chars().next_back())
                .map_or(caret_at, |character| caret_at - character.len_utf8()),
            None => caret_at,
        };
        self.perform(Action::Edit(edit));
        // Lines past the window only move; how far is how many the edit added
        // or removed.
        let after_count = self.line_count();
        let after_last = (last + after_count)
            .saturating_sub(count)
            .max(first)
            .min(after_count.saturating_sub(1));
        let after = self.excerpt(first, after_last);
        history::replacement(&before, &after, changed_from).map(|change| RecordedEdit {
            change,
            before: selection,
        })
    }

    /// Exact selected span, including native word/line selections.
    pub(crate) fn selected_cursor(&self) -> Cursor {
        let cursor = self.cursor();
        let anchor = cursor.selection.unwrap_or(cursor.position);
        let first = cursor.position.line.min(anchor.line).saturating_sub(1);
        let last =
            (cursor.position.line.max(anchor.line) + 1).min(self.line_count().saturating_sub(1));
        self.selected_span(&self.excerpt(first, last), cursor)
    }

    /// The caret with the exact span it has selected.
    ///
    /// A word or line selection reports only the point it was anchored at,
    /// not the span it covers, so the span is found as the selected text
    /// around both of its reported ends. A selection of nothing is no
    /// selection.
    fn selected_span(&self, excerpt: &Excerpt, cursor: Cursor) -> Cursor {
        let caret = Cursor {
            position: cursor.position,
            selection: None,
        };
        let Some(anchor) = cursor.selection else {
            return caret;
        };
        let Some(selected) = self.selection().filter(|selected| !selected.is_empty()) else {
            return caret;
        };
        // Native copy_selection joins native lines with LF, even when their
        // stored endings are CRLF, LFCR, or bare CR. Locate the same native
        // positions in that normalized view when the exact excerpt differs.
        let locate = |excerpt: &Excerpt| -> Option<Cursor> {
            let (Some(caret_at), Some(anchor_at)) =
                (excerpt.offset(cursor.position), excerpt.offset(anchor))
            else {
                return None;
            };
            let text = excerpt.text();
            let (low, high) = (caret_at.min(anchor_at), caret_at.max(anchor_at));
            // Only starts whose end covers both reported points can match.
            // Earlier candidates make repeated-character selections quadratic.
            let mut search = high.saturating_sub(selected.len());
            if search > low {
                return None;
            }
            while !text.is_char_boundary(search) {
                search -= 1;
            }
            let mut limit = (high + selected.len()).min(text.len());
            while !text.is_char_boundary(limit) {
                limit += 1;
            }
            let mut span = None;
            while let Some(found) = text
                .get(search..limit)
                .and_then(|rest| rest.find(&selected))
            {
                let start = search + found;
                if start > low {
                    break;
                }
                if start + selected.len() >= high {
                    span = Some(start..start + selected.len());
                    break;
                }
                search = start + text[start..].chars().next().map_or(1, char::len_utf8);
            }
            let Some(span) = span else {
                return None;
            };
            let (start, end) = (excerpt.position(span.start), excerpt.position(span.end));
            Some(if caret_at < anchor_at {
                Cursor {
                    position: start,
                    selection: Some(end),
                }
            } else {
                Cursor {
                    position: end,
                    selection: Some(start),
                }
            })
        };
        if let Some(span) = locate(excerpt) {
            return span;
        }
        let first = excerpt.position(0).line;
        let last =
            (cursor.position.line.max(anchor.line) + 1).min(self.line_count().saturating_sub(1));
        let internal = self.0.borrow();
        let lines = &internal.editor.buffer().lines;
        let mut normalized = Excerpt::new(first);
        for (index, line) in lines.iter().enumerate().take(last + 1).skip(first) {
            normalized.push_line(line.text(), if index + 1 < lines.len() { "\n" } else { "" });
        }
        locate(&normalized).unwrap_or(cursor)
    }

    /// Serializes lines `first..=last` exactly as [`Content::text`] would.
    fn excerpt(&self, first: usize, last: usize) -> Excerpt {
        let internal = self.0.borrow();
        let count = internal.editor.line_count();
        let mut excerpt = Excerpt::new(first);
        for index in first..=last.min(count.saturating_sub(1)) {
            let Some(line) = internal.editor.line(index) else {
                break;
            };
            excerpt.push_line(&line.text, terminator(line.ending, index + 1 == count));
        }
        if count == 0 {
            excerpt.push_line("", "");
        }
        excerpt
    }

    /// Where `expected` ends if the buffer holds it at `start`, and `None`
    /// when it does not.
    ///
    /// Reads the lines in place, so checking a range costs its own length.
    pub fn range_end(&self, start: Position, expected: &str) -> Option<Position> {
        let internal = self.0.borrow();
        let count = internal.editor.line_count();
        let mut position = start;
        let mut rest = expected;
        loop {
            let line = internal.editor.line(position.line)?;
            let segment = line.text.get(position.column..)?;
            if segment.starts_with(rest) {
                return Some(Position {
                    line: position.line,
                    column: position.column + rest.len(),
                });
            }
            rest = rest.strip_prefix(segment)?;
            let ending = terminator(line.ending, position.line + 1 == count);
            if ending.is_empty() {
                return None;
            }
            rest = rest.strip_prefix(ending)?;
            position = Position {
                line: position.line + 1,
                column: 0,
            };
            if rest.is_empty() {
                return Some(position);
            }
        }
    }

    /// Measures a whole-document replacement against the native line map,
    /// including boundaries which serializing and reparsing would change.
    pub fn replacement_to_text(&self, replacement: &str) -> Option<TextChange> {
        history::replacement(
            &self.excerpt(0, self.line_count().saturating_sub(1)),
            &Excerpt::document(replacement),
            usize::MAX,
        )
    }

    pub fn replace_range_recorded(
        &mut self,
        start: Position,
        end: Position,
        replacement: &str,
    ) -> Option<TextChange> {
        self.set_cursor(Cursor {
            position: end,
            selection: ((start.line, start.column) != (end.line, end.column)).then_some(start),
        });
        self.perform_edit(if replacement.is_empty() {
            Edit::Delete
        } else {
            Edit::Paste(Arc::new(replacement.to_owned()))
        })
        .map(|edit| edit.change)
    }

    /// Replays native line fragments separately. Combining their bytes into
    /// one paste can join LF and CR into a different pair of line endings.
    pub fn replace_range_with_breaks(
        &mut self,
        start: Position,
        end: Position,
        replacement: &str,
        breaks: &[usize],
    ) {
        let empty = (start.line, start.column) == (end.line, end.column);
        self.perform(Action::ClearSelection);
        self.move_to(Cursor {
            position: end,
            selection: (!empty).then_some(start),
        });
        if !replacement.is_empty() {
            if history::canonical_breaks(replacement, breaks) {
                self.perform(Action::Edit(Edit::Paste(Arc::new(replacement.to_owned()))));
                return;
            }
            let window = self.snapshot_edit_window();
            if self.try_splice(replacement, Some(breaks)) {
                self.refresh_snapshot(window);
                return;
            }
            let mut previous = 0;
            for end in breaks
                .iter()
                .copied()
                .chain(std::iter::once(replacement.len()))
            {
                if end > previous {
                    self.perform(Action::Edit(Edit::Paste(Arc::new(
                        replacement[previous..end].to_owned(),
                    ))));
                }
                previous = end;
            }
        } else if !empty {
            self.perform(Action::Edit(Edit::Delete));
        }
    }

    /// Places the caret and selection, clamped into the buffer, and replaces
    /// any selection there was.
    pub fn set_cursor(&mut self, cursor: Cursor) {
        let clamp = |position: Position| {
            let internal = self.0.borrow();
            let line = position
                .line
                .min(internal.editor.line_count().saturating_sub(1));
            let column = internal.editor.line(line).map_or(0, |text| {
                let mut column = position.column.min(text.text.len());
                while !text.text.is_char_boundary(column) {
                    column -= 1;
                }
                column
            });
            Position { line, column }
        };
        let position = clamp(cursor.position);
        // A selection of nothing would swallow the next Backspace or Delete.
        let selection = cursor
            .selection
            .map(clamp)
            .filter(|anchor| (anchor.line, anchor.column) != (position.line, position.column));
        let cursor = Cursor {
            position,
            selection,
        };
        self.perform(Action::ClearSelection);
        self.move_to(cursor);
    }
}

/// One edit as the history records it.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedEdit {
    /// What the edit replaced.
    pub change: TextChange,
    /// The caret and the exact span selected before the edit.
    pub before: Cursor,
}

/// What [`Content::text`] writes after a line: nothing after the last one,
/// and the default newline for an unterminated line in the middle.
fn terminator(ending: LineEnding, last: bool) -> &'static str {
    if last {
        ""
    } else if ending == LineEnding::None {
        LineEnding::default().as_str()
    } else {
        ending.as_str()
    }
}

impl<R> history::EditBuffer for Content<R>
where
    R: text::Renderer<Font = cosmic::iced::Font, Editor = NativeEditor>,
{
    fn replace(
        &mut self,
        start: Position,
        expected: &str,
        replacement: &str,
        replacement_breaks: &[usize],
    ) -> bool {
        let Some(end) = self.range_end(start, expected) else {
            return false;
        };
        self.replace_range_with_breaks(start, end, replacement, replacement_breaks);
        true
    }

    fn set_cursor(&mut self, cursor: Cursor) {
        Content::set_cursor(self, cursor);
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
    /// The last modifier state the compositor reported, because a wheel event
    /// carries none of its own and the Shift axis swap has to know.
    modifiers: keyboard::Modifiers,
    scrollbar_grab: Option<f32>,
    horizontal_grab: Option<f32>,
    scrollbar_target: Option<ScrollTarget>,
    last_theme: RefCell<Option<String>>,
    highlighter: RefCell<Highlighter>,
    highlighter_settings: Highlighter::Settings,
    highlighter_format_address: usize,
    context_menu_position: Option<Point>,
    pending_edit: Option<Action>,
    hover: Option<(Position, Instant, bool)>,
    popup_bounds: Option<Rectangle>,
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
    /// Leaves native focus without turning an outside pointer press into an
    /// editor action. External result controls still own the selected buffer.
    fn blur(&mut self) {
        self.focus = None;
        self.drag_click = None;
        self.context_menu_position = None;
    }

    /// Clears focus, selection, and all interaction state.
    pub fn clear_focus(&mut self) {
        self.blur();
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
            modifiers: keyboard::Modifiers::default(),
            scrollbar_grab: None,
            horizontal_grab: None,
            scrollbar_target: None,
            last_theme: RefCell::default(),
            highlighter: RefCell::new(Highlighter::new(&self.highlighter_settings)),
            highlighter_settings: self.highlighter_settings.clone(),
            highlighter_format_address: self.highlighter_format as usize,
            context_menu_position: None,
            pending_edit: None,
            hover: None,
            popup_bounds: None,
        })
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: self.height,
        }
    }

    fn children(&self) -> Vec<widget::Tree> {
        self.popup.iter().map(widget::Tree::new).collect()
    }

    fn diff(&mut self, tree: &mut widget::Tree) {
        if let Some(popup) = &mut self.popup {
            if tree.children.is_empty() {
                tree.children.push(widget::Tree::new(&*popup));
            }
            tree.children[0].diff(popup);
            tree.children.truncate(1);
        } else {
            tree.children.clear();
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
        internal.layout = Some(EditorLayout {
            bounds: canvas,
            font: self.font.unwrap_or_else(|| renderer.default_font()),
            size: self.text_size.unwrap_or_else(|| renderer.default_size()),
            line_height: self.line_height,
            wrapping: self.wrapping,
        });
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
        let pending_edit_scroll = internal.pending_edit_scroll.take();
        let pending_lines = std::mem::take(&mut internal.pending_edit_scroll_lines);
        shape_after_splice(
            &mut internal.editor,
            pending_edit_scroll,
            pending_lines,
            self.wrapping,
            |editor| shape(editor, canvas),
        );
        if let Some(target) = target {
            seek_scroll(&mut internal.editor, target, self.wrapping, |editor| {
                shape(editor, canvas)
            });
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
            if let Some(layout) = &mut internal.layout {
                layout.bounds = canvas;
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
        internal.finish_layout(state.highlighter.borrow_mut().deref_mut());

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
        // Every later branch may return early, and the wheel needs the current
        // modifiers no matter which one does, so record them first. Losing
        // window focus also loses the key release that would clear Shift, so a
        // stale modifier must not survive it.
        match event {
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = *modifiers;
            }
            Event::Window(window::Event::Unfocused) => {
                state.modifiers = keyboard::Modifiers::default();
            }
            _ => {}
        }
        if !geometry.scrollable {
            state.scrollbar_grab = None;
            state.scrollbar_target = None;
        }
        if geometry.horizontal.is_none() {
            state.horizontal_grab = None;
        }
        let pointer = cursor.position().filter(|point| _viewport.contains(*point));
        // Layout can extend behind an adjacent pane or a clipped parent. New
        // pointer interactions belong only to the visible editor; keep the
        // original cursor for an already-owned drag below.
        let visible_cursor = if pointer.is_some() {
            cursor
        } else {
            mouse::Cursor::Unavailable
        };
        if self.popup.is_none() {
            state.popup_bounds = None;
        }
        if self.on_hover.is_some() || self.on_definition.is_some() {
            let text_bounds = layout.bounds().shrink(self.text_padding());
            let hit = pointer
                .filter(|point| text_bounds.contains(*point))
                .and_then(|point| {
                    let internal = self.content.0.borrow();
                    let point = point
                        - text_bounds.position()
                        - Vector::new(internal.horizontal_translation(), 0.0);
                    let cursor = internal.editor.buffer().hit(point.x, point.y)?;
                    Some(Position {
                        line: cursor.line,
                        column: cursor.index,
                    })
                });
            match event {
                Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                    if pointer.is_some_and(|point| {
                        state
                            .popup_bounds
                            .is_some_and(|bounds| bounds.contains(point))
                    }) {
                        return;
                    }
                    if state.hover.map(|hover| hover.0) != hit {
                        if state.hover.take().is_some()
                            && let Some(on_hover) = &self.on_hover
                        {
                            shell.publish(on_hover(None));
                        }
                        state.hover = hit.map(|at| (at, Instant::now(), false));
                    }
                    if let Some((_, started, false)) = state.hover {
                        shell.request_redraw_at(started + Duration::from_millis(500));
                    }
                }
                Event::Window(window::Event::RedrawRequested(now)) => {
                    if let Some((at, started, sent)) = &mut state.hover
                        && !*sent
                        && *now >= *started + Duration::from_millis(500)
                        && let Some(on_hover) = &self.on_hover
                    {
                        *sent = true;
                        shell.publish(on_hover(Some(*at)));
                    }
                }
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                    if state.modifiers.command()
                        && hit.is_some()
                        && self.on_definition.is_some() =>
                {
                    shell.publish(self.on_definition.as_ref().unwrap()(hit.unwrap()));
                    shell.capture_event();
                    return;
                }
                Event::Keyboard(keyboard::Event::KeyPressed { .. })
                | Event::Mouse(mouse::Event::CursorLeft)
                | Event::Window(window::Event::Unfocused) => {
                    if state.hover.take().is_some()
                        && let Some(on_hover) = &self.on_hover
                    {
                        shell.publish(on_hover(None));
                    }
                }
                _ => {}
            }
        }
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
        // The horizontal thumb is dragged the same way, but it writes the
        // canvas offset directly: the native editor has no horizontal scroll
        // action to publish, and `draw` reads the offset as it is.
        match event {
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                if state.horizontal_grab.take().is_some() =>
            {
                shell.capture_event();
                shell.request_redraw();
                return;
            }
            Event::Window(window::Event::Unfocused) => state.horizontal_grab = None,
            _ => {}
        }
        let pan = geometry.horizontal.and_then(|scroll| match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if self.on_edit.is_some()
                    && pointer.is_some_and(|point| scroll.track.contains(point)) =>
            {
                let point = pointer?;
                let grab = if scroll.thumb.contains(point) {
                    point.x - scroll.thumb.x
                } else {
                    scroll.thumb.width / 2.0
                };
                state.horizontal_grab = Some(grab);
                Some(scroll.offset_at(point.x, grab))
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let grab = state.horizontal_grab?;
                cursor
                    .position()
                    .map(|point| scroll.offset_at(point.x, grab))
            }
            _ => None,
        });
        if let Some(offset) = pan {
            let mut internal = self.content.0.borrow_mut();
            if internal.horizontal != offset {
                internal.horizontal = offset;
                internal.reveal_horizontal = false;
            }
            drop(internal);
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
            ) && visible_cursor.position_over(text_bounds).is_none()
            {
                state.blur();
            }

            match event {
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                    if let Some(pos) = visible_cursor.position_over(text_bounds) {
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

        let input_cursor = if matches!(event, Event::Mouse(mouse::Event::CursorMoved { .. }))
            && state.drag_click.is_some()
        {
            // A drag admitted in the viewport keeps its existing movement
            // and release contract after crossing the clip boundary.
            cursor
        } else {
            visible_cursor
        };
        if let Some(update) = Update::from_event(
            event,
            state,
            layout.bounds(),
            self.text_padding(),
            input_cursor,
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
                    // X11 and Wayland both expect a finished selection to
                    // become the primary selection, which middle click pastes.
                    if let Some(selection) = self.content.selection() {
                        clipboard.write(clipboard::Kind::Primary, selection);
                    }
                }
                Update::PastePrimary(position) => {
                    // Middle click moves the caret first, exactly as it does
                    // everywhere else on this platform, and then inserts.
                    shell.publish(on_edit(Action::Click(position)));
                    if let Some(contents) = clipboard.read(clipboard::Kind::Primary) {
                        shell.publish(on_edit(Action::Edit(Edit::Paste(Arc::new(contents)))));
                    }
                    state.focus = Some(Focus::now());
                    shell.capture_event();
                }
                Update::Scroll {
                    lines,
                    horizontal,
                    redirected,
                } => {
                    let bounds = self.content.0.borrow().editor.bounds();

                    if bounds.height >= i32::MAX as f32 {
                        return;
                    }

                    let panned = self.pan_horizontally(layout.bounds(), horizontal);
                    if panned {
                        shell.request_redraw();
                        shell.capture_event();
                    }
                    // Shift makes the gesture horizontal, so the vertical
                    // delta is spent whether or not the pan had anywhere to
                    // go. A redirect the horizontal axis cannot absorb - the
                    // left or right bound, a wrapped buffer, a document whose
                    // longest line already fits - used to fall back to
                    // scrolling the document, which moved it along an axis the
                    // user had explicitly modified away from.
                    let lines = if redirected { 0.0 } else { lines } + state.partial_scroll;
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
        // Paint the added lines under selection and glyphs, using only shaped
        // visible runs. Wrapped continuations share their logical source
        // annotation.
        //
        // `draw` is generic over `Theme: Catalog`, and a `text_editor` style
        // has no semantic slot for "this line is new", so the tint is read off
        // the process-global active theme - the same theme this frame renders
        // with. That read locks and clones, so it happens once per draw and
        // only when there is a tint to paint.
        if !self.additions.is_empty() {
            let wash = crate::design::added_line_wash(&cosmic::theme::active());
            for run in internal.editor.buffer().layout_runs() {
                if self.additions.binary_search(&run.line_i).is_err() {
                    continue;
                }
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
                        wash,
                    );
                }
            }
        }

        let horizontal = Vector::new(internal.horizontal_translation(), 0.0);
        if let Some(matches) = self.find_matches.filter(|matches| !matches.is_empty()) {
            for run in internal.editor.buffer().layout_runs() {
                for glyph in run.glyphs {
                    let rect = Rectangle::new(
                        Point::new(
                            text_bounds.x + horizontal.x + glyph.x,
                            text_bounds.y + run.line_top,
                        ),
                        Size::new(glyph.w, run.line_height),
                    );
                    let Some(bounds) = text_bounds.intersection(&rect) else {
                        continue;
                    };
                    // Seek by the actual glyph's source bytes, including bidi
                    // and ligatures. Match density outside this glyph cannot
                    // multiply its paint work or create one quad per hit.
                    let (first, overlapping) = matches.visible(run.line_i, glyph.start, glyph.end);
                    if overlapping.len() == 0 {
                        continue;
                    }
                    let current = self.current_match >= first
                        && self.current_match - first < overlapping.len();
                    let mut color = style.selection;
                    color.a *= if current { 0.55 } else { 0.18 };
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds,
                            ..Default::default()
                        },
                        color,
                    );
                }
            }
        }
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

        if !self.diagnostics.is_empty() {
            use cosmic::iced::advanced::graphics::text::cosmic_text;
            let theme = cosmic::theme::active();
            for run in internal.editor.buffer().layout_runs() {
                for diagnostic in self.diagnostics.iter().filter(|diagnostic| {
                    diagnostic.start.line <= run.line_i && diagnostic.end.line >= run.line_i
                }) {
                    let color = match diagnostic.severity {
                        crate::lsp::LspSeverity::Error => crate::design::destructive_color(&theme),
                        crate::lsp::LspSeverity::Warning => crate::design::warning_color(&theme),
                        crate::lsp::LspSeverity::Information => crate::design::accent_color(&theme),
                        crate::lsp::LspSeverity::Hint => crate::design::muted_color(&theme),
                    };
                    let start = if run.line_i == diagnostic.start.line {
                        diagnostic.start.column
                    } else {
                        0
                    };
                    let end = if run.line_i == diagnostic.end.line {
                        diagnostic.end.column
                    } else {
                        run.text.len()
                    };
                    let spans = run.highlight(
                        cosmic_text::Cursor::new(run.line_i, start),
                        cosmic_text::Cursor::new(run.line_i, end),
                    );
                    for (left, width) in spans {
                        let y = text_bounds.y + run.line_top + run.line_height - 2.0;
                        let left = left + text_bounds.x + horizontal.x;
                        let mut x = left.max(text_bounds.x);
                        let right = (left + width).min(text_bounds.x + text_bounds.width);
                        let mut raised = false;
                        while x < right {
                            let mark = Rectangle::new(
                                Point::new(x, y + if raised { 0.0 } else { 1.0 }),
                                Size::new(2.0_f32.min(right - x), 1.0),
                            );
                            if let Some(bounds) = text_bounds.intersection(&mark) {
                                renderer.fill_quad(
                                    renderer::Quad {
                                        bounds,
                                        ..Default::default()
                                    },
                                    color,
                                );
                            }
                            x += 2.0;
                            raised = !raised;
                        }
                    }
                    let dot = Rectangle::new(
                        Point::new(
                            bounds.x + 2.0,
                            text_bounds.y + run.line_top + run.line_height / 2.0 - 2.0,
                        ),
                        Size::new(4.0, 4.0),
                    );
                    if let Some(bounds) = bounds.intersection(&dot) {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds,
                                border: Border {
                                    radius: 2.0.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            },
                            color,
                        );
                    }
                }
            }
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
        if geometry.scrollable {
            draw_scrollbar(
                renderer,
                geometry.track,
                geometry.thumb,
                style.value,
                state.scrollbar_grab.is_some() || _cursor.is_over(geometry.track),
            );
        }
        if let Some(scroll) = geometry.horizontal {
            draw_scrollbar(
                renderer,
                scroll.track,
                scroll.thumb,
                style.value,
                state.horizontal_grab.is_some() || _cursor.is_over(scroll.track),
            );
        }
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
        let over_horizontal = geometry
            .horizontal
            .is_some_and(|scroll| cursor.is_over(scroll.track));
        if cursor.is_over(geometry.track) || over_horizontal {
            return if !is_disabled && (geometry.scrollable || over_horizontal) {
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

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut widget::Tree,
        layout: Layout<'b>,
        _renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let internal = self.content.0.borrow();
        let point = if let Some(at) = self.popup_at {
            use cosmic::iced::advanced::graphics::text::cosmic_text;
            internal.editor.buffer().layout_runs().find_map(|run| {
                if run.line_i != at.line as usize {
                    return None;
                }
                let cursor = cosmic_text::Cursor::new(run.line_i, at.column);
                run.cursor_position(&cursor)
                    .map(|x| Point::new(x, run.line_top))
            })
        } else {
            internal.caret_position()
        }?;
        let origin = layout.bounds().shrink(self.text_padding()).position();
        let anchor = origin + Vector::new(point.x + internal.horizontal_translation(), point.y);
        // Workspace layouts are in content space during the overlay pass;
        // the camera translation places their anchors in the screen-space viewport.
        let anchor = anchor + translation;
        if !viewport.contains(anchor) {
            return None;
        }
        let height = self
            .line_height
            .to_absolute(self.text_size.unwrap_or(Pixels(13.0)))
            .0;
        let popup = self.popup.as_mut()?;
        Some(overlay::Element::new(Box::new(EditorPopup {
            content: popup,
            tree: &mut tree.children[0],
            bounds: &mut tree.state.downcast_mut::<State<Highlighter>>().popup_bounds,
            anchor: Rectangle::new(anchor, Size::new(1.0, height)),
        })))
    }

    fn id(&self) -> Option<widget::Id> {
        self.id.clone()
    }
    fn set_id(&mut self, id: widget::Id) {
        self.id = Some(id);
    }
}

struct EditorPopup<'a, 'b, Message, Theme, Renderer> {
    content: &'a mut Element<'b, Message, Theme, Renderer>,
    tree: &'a mut widget::Tree,
    anchor: Rectangle,
    bounds: &'a mut Option<Rectangle>,
}

impl<Message, Theme, Renderer> overlay::Overlay<Message, Theme, Renderer>
    for EditorPopup<'_, '_, Message, Theme, Renderer>
where
    Renderer: cosmic::iced::advanced::Renderer,
{
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        let node = self.content.as_widget_mut().layout(
            self.tree,
            renderer,
            &layout::Limits::new(Size::ZERO, bounds),
        );
        let size = node.size();
        let x = self
            .anchor
            .x
            .clamp(0.0, (bounds.width - size.width).max(0.0));
        let below = self.anchor.y + self.anchor.height + 4.0;
        let y = if below + size.height <= bounds.height {
            below
        } else {
            self.anchor.y - size.height - 4.0
        };
        let position = Point::new(x, y.clamp(0.0, (bounds.height - size.height).max(0.0)));
        *self.bounds = Some(Rectangle::new(position, size));
        node.move_to(position)
    }
    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.content.as_widget().draw(
            self.tree,
            renderer,
            theme,
            style,
            layout,
            cursor,
            &layout.bounds(),
        );
    }
    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        self.content.as_widget_mut().update(
            self.tree,
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            &layout.bounds(),
        );
    }
    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            self.tree,
            layout,
            cursor,
            &layout.bounds(),
            renderer,
        )
    }
    fn operate(
        &mut self,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(self.tree, layout, renderer, operation);
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
    /// Middle click: move the caret there, then insert the primary selection.
    PastePrimary(Point),
    Scroll {
        /// Vertical movement, in document lines.
        lines: f32,
        /// Horizontal movement, in logical pixels.
        horizontal: f32,
        /// Whether `horizontal` was produced by redirecting the wheel's
        /// vertical delta rather than by a real horizontal one.
        ///
        /// The handler needs this because only it knows the geometry: a
        /// redirected gesture that the horizontal axis cannot absorb - a
        /// wrapped buffer, or a file whose longest line already fits the
        /// viewport - has to fall back to scrolling vertically, or holding
        /// Shift would appear to freeze the wheel.
        redirected: bool,
    },
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
                mouse::Event::ButtonPressed(mouse::Button::Middle) => cursor
                    .position_in(bounds)
                    .map(|position| position - Vector::new(padding.left, padding.top))
                    .map(Update::PastePrimary),
                mouse::Event::CursorMoved { .. } => match state.drag_click {
                    Some(mouse::click::Kind::Single) => {
                        let cursor_position =
                            cursor.position_in(bounds)? - Vector::new(padding.left, padding.top);

                        Some(Update::Drag(cursor_position))
                    }
                    _ => None,
                },
                mouse::Event::WheelScrolled { delta } if cursor.is_over(bounds) => {
                    // The horizontal axis is in pixels because that is what the
                    // canvas offset is; the factors are iced's own, so a wheel
                    // notch pans this editor as far as it pans any other
                    // scrollable in the shell.
                    //
                    // Shift routes the wheel's vertical delta onto the
                    // horizontal axis, which is the convention every native
                    // scrollable in the shell already follows and the only way
                    // a wheel that reports nothing but `y` can follow a long
                    // unwrapped line. Only one axis is ever taken, because
                    // `interaction::wheel_as_horizontal` established that
                    // picking an axis is the only safe projection: summing the
                    // two components cancels an opposite-sign diagonal exactly,
                    // discarding the gesture. The axis is chosen by magnitude
                    // rather than by `x == 0`, because a high-resolution
                    // trackpad reports a small nonzero `x` on a two-finger drag
                    // that is plainly vertical, and testing for exact zero
                    // would hand the whole gesture to that jitter.
                    //
                    // Both axes are negated because a positive wheel delta asks
                    // for the content to move right or down, which moves the
                    // viewport the opposite way.
                    //
                    // `lines` stays the plain vertical delta in every case. The
                    // handler suppresses it only once it has confirmed the
                    // horizontal axis absorbed the gesture, so a redirect that
                    // had nowhere to go still scrolls.
                    let shift = state.modifiers.shift();
                    let redirect = move |x: f32, y: f32| shift && y.abs() > x.abs();
                    let pan = move |x: f32, y: f32| -if redirect(x, y) { y } else { x };
                    Some(match *delta {
                        mouse::ScrollDelta::Lines { x, y } => Update::Scroll {
                            lines: if y == 0.0 {
                                0.0
                            } else {
                                y.signum() * -(y.abs() * 4.0).max(1.0)
                            },
                            horizontal: pan(x, y) * 60.0,
                            redirected: redirect(x, y),
                        },
                        mouse::ScrollDelta::Pixels { x, y } => Update::Scroll {
                            lines: -y / 4.0,
                            horizontal: pan(x, y),
                            redirected: redirect(x, y),
                        },
                    })
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

/// One scrollbar's track and thumb, in the shell's scrollbar tokens.
fn draw_scrollbar<Renderer: renderer::Renderer>(
    renderer: &mut Renderer,
    track: Rectangle,
    thumb: Rectangle,
    color: cosmic::iced::Color,
    active: bool,
) {
    let quad = |bounds| renderer::Quad {
        bounds,
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        ..renderer::Quad::default()
    };
    let mut track_color = color;
    track_color.a *= 0.06;
    renderer.fill_quad(quad(track), track_color);
    let mut thumb_color = color;
    thumb_color.a *= if active { 0.55 } else { 0.3 };
    renderer.fill_quad(quad(thumb), thumb_color);
}

#[derive(Debug, Clone, Copy)]
struct ScrollGeometry {
    track: Rectangle,
    thumb: Rectangle,
    visible: f32,
    scrollable: bool,
    /// The horizontal pair, present only while the measured canvas is wider
    /// than the viewport.
    horizontal: Option<HorizontalScroll>,
}

/// A horizontal track, its thumb, and the offset range the thumb spans.
#[derive(Debug, Clone, Copy)]
struct HorizontalScroll {
    track: Rectangle,
    thumb: Rectangle,
    maximum: f32,
}

impl HorizontalScroll {
    /// The canvas offset a thumb grabbed at `grab` represents when the pointer
    /// is at `x`.
    fn offset_at(self, x: f32, grab: f32) -> f32 {
        let travel = self.track.width - self.thumb.width;
        if travel > 0.0 {
            ((x - self.track.x - grab) / travel).clamp(0.0, 1.0) * self.maximum
        } else {
            0.0
        }
    }
}

impl ScrollGeometry {
    fn new(
        bounds: Rectangle,
        text: Rectangle,
        total: f32,
        first: f32,
        visible: f32,
        canvas_width: f32,
        offset: f32,
    ) -> Self {
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
            horizontal: Self::horizontal(bounds, text, canvas_width, offset),
        }
    }

    fn horizontal(
        bounds: Rectangle,
        text: Rectangle,
        canvas_width: f32,
        offset: f32,
    ) -> Option<HorizontalScroll> {
        let maximum = canvas_width - text.width;
        if maximum <= 0.0 || text.width <= 0.0 {
            return None;
        }
        let track = Rectangle {
            x: text.x,
            y: bounds.y + bounds.height - crate::design::SCROLLBAR_GUTTER
                + crate::design::SCROLLBAR_MARGIN,
            width: text.width,
            height: crate::design::SCROLLBAR_THUMB_WIDTH,
        };
        let width = (track.width * text.width / canvas_width)
            .max(24.0)
            .min(track.width);
        let fraction = (offset / maximum).clamp(0.0, 1.0);
        Some(HorizontalScroll {
            track,
            thumb: Rectangle {
                x: track.x + (track.width - width) * fraction,
                width,
                ..track
            },
            maximum,
        })
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

    fn native_state(editor: &NativeEditor) -> Vec<(String, String)> {
        editor
            .buffer()
            .lines
            .iter()
            .map(|line| (line.text().to_owned(), format!("{:?}", line.ending())))
            .collect()
    }

    #[test]
    fn large_splices_preserve_native_lines_endings_and_caret() {
        let source = "α token\r\nβ token\n\rγ token\rδ token\n".repeat(100) + "EOF";
        let replacement = "é inserted\r\n漢\n\r".repeat(100);
        for (first, last) in [(0, 400), (20, 300), (250, 400), (400, 400), (100, 100)] {
            for reverse in [false, true] {
                for edit in [
                    Edit::Paste(Arc::new(replacement.clone())),
                    Edit::Paste(Arc::new(String::new())),
                    Edit::Delete,
                    Edit::Backspace,
                ] {
                    if first == last && !matches!(edit, Edit::Paste(_)) {
                        continue;
                    }
                    let mut content: Content = Content::with_text(&source);
                    let mut native = NativeEditor::with_text(&source);
                    let start = Position {
                        line: first,
                        column: if first == 400 { 3 } else { 2 },
                    };
                    let end = Position {
                        line: last,
                        column: if last == 400 { 3 } else { 3 },
                    };
                    let cursor = Cursor {
                        position: if reverse { start } else { end },
                        selection: (start != end).then_some(if reverse { end } else { start }),
                    };
                    content.set_cursor(cursor);
                    native.move_to(cursor);
                    native.perform(Action::Edit(edit.clone()));
                    content.perform(Action::Edit(edit.clone()));
                    assert_eq!(
                        native_state(&content.0.borrow().editor),
                        native_state(&native),
                        "{first}..{last}, reverse={reverse}, {edit:?}"
                    );
                    assert_eq!(content.cursor(), native.cursor());
                }
            }
        }
    }

    #[test]
    fn large_splices_normalize_word_and_line_selection_over_mixed_endings() {
        let source = "α token\r\nβ token\n\rγ token\rδ token\n".repeat(100);
        for selection in [Action::SelectWord, Action::SelectLine] {
            let mut content: Content = Content::with_text(&source);
            let mut native = NativeEditor::with_text(&source);
            let anchor = Cursor {
                position: Position {
                    line: 10,
                    column: 3,
                },
                selection: None,
            };
            content.move_to(anchor);
            native.move_to(anchor);
            content.perform(selection.clone());
            native.perform(selection);
            let caret = Cursor {
                position: Position {
                    line: 300,
                    column: 3,
                },
                selection: None,
            };
            content.move_to(caret);
            native.move_to(caret);
            let edit = Edit::Paste(Arc::new("new\r\n".repeat(200)));
            native.perform(Action::Edit(edit.clone()));
            content.perform_edit(edit);
            assert_eq!(
                native_state(&content.0.borrow().editor),
                native_state(&native)
            );
            assert_eq!(content.cursor(), native.cursor());
        }
    }

    #[test]
    fn large_recorded_fragment_replay_keeps_separate_lf_cr_boundaries() {
        let mut content: Content = Content::with_text(&"suffix\n".repeat(400));
        let mut native = NativeEditor::with_text(&"suffix\n".repeat(400));
        let fragments = ["x\n", "\r", "y\r\n"].repeat(100);
        let text = fragments.concat();
        let mut length = 0;
        let breaks: Vec<_> = fragments
            .iter()
            .map(|fragment| {
                length += fragment.len();
                length
            })
            .collect();
        content.replace_range_with_breaks(
            Position {
                line: 100,
                column: 2,
            },
            Position {
                line: 110,
                column: 3,
            },
            &text,
            &breaks,
        );
        native.move_to(Cursor {
            position: Position {
                line: 110,
                column: 3,
            },
            selection: Some(Position {
                line: 100,
                column: 2,
            }),
        });
        for fragment in fragments {
            native.perform(Action::Edit(Edit::Paste(Arc::new(fragment.to_owned()))));
        }
        assert_eq!(
            native_state(&content.0.borrow().editor),
            native_state(&native)
        );
        assert_eq!(content.cursor(), native.cursor());
    }

    #[test]
    fn rebuilt_buffer_retains_first_viewport_through_queued_edits() {
        let source = "row\n".repeat(600);
        let mut content: Content = Content::with_text(&source);
        layout_content(&mut content, 500.0, 240.0);
        seek_content(
            &mut content,
            ScrollTarget::Position(ScrollPosition {
                line: 90,
                vertical: 0.0,
                horizontal: 0.0,
            }),
            500.0,
            240.0,
        );
        let before = content.scroll_position();
        content.0.get_mut().layout = None; // exercise edits queued before a first configured layout
        for end in [400, 300] {
            content.set_cursor(Cursor {
                position: Position {
                    line: end,
                    column: 0,
                },
                selection: Some(Position {
                    line: 100,
                    column: 0,
                }),
            });
            content.perform(Action::Edit(Edit::Paste(Arc::new("new\n".repeat(200)))));
            assert_eq!(content.0.borrow().pending_edit_scroll, Some(before));
        }
        content.move_to(Cursor {
            position: Position {
                line: 100,
                column: 0,
            },
            selection: None,
        });
        layout_content(&mut content, 500.0, 240.0);
        assert_eq!(content.scroll_position(), before);
        assert!(content.0.borrow().pending_edit_scroll.is_none());
        let shaped = content
            .0
            .borrow()
            .editor
            .buffer()
            .lines
            .iter()
            .filter(|line| line.shape_opt().is_some())
            .count();
        assert!(
            shaped < 40,
            "viewport seeding must not shape the intervening document: {shaped}"
        );
    }

    #[test]
    fn rebuilt_buffer_queued_scroll_matches_native_before_and_after_layout() {
        let source = "row\n".repeat(600);
        for queued in [false, true] {
            let mut content: Content = Content::with_text(&source);
            let mut reference: Content = Content::with_text(&source);
            for document in [&mut content, &mut reference] {
                layout_content(document, 500.0, 240.0);
                seek_content(
                    document,
                    ScrollTarget::Position(ScrollPosition {
                        line: 90,
                        vertical: 0.0,
                        horizontal: 0.0,
                    }),
                    500.0,
                    240.0,
                );
                document.set_cursor(Cursor {
                    position: Position {
                        line: 400,
                        column: 0,
                    },
                    selection: Some(Position {
                        line: 100,
                        column: 0,
                    }),
                });
            }
            let paste = Action::Edit(Edit::Paste(Arc::new("new\n".repeat(200))));
            content.perform(paste.clone());
            reference.0.get_mut().editor.perform(paste);
            if !queued {
                layout_content(&mut content, 500.0, 240.0);
                layout_content(&mut reference, 500.0, 240.0);
            }
            content.perform(Action::Scroll { lines: -50 });
            reference
                .0
                .get_mut()
                .editor
                .perform(Action::Scroll { lines: -50 });
            layout_content(&mut content, 500.0, 240.0);
            layout_content(&mut reference, 500.0, 240.0);
            assert_eq!(
                content.scroll_position(),
                reference.scroll_position(),
                "queued={queued}"
            );
            assert_eq!(content.cursor(), reference.cursor());
            if !queued {
                assert!(content.scroll_position().line + 12 < content.cursor().position.line);
            }
        }
    }

    #[test]
    fn rebuilt_buffer_explicit_restore_wins_over_queued_scroll() {
        let mut content: Content = Content::with_text(&"row\n".repeat(600));
        layout_content(&mut content, 500.0, 240.0);
        content.set_cursor(Cursor {
            position: Position {
                line: 400,
                column: 0,
            },
            selection: Some(Position {
                line: 100,
                column: 0,
            }),
        });
        content.perform(Action::Edit(Edit::Paste(Arc::new("new\n".repeat(200)))));
        content.perform(Action::Scroll { lines: 50 });
        let restored = ScrollPosition {
            line: 20,
            vertical: 0.0,
            horizontal: 0.0,
        };
        content.restore_scroll_position(restored);
        assert_eq!(content.0.borrow().pending_edit_scroll_lines, 0);
        // Production applies an explicit restore after ordinary caret reveal.
        layout_content(&mut content, 500.0, 240.0);
        let target = content.0.get_mut().pending_scroll.take().unwrap();
        seek_content(&mut content, ScrollTarget::Position(target), 500.0, 240.0);
        assert_eq!(content.scroll_position(), restored);
    }

    #[test]
    fn large_paste_before_a_long_eof_suffix_rebuilds_once() {
        let source = "é".repeat(50_000);
        let mut content: Content = Content::with_text(&source);
        let mut native = NativeEditor::with_text(&source);
        let cursor = Cursor {
            position: Position { line: 0, column: 2 },
            selection: None,
        };
        content.move_to(cursor);
        native.move_to(cursor);
        let edit = Edit::Paste(Arc::new("new\n".repeat(200)));
        content.perform(Action::Edit(edit.clone()));
        native.perform(Action::Edit(edit));
        assert!(
            content.0.borrow().pending_edit_scroll.is_some(),
            "a long carried suffix must use the one-pass rebuild"
        );
        assert_eq!(
            native_state(&content.0.borrow().editor),
            native_state(&native)
        );
        assert_eq!(content.cursor(), native.cursor());
    }

    #[test]
    fn rebuilt_editor_queued_pointer_and_page_actions_match_native() {
        for action in [
            Action::Click(Point::new(24.0, 40.0)),
            Action::Drag(Point::new(40.0, 80.0)),
            Action::Move(Motion::PageDown),
            Action::Move(Motion::PageUp),
        ] {
            let source = "α token row\n".repeat(600);
            let mut content: Content = Content::with_text(&source);
            let mut reference: Content = Content::with_text(&source);
            for document in [&mut content, &mut reference] {
                layout_content(document, 500.0, 240.0);
                seek_content(
                    document,
                    ScrollTarget::Position(ScrollPosition {
                        line: 90,
                        vertical: 0.0,
                        horizontal: 0.0,
                    }),
                    500.0,
                    240.0,
                );
                document.set_cursor(Cursor {
                    position: Position {
                        line: 400,
                        column: 0,
                    },
                    selection: Some(Position {
                        line: 100,
                        column: 0,
                    }),
                });
            }
            let paste = Action::Edit(Edit::Paste(Arc::new("new row\n".repeat(200))));
            content.perform(paste.clone());
            reference.0.get_mut().editor.perform(paste);
            assert!(content.0.borrow().pending_edit_scroll.is_none());
            content.perform(action.clone());
            reference.0.get_mut().editor.perform(action.clone());
            assert_eq!(content.cursor(), reference.cursor(), "{action:?}");
            assert_eq!(content.selection(), reference.selection(), "{action:?}");
            let shaped = content
                .0
                .borrow()
                .editor
                .buffer()
                .lines
                .iter()
                .filter(|line| line.shape_opt().is_some())
                .count();
            assert!(
                shaped < 100,
                "queued {action:?} eagerly shaped {shaped} rows"
            );
            layout_content(&mut content, 500.0, 240.0);
            layout_content(&mut reference, 500.0, 240.0);
            assert_eq!(
                content.scroll_position(),
                reference.scroll_position(),
                "{action:?}"
            );
        }
    }

    #[test]
    fn rebuilt_editor_keeps_a_word_selection_made_before_layout() {
        let mut content: Content = Content::with_text(&"row words\n".repeat(600));
        layout_content(&mut content, 500.0, 240.0);
        content.set_cursor(Cursor {
            position: Position {
                line: 400,
                column: 0,
            },
            selection: Some(Position {
                line: 100,
                column: 0,
            }),
        });
        content.perform(Action::Edit(Edit::Paste(Arc::new(
            "new words\n".repeat(200),
        ))));
        content.move_to(Cursor {
            position: Position {
                line: 110,
                column: 6,
            },
            selection: None,
        });
        content.perform(Action::SelectWord);
        let selected = content.selection();
        layout_content(&mut content, 500.0, 240.0);
        assert_eq!(content.selection(), selected);
        assert_eq!(selected.as_deref(), Some("words"));
    }

    #[test]
    fn first_layout_after_a_large_splice_keeps_word_and_line_selection_modes() {
        for selection in [Action::SelectWord, Action::SelectLine] {
            let mut content: Content = Content::with_text(&"row words\n".repeat(600));
            content.set_cursor(Cursor {
                position: Position {
                    line: 400,
                    column: 0,
                },
                selection: Some(Position {
                    line: 100,
                    column: 0,
                }),
            });
            content.perform(Action::Edit(Edit::Paste(Arc::new(
                "new words\n".repeat(200),
            ))));
            content.move_to(Cursor {
                position: Position {
                    line: 110,
                    column: 6,
                },
                selection: None,
            });
            content.perform(selection);
            let selected = content.selection();
            let cursor = content.cursor();
            layout_content(&mut content, 500.0, 240.0);
            assert_eq!(content.selection(), selected);
            assert_eq!(content.cursor(), cursor);
        }
    }

    #[test]
    fn construction_preserves_exact_native_lines_endings_and_initial_cursor() {
        for source in [
            "",
            "plain",
            "one\ntwo\n",
            "one\r\ntwo\r\n",
            "one\rtwo\r",
            "one\n\rtwo\n\r",
            "\n\n\r\r\n\r\n",
            "Straße\rcafé\r\n東京\n\rtail",
            "α\r\nβ\nγ\rδ\n\r",
        ] {
            let original = NativeEditor::with_text(source);
            let content: Content = Content::with_text(source);
            let internal = content.0.borrow();
            let lines = |editor: &NativeEditor| {
                editor
                    .buffer()
                    .lines
                    .iter()
                    .map(|line| (line.text().to_owned(), line.ending()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(lines(&internal.editor), lines(&original), "{source:?}");
            assert_eq!(internal.editor.cursor(), original.cursor());
            drop(internal);
            assert_eq!(content.text(), source);
        }
    }

    #[test]
    fn construction_notifies_the_first_highlighter_from_line_zero() {
        struct ChangedLine(Option<usize>);
        impl Highlighter for ChangedLine {
            type Settings = ();
            type Highlight = ();
            type Iterator<'a> = std::iter::Empty<(ops::Range<usize>, ())>;
            fn new(_: &()) -> Self {
                Self(None)
            }
            fn update(&mut self, _: &()) {}
            fn change_line(&mut self, line: usize) {
                self.0 = Some(line);
            }
            fn highlight_line(&mut self, _: &str) -> Self::Iterator<'_> {
                std::iter::empty()
            }
            fn current_line(&self) -> usize {
                self.0.unwrap_or(usize::MAX)
            }
        }
        let mut content: Content = Content::with_text(&"token\n".repeat(300));
        let mut highlighter = ChangedLine::new(&());
        // Match the constructor's font and metrics, so those transitions do
        // not themselves renew the changed-line marker and mask a bad load.
        content.0.get_mut().editor.update(
            Size::new(800.0, 24.0),
            cosmic::iced::Font::DEFAULT,
            Pixels(1.0),
            LineHeight::Absolute(Pixels(1.0)),
            Wrapping::None,
            &mut highlighter,
        );
        assert_eq!(highlighter.0, Some(0));
        let config = EditorLayout {
            bounds: Size::new(800.0, 24.0),
            font: cosmic::iced::Font::DEFAULT,
            size: Pixels(1.0),
            line_height: LineHeight::Absolute(Pixels(1.0)),
            wrapping: Wrapping::None,
        };
        content.0.get_mut().layout = Some(config);
        content.set_cursor(Cursor {
            position: Position {
                line: 300,
                column: 0,
            },
            selection: Some(Position { line: 0, column: 0 }),
        });
        content.perform(Action::Edit(Edit::Paste(Arc::new("next\n".repeat(200)))));
        content.move_to(Cursor {
            position: Position {
                line: 150,
                column: 0,
            },
            selection: None,
        });
        content.perform(Action::Edit(Edit::Insert('x')));
        highlighter.0 = None;
        // Bounded preparation already used PlainText and these exact metrics.
        // The actual syntax highlighter still needs a fresh line-zero marker.
        content.0.get_mut().editor.update(
            config.bounds,
            config.font,
            config.size,
            config.line_height,
            config.wrapping,
            &mut highlighter,
        );
        assert_eq!(
            highlighter.0,
            Some(150),
            "the queued native edit overwrites its marker"
        );
        content.0.get_mut().finish_layout(&mut highlighter);
        assert_eq!(highlighter.0, Some(0));
    }

    #[test]
    fn large_document_construction_leaves_unvisited_lines_unshaped() {
        let source = "let token = 123; // native fixture\n".repeat(100_000);
        let started = Instant::now();
        let mut content: Content = Content::with_text(&source);
        eprintln!("100k-line construction: {:?}", started.elapsed());
        {
            let internal = content.0.borrow();
            let lines = &internal.editor.buffer().lines;
            assert_eq!(lines.len(), 100_001);
            assert!(lines.iter().all(|line| line.shape_opt().is_none()));
            assert!(lines.iter().all(|line| line.layout_opt().is_none()));
        }
        layout_content(&mut content, 800.0, 240.0);
        let internal = content.0.borrow();
        let lines = &internal.editor.buffer().lines;
        let shaped = lines
            .iter()
            .filter(|line| line.shape_opt().is_some())
            .count();
        assert!(
            (1..100).contains(&shaped),
            "only the viewport needs shapes: {shaped}"
        );
        assert!(lines.last().unwrap().shape_opt().is_none());
        assert!(lines.last().unwrap().layout_opt().is_none());
    }

    #[test]
    fn replacement_output_budget_counts_prefix_tail_and_sparse_aggregate() {
        let original = "λa🦀aZ";
        let snapshot = SearchSnapshot {
            source: TextSnapshot::from_lines([(original, LineEnding::None, "")]),
        };
        let matches = snapshot.find_matches("a", || false).unwrap();
        let expected = "λββ🦀ββZ";
        assert_eq!(
            snapshot
                .replace_matches_bounded(
                    &matches,
                    "a",
                    "ββ",
                    Default::default(),
                    false,
                    || false,
                    expected.len()
                )
                .unwrap(),
            expected
        );
        assert!(
            snapshot
                .replace_matches_bounded(
                    &matches,
                    "a",
                    "ββ",
                    Default::default(),
                    false,
                    || false,
                    expected.len() - 1
                )
                .is_err()
        );
        let edits = snapshot
            .replacement_edits_bounded(
                &matches,
                "a",
                "ββ",
                Default::default(),
                false,
                expected.len(),
            )
            .unwrap();
        assert_eq!(edits.len(), 2);
        assert!(
            snapshot
                .replacement_edits_bounded(
                    &matches,
                    "a",
                    "ββ",
                    Default::default(),
                    false,
                    expected.len() - 1
                )
                .is_err()
        );
        assert_eq!(snapshot.source.text().as_ref(), original);

        // Growth at an early match followed by shrinkage is valid; don't use
        // source-size-plus-intermediate-deltas as the allocation budget.
        let original = "a long";
        let snapshot = SearchSnapshot {
            source: TextSnapshot::from_lines([(original, LineEnding::None, "")]),
        };
        let options = crate::editor_search::FindOptions {
            regex: true,
            ..Default::default()
        };
        let matches = snapshot
            .find_matches_with("(a)|long", options, || false)
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot
                .replace_matches_bounded(
                    &matches,
                    "(a)|long",
                    "$1$1$1",
                    options,
                    false,
                    || false,
                    original.len()
                )
                .unwrap(),
            "aaa "
        );
        assert_eq!(
            snapshot
                .replacement_edits_bounded(
                    &matches,
                    "(a)|long",
                    "$1$1$1",
                    options,
                    false,
                    original.len()
                )
                .unwrap()
                .len(),
            2
        );
        for text in ["${0}${0}${0}", "λ$0🦀"] {
            let expected = regex::Regex::new("(a)|long")
                .unwrap()
                .replace_all(original, text)
                .into_owned();
            assert_eq!(
                snapshot
                    .replace_matches_bounded(
                        &matches,
                        "(a)|long",
                        text,
                        options,
                        false,
                        || false,
                        expected.len()
                    )
                    .unwrap(),
                expected
            );
            assert!(
                snapshot
                    .replace_matches_bounded(
                        &matches,
                        "(a)|long",
                        text,
                        options,
                        false,
                        || false,
                        expected.len() - 1
                    )
                    .is_err()
            );
            assert!(
                snapshot
                    .replacement_edits_bounded(
                        &matches,
                        "(a)|long",
                        text,
                        options,
                        false,
                        expected.len() - 1
                    )
                    .is_err()
            );
            assert_eq!(snapshot.source.text().as_ref(), original);
        }
    }

    #[test]
    fn oversized_existing_buffer_replacements_allow_only_shrink_or_nongrowth() {
        let original = format!(
            "λ{}",
            "x".repeat(crate::editor::MAX_EDITABLE_FILE_BYTES as usize)
        );
        let snapshot = SearchSnapshot {
            source: TextSnapshot::from_lines([(original.as_str(), LineEnding::None, "")]),
        };
        let matches = [(
            Position { line: 0, column: 0 },
            Position { line: 0, column: 2 },
        )];
        for replacement in ["a", "zz"] {
            let output = snapshot
                .replace_matches_with(&matches, "λ", replacement, Default::default(), false)
                .unwrap();
            assert_eq!(output.len(), original.len() - 2 + replacement.len());
            assert!(
                snapshot
                    .replacement_edits(&matches, "λ", replacement, Default::default(), false)
                    .is_ok()
            );
        }
        assert!(
            snapshot
                .replace_matches_with(&matches, "λ", "zzz", Default::default(), false)
                .unwrap_err()
                .contains("result size limit")
        );
        assert!(
            snapshot
                .replacement_edits(&matches, "λ", "zzz", Default::default(), false)
                .is_err()
        );
        assert_eq!(snapshot.source.text().as_ref(), original);
    }

    #[test]
    fn dense_replacement_cancellation_discards_partial_output_and_keeps_source() {
        let text = "λa".repeat(2048);
        let snapshot = SearchSnapshot {
            source: TextSnapshot::from_lines([(text.as_str(), LineEnding::None, "")]),
        };
        let matches = snapshot.find_matches("a", || false).unwrap();
        let checks = std::cell::Cell::new(0);
        let result = snapshot.replace_matches_with_cancellation(
            &matches,
            "a",
            "β",
            Default::default(),
            false,
            || {
                let calls = checks.get() + 1;
                checks.set(calls);
                calls >= 3
            },
        );
        assert_eq!(result.unwrap_err(), "Replacement cancelled.");
        assert_eq!(snapshot.source.text().as_ref(), text);
    }

    #[test]
    #[ignore = "explicit constructor benchmark retains the old full-file glyph caches"]
    fn native_constructor_large_file_microbenchmark() {
        let source = "let token = 123; // native fixture\n".repeat(100_000);
        let started = Instant::now();
        let content: Content = Content::with_text(&source);
        let current_elapsed = started.elapsed();
        let current_shaped = content
            .0
            .borrow()
            .editor
            .buffer()
            .lines
            .iter()
            .filter(|line| line.shape_opt().is_some())
            .count();
        let started = Instant::now();
        let original = NativeEditor::with_text(&source);
        let original_elapsed = started.elapsed();
        let original_shaped = original
            .buffer()
            .lines
            .iter()
            .filter(|line| line.shape_opt().is_some())
            .count();
        eprintln!(
            "100k lines / {} bytes: original {original_elapsed:?}, {original_shaped} shaped; current {current_elapsed:?}, {current_shaped} shaped (unoptimized test profile)",
            source.len()
        );
        assert_eq!(original_shaped, 100_001);
        assert_eq!(current_shaped, 0);
    }

    #[test]
    #[ignore = "explicit full-buffer native paste benchmark"]
    fn native_full_buffer_paste_microbenchmark() {
        for count in [10_000, 50_000, 100_000] {
            let mut content: Content = Content::with_text(&"token\n".repeat(count));
            content.set_cursor(Cursor {
                position: Position {
                    line: count,
                    column: 0,
                },
                selection: Some(Position { line: 0, column: 0 }),
            });
            let replacement = Arc::new("next\n".repeat(count));
            let started = Instant::now();
            let edit = content.perform_edit(Edit::Paste(replacement)).unwrap();
            eprintln!(
                "{count} lines full-buffer perform_edit(Paste): {:?} (unoptimized test profile)",
                started.elapsed()
            );
            assert_eq!(content.text(), "next\n".repeat(count));
            assert_eq!(edit.before.selection, Some(Position { line: 0, column: 0 }));
        }
    }

    #[test]
    fn compact_search_keeps_unicode_regex_scope_and_zero_width_replacements_complete() {
        let text = "é1 λ22\nσ333";
        let snapshot = SearchSnapshot {
            source: TextSnapshot::from_lines([
                ("é1 λ22", LineEnding::Lf, "\n"),
                ("σ333", LineEnding::None, ""),
            ]),
        };
        let options = crate::editor_search::FindOptions {
            regex: true,
            ..Default::default()
        };
        let query = r"(\p{L})(\d+)";
        let matches = snapshot
            .find_matches_with(query, options, || false)
            .unwrap()
            .unwrap();
        assert_eq!(matches.len(), 3);
        assert_eq!(
            snapshot
                .replace_matches_with(&matches, query, "$2:$1", options, false)
                .unwrap(),
            "1:é 22:λ\n333:σ"
        );
        let scoped = matches.scoped(Some((
            Position { line: 0, column: 4 },
            Position { line: 1, column: 5 },
        )));
        assert_eq!(scoped.len(), 2);
        assert_eq!(scoped.allocation_id(), matches.allocation_id());
        assert_eq!(
            snapshot
                .replace_matches_with(&scoped, query, "$2:$1", options, false)
                .unwrap(),
            "é1 22:λ\n333:σ"
        );
        assert_eq!(snapshot.source.text().as_ref(), text);
        let single = SearchSnapshot {
            source: TextSnapshot::from_lines([("aλ b", LineEnding::None, "")]),
        };
        let empty = single
            .find_matches_with(r"\b", options, || false)
            .unwrap()
            .unwrap();
        assert_eq!(empty.len(), 4);
        assert_eq!(
            single
                .replace_matches_with(&empty, r"\b", "|", options, false)
                .unwrap(),
            "|aλ| |b|"
        );
    }

    #[test]
    #[ignore = "explicit dense8MiB match allocation/paint-query/replacement benchmark"]
    fn benchmark_dense_eight_mib_complete_matches_and_visible_queries() {
        use crate::editor_search::{FindOptions, find_in_lines};
        const BYTES: usize = 8 * 1024 * 1024;
        for irregular in [false, true] {
            let text = if irregular {
                format!("{}{}", "aλ".repeat(BYTES / 3), "a".repeat(BYTES % 3))
            } else {
                "a".repeat(BYTES)
            };
            assert_eq!(text.len(), BYTES);
            let options = FindOptions {
                regex: irregular,
                ..Default::default()
            };
            let query = if irregular { "." } else { "a" };
            let expected = if irregular {
                2 * (BYTES / 3) + BYTES % 3
            } else {
                BYTES
            };
            let started = Instant::now();
            let matches = find_in_lines(std::iter::once(text.as_str()), query, options, || false)
                .unwrap()
                .unwrap();
            let search_time = started.elapsed();
            assert_eq!(matches.len(), expected);
            let retained = matches.retained_bytes();
            assert!(retained < if irregular { 6 * BYTES } else { 1024 });
            MatchIndex::take_query_runs();
            let paint_started = Instant::now();
            for column in BYTES / 2..BYTES / 2 + 80 {
                let (_first, hits) = matches.visible(0, column, column + 1);
                assert!(hits.len() <= 2);
            }
            let query_time = paint_started.elapsed();
            let decoded = MatchIndex::take_query_runs();
            assert!(
                decoded < 400_000,
                "eighty visible glyph queries must not walk millions of matches"
            );
            let snapshot = SearchSnapshot {
                source: TextSnapshot::from_lines([(text.as_str(), LineEnding::None, "")]),
            };
            let replace_started = Instant::now();
            let replaced = snapshot
                .replace_matches_with(&matches, query, "b", options, false)
                .unwrap();
            let replace_time = replace_started.elapsed();
            assert_eq!(replaced.len(), expected);
            assert!(replaced.bytes().all(|byte| byte == b'b'));
            eprintln!(
                "find8MiB irregular={irregular} matches={expected} retained_index_bytes={retained} prior_pair_payload_bytes={} query_decoded_runs={decoded} search={search_time:?} visible80={query_time:?} replace_all={replace_time:?}; native glyph-layout traversal and processRSS are separate",
                expected * std::mem::size_of::<(Position, Position)>()
            );
        }
    }

    #[test]
    fn capturing_and_searching_large_snapshot_never_materializes_whole_text() {
        let source = "let target = 1;\n".repeat(50_000);
        let content: Content = Content::with_text(&source);
        take_text_call_count();
        let snapshot = content.search_snapshot();
        assert!(!snapshot.source.is_materialized());
        assert_eq!(
            snapshot.find_matches("target", || false).unwrap().len(),
            50_000
        );
        assert_eq!(
            snapshot
                .find_matches_with(
                    "tar(get)",
                    crate::editor_search::FindOptions {
                        regex: true,
                        ..Default::default()
                    },
                    || false
                )
                .unwrap()
                .unwrap()
                .len(),
            50_000
        );
        let edits = snapshot
            .replacement_edits(
                &[
                    (
                        Position { line: 0, column: 4 },
                        Position {
                            line: 0,
                            column: 10,
                        },
                    ),
                    (
                        Position {
                            line: 49_999,
                            column: 4,
                        },
                        Position {
                            line: 49_999,
                            column: 10,
                        },
                    ),
                ],
                "tar(get)",
                "new$1",
                crate::editor_search::FindOptions {
                    regex: true,
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().all(|(_, _, text)| text == "newget"));
        assert!(!snapshot.source.is_materialized());
        assert_eq!(take_text_call_count(), 0);
    }

    #[test]
    fn search_snapshot_preserves_native_positions_unicode_and_mixed_endings() {
        let source = "Straße\rSTRASSE\r\ncafé CAFÉ\n\rtail";
        let content: Content = Content::with_text(source);
        let snapshot = content.search_snapshot();
        assert_eq!(snapshot.source.text().as_ref(), content.text());
        for query in ["strasse", "café", " ", "tail", "s", ""] {
            assert_eq!(
                snapshot.find_matches(query, || false).unwrap(),
                content.find_matches(query)
            );
        }
        let matches = snapshot.find_matches("strasse", || false).unwrap();
        assert_eq!(
            snapshot.replace_matches(&matches, "road\nnext").unwrap(),
            "road\nnext\rroad\nnext\r\ncafé CAFÉ\n\rtail"
        );
    }

    #[test]
    fn replace_all_builds_a_large_changed_length_document_in_source_order() {
        let text = "token\n".repeat(100_000);
        let content: Content = Content::with_text(&text);
        let snapshot = content.search_snapshot();
        let matches = snapshot.find_matches("token", || false).unwrap();
        assert_eq!(matches.len(), 100_000);
        assert_eq!(
            snapshot
                .replace_matches(&matches, "longer replacement")
                .unwrap(),
            "longer replacement\n".repeat(100_000)
        );
        assert_eq!(
            snapshot.replace_matches(&matches, "").unwrap(),
            "\n".repeat(100_000)
        );
    }

    #[test]
    fn snapshot_search_cancels_inside_a_single_long_line() {
        let content: Content = Content::with_text(&"Straße ".repeat(100_000));
        let snapshot = content.search_snapshot();
        let checks = std::cell::Cell::new(0);
        assert!(
            snapshot
                .find_matches("strasse", || {
                    checks.set(checks.get() + 1);
                    checks.get() == 3
                })
                .is_none()
        );
        assert_eq!(checks.get(), 3);
    }

    #[test]
    fn replace_matches_rejects_overlapping_or_invalid_unicode_ranges() {
        let content: Content = Content::with_text("café");
        let snapshot = content.search_snapshot();
        let at = |column| Position { line: 0, column };
        for matches in [
            vec![(at(0), at(3)), (at(2), at(5))],
            vec![(at(0), at(4))],
            vec![(at(9), at(9))],
        ] {
            assert!(snapshot.replace_matches(&matches, "x").is_none());
        }
    }

    #[test]
    fn find_skips_partial_fold_hits_without_losing_overlapping_source_matches() {
        let source = "sß\nß\nsﬃ\niss";
        let content: Content = Content::with_text(source);
        let at = |line, column| Position { line, column };
        let expected = vec![
            (at(0, 1), at(0, 3)),
            (at(1, 0), at(1, 2)),
            (at(3, 1), at(3, 3)),
        ];
        assert_eq!(content.find_matches("ss"), expected);
        assert_eq!(
            content.search_snapshot().find_matches("ss", || false),
            Some(expected.into_iter().collect::<MatchIndex>())
        );
        assert_eq!(content.find_matches("ffi"), vec![(at(2, 1), at(2, 4))]);
        let partial: Content = Content::with_text("ß");
        assert!(partial.find_matches("s").is_empty());
        let ascii: Content = Content::with_text("sss");
        assert_eq!(ascii.find_matches("ss"), vec![(at(0, 0), at(0, 2))]);
    }

    #[test]
    fn find_casefolds_unicode_preserves_whitespace_and_source_offsets() {
        let content: Content = Content::with_text("Straße\n  café CAFÉ\nİ");
        assert_eq!(
            content.find_matches("STRASSE"),
            vec![(
                Position { line: 0, column: 0 },
                Position { line: 0, column: 7 }
            )]
        );
        assert_eq!(content.find_matches(" ").len(), 3);
        assert_eq!(content.find_matches("CAFÉ").len(), 2);
        assert!(content.find_matches("").is_empty());
        // A match must cover a complete original character, even when folding
        // expands it into more than one scalar.
        assert!(content.find_matches("i").is_empty());
    }

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
        // Mirror production layout: the widget syncs its wrapping mode into
        // content before shaping or seeking, and scroll canonicalization
        // reads it back.
        content.0.get_mut().wrapping = Wrapping::Word;
        let internal = content.0.get_mut();
        internal.layout = Some(EditorLayout {
            bounds: Size::new(width, height),
            font: cosmic::iced::Font::MONOSPACE,
            size: Pixels(16.0),
            line_height: LineHeight::Absolute(Pixels(20.0)),
            wrapping: Wrapping::Word,
        });
        let previous = internal.pending_edit_scroll.take();
        let lines = std::mem::take(&mut internal.pending_edit_scroll_lines);
        shape_after_splice(
            &mut internal.editor,
            previous,
            lines,
            Wrapping::Word,
            |editor| {
                editor.update(
                    Size::new(width, height),
                    cosmic::iced::Font::MONOSPACE,
                    Pixels(16.0),
                    LineHeight::Absolute(Pixels(20.0)),
                    Wrapping::Word,
                    &mut highlighter::PlainText::new(&()),
                )
            },
        );
    }

    fn seek_content(content: &mut Content, target: ScrollTarget, width: f32, height: f32) -> usize {
        let mut shapes = 0;
        let mut highlighter = highlighter::PlainText::new(&());
        // Mirror production layout (see above): this helper shapes with Word
        // wrapping, so content must agree before seeking.
        content.0.get_mut().wrapping = Wrapping::Word;
        seek_scroll(
            &mut content.0.get_mut().editor,
            target,
            Wrapping::Word,
            |editor| {
                shapes += 1;
                editor.update(
                    Size::new(width, height),
                    cosmic::iced::Font::MONOSPACE,
                    Pixels(16.0),
                    LineHeight::Absolute(Pixels(20.0)),
                    Wrapping::Word,
                    &mut highlighter,
                );
            },
        );
        shapes
    }

    #[test]
    fn short_files_hide_the_scrollbar_while_long_files_scroll() {
        // Files that fit the viewport must not paint a scrollbar track; only
        // overflowing content scrolls. `draw` early-returns on `scrollable`,
        // so this geometry flag is the whole visibility contract.
        for source in [
            "".to_owned(),
            "one\n".to_owned(),
            "one\ntwo\nthree\n".to_owned(),
        ] {
            let mut content = Content::with_text(&source);
            layout_content(&mut content, 500.0, 200.0);
            for height in [211.0, 1000.7, 1762.0] {
                let bounds = Rectangle::new(Point::ORIGIN, Size::new(600.0, height));
                let editor: TextEditor<'_, highlighter::PlainText, ()> = TextEditor::new(&content);
                assert!(
                    !editor.scroll_geometry(bounds).scrollable,
                    "short file scrolls at height {height}"
                );
            }
        }
        let source = (1..=200).map(|i| format!("line {i}\n")).collect::<String>();
        let mut content = Content::with_text(&source);
        layout_content(&mut content, 500.0, 200.0);
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(600.0, 210.0));
        let editor: TextEditor<'_, highlighter::PlainText, ()> = TextEditor::new(&content);
        assert!(editor.scroll_geometry(bounds).scrollable);
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
        let renderer = headless_renderer();
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
        let fit = ScrollGeometry::new(bounds, bounds, 1.0, 0.0, 1.0, 200.0, 0.0);
        assert!(!fit.scrollable);
        assert_eq!(fit.fraction_at(500.0, 0.0), 0.0);
        assert!(fit.thumb.height <= bounds.height);
        assert!(
            fit.horizontal.is_none(),
            "a canvas that fits needs no horizontal track"
        );
        let bounds = Rectangle {
            height: 200.0,
            ..bounds
        };
        let scrolling = ScrollGeometry::new(bounds, bounds, 100.0, 40.0, 10.0, 200.0, 0.0);
        assert_eq!(scrolling.fraction_at(-500.0, 0.0), 0.0);
        assert_eq!(scrolling.fraction_at(500.0, 0.0), 1.0);

        // A canvas wider than the viewport gets a track whose thumb maps the
        // pointer back onto the full offset range in both directions.
        let wide = ScrollGeometry::new(bounds, bounds, 1.0, 0.0, 1.0, 600.0, 400.0)
            .horizontal
            .expect("a canvas three viewports wide must be pannable");
        assert_eq!(wide.maximum, 400.0);
        assert!(wide.thumb.width < wide.track.width);
        assert!(
            (wide.thumb.x + wide.thumb.width - (wide.track.x + wide.track.width)).abs() < 0.01,
            "a fully panned canvas puts the thumb at the end of its track"
        );
        assert_eq!(wide.offset_at(-500.0, 0.0), 0.0);
        assert_eq!(wide.offset_at(500.0, 0.0), 400.0);
        assert!(wide.track.y >= bounds.y + bounds.height - crate::design::SCROLLBAR_GUTTER);
    }

    /// A software renderer, which is all the editor needs: shaping, glyph
    /// metrics and hit testing come from cosmic-text, not the GPU.
    fn headless_renderer() -> cosmic::Renderer {
        use cosmic::iced::advanced::renderer::Headless;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(cosmic::Renderer::new(
                cosmic::iced::Font::MONOSPACE,
                Pixels(16.0),
                Some("tiny-skia"),
            ))
            .unwrap()
    }

    fn pointer_event(
        editor: &mut TextEditor<'_, highlighter::PlainText, Action>,
        tree: &mut widget::Tree,
        node: &layout::Node,
        renderer: &cosmic::Renderer,
        input: (mouse::Event, Point),
        viewport: &Rectangle,
        published: &mut Vec<Action>,
    ) -> bool {
        let mut shell = Shell::new(published);
        editor.update(
            tree,
            &Event::Mouse(input.0),
            Layout::new(node),
            mouse::Cursor::Available(input.1),
            renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut shell,
            viewport,
        );
        shell.is_event_captured()
    }

    #[test]
    fn clipped_and_external_presses_blur_without_publishing_editor_actions() {
        let renderer = headless_renderer();
        let size = Size::new(600.0, 200.0);
        let viewport = Rectangle::with_size(Size::new(250.0, 200.0));
        for point in [Point::new(400.0, 60.0), Point::new(650.0, 60.0)] {
            for button in [
                mouse::Button::Left,
                mouse::Button::Middle,
                mouse::Button::Right,
            ] {
                let mut content = Content::with_text("selected text\n");
                content.set_cursor(Cursor {
                    position: Position { line: 0, column: 8 },
                    selection: Some(Position { line: 0, column: 0 }),
                });
                let mut editor = wheel_editor(&content, Wrapping::None);
                let mut tree = widget::Tree::new(
                    &editor as &dyn Widget<Action, cosmic::Theme, cosmic::Renderer>,
                );
                let node =
                    editor.layout(&mut tree, &renderer, &layout::Limits::new(Size::ZERO, size));
                tree.state
                    .downcast_mut::<State<highlighter::PlainText>>()
                    .focus = Some(Focus::now());
                let before = (content.text(), content.cursor(), content.selection());
                let mut actions = Vec::new();
                let captured = pointer_event(
                    &mut editor,
                    &mut tree,
                    &node,
                    &renderer,
                    (mouse::Event::ButtonPressed(button), point),
                    &viewport,
                    &mut actions,
                );
                assert!(actions.is_empty(), "outside {button:?} emitted {actions:?}");
                assert!(!captured, "an external control owns the press");
                let state = tree.state.downcast_ref::<State<highlighter::PlainText>>();
                assert!(state.focus.is_none());
                assert!(state.drag_click.is_none());
                assert!(state.context_menu_position.is_none());
                assert_eq!(
                    (content.text(), content.cursor(), content.selection()),
                    before
                );
            }
        }
    }

    #[test]
    fn visible_pointer_presses_keep_editor_and_context_menu_behavior() {
        let renderer = headless_renderer();
        let viewport = Rectangle::with_size(Size::new(600.0, 200.0));
        for button in [
            mouse::Button::Left,
            mouse::Button::Middle,
            mouse::Button::Right,
        ] {
            let content = Content::with_text("visible text\n");
            let mut editor = wheel_editor(&content, Wrapping::None);
            let mut tree =
                widget::Tree::new(&editor as &dyn Widget<Action, cosmic::Theme, cosmic::Renderer>);
            let node = editor.layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            let mut actions = Vec::new();
            assert!(pointer_event(
                &mut editor,
                &mut tree,
                &node,
                &renderer,
                (mouse::Event::ButtonPressed(button), Point::new(60.0, 30.0)),
                &viewport,
                &mut actions,
            ));
            let state = tree.state.downcast_ref::<State<highlighter::PlainText>>();
            assert!(state.focus.is_some());
            if button == mouse::Button::Right {
                assert!(actions.is_empty());
                assert!(state.context_menu_position.is_some());
            } else {
                assert!(matches!(actions.as_slice(), [Action::Click(_)]));
                assert!(state.context_menu_position.is_none());
            }
        }
    }

    #[test]
    fn explicit_focus_operation_still_requests_selection_clearing() {
        let renderer = headless_renderer();
        let content = Content::with_text("selection\n");
        let mut editor = wheel_editor(&content, Wrapping::None);
        let mut tree =
            widget::Tree::new(&editor as &dyn Widget<Action, cosmic::Theme, cosmic::Renderer>);
        let viewport = Rectangle::with_size(Size::new(600.0, 200.0));
        let node = editor.layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        operation::Focusable::unfocus(tree.state.downcast_mut::<State<highlighter::PlainText>>());
        let mut actions = Vec::new();
        pointer_event(
            &mut editor,
            &mut tree,
            &node,
            &renderer,
            (
                mouse::Event::CursorMoved {
                    position: Point::new(30.0, 30.0),
                },
                Point::new(30.0, 30.0),
            ),
            &viewport,
            &mut actions,
        );
        assert!(matches!(actions.as_slice(), [Action::ClearSelection]));
    }

    #[test]
    fn admitted_drag_and_scrollbar_grabs_settle_outside_the_viewport() {
        let renderer = headless_renderer();
        let size = Size::new(600.0, 200.0);
        for kind in 0..3 {
            let source = format!("{}\n", "wide text ".repeat(150)).repeat(100);
            let content = Content::with_text(&source);
            let mut editor = wheel_editor(&content, Wrapping::None);
            let mut tree =
                widget::Tree::new(&editor as &dyn Widget<Action, cosmic::Theme, cosmic::Renderer>);
            let node = editor.layout(&mut tree, &renderer, &layout::Limits::new(Size::ZERO, size));
            let geometry = editor.scroll_geometry(node.bounds());
            let (start, outside, viewport) = match kind {
                0 => (
                    Point::new(30.0, 30.0),
                    Point::new(400.0, 60.0),
                    Rectangle::with_size(Size::new(250.0, 200.0)),
                ),
                1 => (
                    geometry.thumb.center(),
                    Point::new(geometry.track.center_x(), 180.0),
                    Rectangle::with_size(Size::new(600.0, 100.0)),
                ),
                _ => {
                    let horizontal = geometry.horizontal.expect("wide content has a thumb");
                    (
                        horizontal.thumb.center(),
                        Point::new(500.0, horizontal.track.center_y()),
                        Rectangle::with_size(Size::new(250.0, 200.0)),
                    )
                }
            };
            assert!(viewport.contains(start));
            assert!(!viewport.contains(outside));
            let mut actions = Vec::new();
            assert!(pointer_event(
                &mut editor,
                &mut tree,
                &node,
                &renderer,
                (mouse::Event::ButtonPressed(mouse::Button::Left), start),
                &viewport,
                &mut actions,
            ));
            actions.clear();
            let before_horizontal = content.0.borrow().horizontal;
            pointer_event(
                &mut editor,
                &mut tree,
                &node,
                &renderer,
                (mouse::Event::CursorMoved { position: outside }, outside),
                &viewport,
                &mut actions,
            );
            let state = tree.state.downcast_ref::<State<highlighter::PlainText>>();
            match kind {
                0 => assert!(matches!(actions.as_slice(), [Action::Drag(_)])),
                1 => {
                    assert!(state.scrollbar_grab.is_some());
                    assert!(
                        matches!(state.scrollbar_target, Some(ScrollTarget::Fraction { fraction, .. }) if fraction > 0.0)
                    );
                }
                _ => {
                    assert!(state.horizontal_grab.is_some());
                    assert!(content.0.borrow().horizontal > before_horizontal);
                }
            }
            pointer_event(
                &mut editor,
                &mut tree,
                &node,
                &renderer,
                (mouse::Event::ButtonReleased(mouse::Button::Left), outside),
                &viewport,
                &mut actions,
            );
            let state = tree.state.downcast_ref::<State<highlighter::PlainText>>();
            assert!(state.drag_click.is_none());
            assert!(state.scrollbar_grab.is_none());
            assert!(state.horizontal_grab.is_none());
        }
    }

    #[test]
    fn completion_and_hover_overlays_follow_the_scrolled_pane() {
        let renderer = headless_renderer();
        let content: Content = Content::with_text("fn main() {}\n");
        let viewport = Rectangle::new(Point::ORIGIN, Size::new(420.0, 300.0));
        for at in [None, Some(Position { line: 0, column: 3 })] {
            let mut editor: TextEditor<'_, highlighter::PlainText, ()> = TextEditor::new(&content)
                .size(16.0)
                .height(Length::Fill)
                .popup(Some(cosmic::widget::text("Native result").into()), at);
            let mut tree =
                widget::Tree::new(&editor as &dyn Widget<(), cosmic::Theme, cosmic::Renderer>);
            let node = editor
                .layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, viewport.size()),
                )
                .move_to(Point::new(1000.0, 40.0));
            let mut popup = editor
                .overlay(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &viewport,
                    Vector::new(-900.0, 0.0),
                )
                .expect("the translated pane's visible anchor must retain its popup");
            let popup_layout = popup.as_overlay_mut().layout(&renderer, viewport.size());
            assert!(viewport.contains(popup_layout.bounds().position()));
            assert!(popup_layout.bounds().x >= 100.0);
            drop(popup);
            assert!(
                editor
                    .overlay(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        &viewport,
                        Vector::ZERO,
                    )
                    .is_none(),
                "an offscreen pane must not raise a popup"
            );
        }
    }

    /// The viewport every wheel test lays out and points its cursor into.
    const WHEEL_VIEWPORT: Size = Size {
        width: 260.0,
        height: 200.0,
    };

    /// Drives the real widget so the assertions read the offset the editor
    /// will draw and the scroll it asked the application to perform, rather
    /// than any intermediate the conversion happens to keep.
    fn wheel_editor<'a>(
        content: &'a Content,
        wrapping: Wrapping,
    ) -> TextEditor<'a, highlighter::PlainText, Action> {
        TextEditor::new(content)
            .font(cosmic::iced::Font::MONOSPACE)
            .size(16.0)
            .wrapping(wrapping)
            .height(Length::Fill)
            .on_action(|action| action)
    }

    #[test]
    fn shift_wheel_pans_the_unwrapped_canvas_to_both_bounds() {
        let renderer = headless_renderer();
        let content = Content::with_text(&"abcdefghij".repeat(30));
        let mut widget = wheel_editor(&content, Wrapping::None);
        let mut tree =
            widget::Tree::new(&widget as &dyn Widget<Action, cosmic::Theme, cosmic::Renderer>);
        let bounds = Rectangle::new(Point::ORIGIN, WHEEL_VIEWPORT);
        let node = widget.layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, WHEEL_VIEWPORT),
        );
        let mut published = Vec::new();
        let feed = |widget: &mut TextEditor<'_, highlighter::PlainText, Action>,
                    tree: &mut widget::Tree,
                    published: &mut Vec<Action>,
                    event: Event| {
            let mut clipboard = cosmic::iced::advanced::clipboard::Null;
            let mut shell = Shell::new(published);
            widget.update(
                tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(Point::new(100.0, 100.0)),
                &renderer,
                &mut clipboard,
                &mut shell,
                &bounds,
            );
        };
        let wheel = |delta| Event::Mouse(mouse::Event::WheelScrolled { delta });
        let modifiers = |modifiers| Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers));
        let vertical = |published: &Vec<Action>| match published.last() {
            Some(Action::Scroll { lines }) => *lines,
            other => panic!("a wheel over the editor must publish a scroll, got {other:?}"),
        };

        assert_eq!(content.scroll_position().horizontal, 0.0);
        feed(
            &mut widget,
            &mut tree,
            &mut published,
            modifiers(keyboard::Modifiers::SHIFT),
        );
        feed(
            &mut widget,
            &mut tree,
            &mut published,
            wheel(mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 }),
        );
        assert_eq!(content.scroll_position().horizontal, 60.0);
        assert_eq!(vertical(&published), 0);

        feed(
            &mut widget,
            &mut tree,
            &mut published,
            wheel(mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 }),
        );
        assert_eq!(
            content.scroll_position().horizontal,
            0.0,
            "Shift must pan both ways"
        );

        // A real horizontal component wins outright, so a diagonal lands on
        // its own `x` rather than a sum. `interaction::wheel_as_horizontal`
        // documents why: summing cancels an opposite-sign diagonal exactly.
        feed(
            &mut widget,
            &mut tree,
            &mut published,
            wheel(mouse::ScrollDelta::Pixels { x: -8.0, y: -4.0 }),
        );
        assert_eq!(content.scroll_position().horizontal, 8.0);

        // The gesture a sum would discard. Opposite-sign components must still
        // pan by `x`, never cancel to a dead gesture.
        feed(
            &mut widget,
            &mut tree,
            &mut published,
            wheel(mouse::ScrollDelta::Pixels { x: -6.0, y: 6.0 }),
        );
        assert_eq!(
            content.scroll_position().horizontal,
            14.0,
            "an opposite-sign diagonal must not cancel itself"
        );

        // High-resolution deltas stay continuous instead of quantizing to a
        // wheel notch or a line.
        for _ in 0..4 {
            feed(
                &mut widget,
                &mut tree,
                &mut published,
                wheel(mouse::ScrollDelta::Pixels { x: 0.0, y: -0.5 }),
            );
        }
        assert_eq!(content.scroll_position().horizontal, 16.0);

        // The wheel must stop exactly where a thumb dragged to the end of its
        // track stops, so the two routes cannot disagree about the canvas.
        let maximum = widget
            .scroll_geometry(bounds)
            .horizontal
            .expect("a long unwrapped line must be pannable")
            .maximum;
        assert!(maximum > 0.0);
        feed(
            &mut widget,
            &mut tree,
            &mut published,
            wheel(mouse::ScrollDelta::Pixels {
                x: 0.0,
                y: -100_000.0,
            }),
        );
        assert_eq!(content.scroll_position().horizontal, maximum);
        feed(
            &mut widget,
            &mut tree,
            &mut published,
            wheel(mouse::ScrollDelta::Pixels {
                x: 0.0,
                y: 100_000.0,
            }),
        );
        assert_eq!(content.scroll_position().horizontal, 0.0);

        // Releasing Shift restores plain vertical scrolling, and the wheel's
        // own horizontal delta keeps panning without a modifier.
        feed(
            &mut widget,
            &mut tree,
            &mut published,
            modifiers(keyboard::Modifiers::default()),
        );
        feed(
            &mut widget,
            &mut tree,
            &mut published,
            wheel(mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 }),
        );
        assert_eq!(content.scroll_position().horizontal, 0.0);
        assert!(vertical(&published) > 0);
        feed(
            &mut widget,
            &mut tree,
            &mut published,
            wheel(mouse::ScrollDelta::Lines { x: -1.0, y: 0.0 }),
        );
        assert_eq!(content.scroll_position().horizontal, 60.0);
        assert_eq!(vertical(&published), 0);
    }

    #[test]
    fn wrapped_documents_gain_no_horizontal_range_from_the_wheel() {
        let renderer = headless_renderer();
        let content = Content::with_text(&"wrapped words ".repeat(200));
        let mut widget = wheel_editor(&content, Wrapping::Word);
        let mut tree =
            widget::Tree::new(&widget as &dyn Widget<Action, cosmic::Theme, cosmic::Renderer>);
        let bounds = Rectangle::new(Point::ORIGIN, WHEEL_VIEWPORT);
        let node = widget.layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, WHEEL_VIEWPORT),
        );
        assert!(
            widget.scroll_geometry(bounds).horizontal.is_none(),
            "wrapped text has no canvas to pan"
        );
        let mut published = Vec::new();
        for event in [
            Event::Keyboard(keyboard::Event::ModifiersChanged(
                keyboard::Modifiers::SHIFT,
            )),
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -120.0 },
            }),
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: -120.0, y: 0.0 },
            }),
        ] {
            let mut clipboard = cosmic::iced::advanced::clipboard::Null;
            let mut shell = Shell::new(&mut published);
            widget.update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(Point::new(100.0, 100.0)),
                &renderer,
                &mut clipboard,
                &mut shell,
                &bounds,
            );
            assert_eq!(content.scroll_position().horizontal, 0.0);
        }
        // A wrapped buffer has no canvas, so the redirect has nowhere to go -
        // and a gesture the user modified onto the horizontal axis must then
        // do nothing at all rather than move the document vertically.
        assert_eq!(
            published
                .iter()
                .filter_map(|action| match action {
                    Action::Scroll { lines } => Some(*lines),
                    _ => None,
                })
                .sum::<i32>(),
            0,
            "Shift+wheel must never scroll vertically, even with nothing to pan"
        );
    }

    /// A high-resolution trackpad reports a small nonzero `x` on a drag that is
    /// plainly vertical. Testing the horizontal component for exact zero would
    /// hand the whole gesture to that jitter, panning by a fraction of a pixel
    /// while the vertical delta was discarded.
    #[test]
    fn a_mostly_vertical_trackpad_drag_still_pans_by_its_dominant_axis() {
        let renderer = headless_renderer();
        let content = Content::with_text(&"abcdefghij".repeat(30));
        let mut widget = wheel_editor(&content, Wrapping::None);
        let mut tree =
            widget::Tree::new(&widget as &dyn Widget<Action, cosmic::Theme, cosmic::Renderer>);
        let bounds = Rectangle::new(Point::ORIGIN, WHEEL_VIEWPORT);
        let node = widget.layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, WHEEL_VIEWPORT),
        );
        let mut published = Vec::new();
        for event in [
            Event::Keyboard(keyboard::Event::ModifiersChanged(
                keyboard::Modifiers::SHIFT,
            )),
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: -0.4, y: -40.0 },
            }),
        ] {
            let mut clipboard = cosmic::iced::advanced::clipboard::Null;
            let mut shell = Shell::new(&mut published);
            widget.update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(Point::new(100.0, 100.0)),
                &renderer,
                &mut clipboard,
                &mut shell,
                &bounds,
            );
        }
        assert_eq!(
            content.scroll_position().horizontal,
            40.0,
            "the dominant axis decides the pan, not a nonzero test on x"
        );
    }

    #[test]
    fn a_selection_search_window_ending_inside_a_character_still_finds_it() {
        // A word selection reports only the point it was made at; the search
        // for its span looks the selection's length in bytes past that
        // point, which here ends inside the three-byte dash.
        let text = "ab\u{2014}\n";
        let mut content: Content = Content::with_text(text);
        content.set_cursor(Cursor {
            position: Position { line: 0, column: 1 },
            selection: None,
        });
        content.perform(Action::SelectWord);
        assert_eq!(content.selection().as_deref(), Some("ab"));
        assert_eq!(
            content.selected_span(&Excerpt::document(text), content.cursor()),
            Cursor {
                position: Position { line: 0, column: 2 },
                selection: Some(Position { line: 0, column: 0 }),
            }
        );
    }

    /// Every kind of edit the native buffer performs, at random places and
    /// over random selections, is recorded exactly: undoing the whole
    /// history reproduces the original bytes, including mixed line endings,
    /// and redoing it reproduces the final ones.
    #[test]
    fn recorded_native_edits_undo_and_redo_byte_for_byte() {
        use history::{EditKind, History, Replay};
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound.max(1) as u64) as usize
        };
        let original = "fn main() {\r\n    let é = 1;\n\n\tprintln!(\"漢字\");\r\n}\n";
        let native_lines = |content: &Content| {
            (0..content.line_count())
                .map(|index| {
                    let line = content.line(index).unwrap();
                    (line.text.into_owned(), line.ending)
                })
                .collect::<Vec<_>>()
        };
        let start = Instant::now();
        for round in 0..60 {
            let mut content: Content = Content::with_text(original);
            let original_lines = native_lines(&content);
            let mut history = History::default();
            for step in 0..30 {
                let lines = content.line_count();
                let pick = |line: usize, column: usize, content: &Content| {
                    let line = line % lines;
                    let text = content.line(line).unwrap().text.into_owned();
                    let mut column = column % (text.len() + 1);
                    while !text.is_char_boundary(column) {
                        column -= 1;
                    }
                    Position { line, column }
                };
                // Half the time the caret moves, which seals the step the way
                // the editor does; otherwise the edit continues from where the
                // last one left it and may coalesce into its step.
                if step == 0 || next(2) == 0 {
                    let position = pick(next(lines), next(40), &content);
                    let selection = (next(3) == 0).then(|| pick(next(lines), next(40), &content));
                    content.set_cursor(Cursor {
                        position,
                        selection,
                    });
                    history.seal();
                }
                let selection = content.cursor().selection;
                // Unindent is left out: cosmic-text 0.19 underflows its caret
                // when unindenting with the caret inside the indentation,
                // and nothing in the application binds it.
                let edit = match next(7) {
                    0 | 1 => Edit::Insert(['a', ' ', 'é', '漢', '\t', 'z'][next(6)]),
                    2 => Edit::Enter,
                    3 => Edit::Backspace,
                    4 => Edit::Delete,
                    5 => Edit::Paste(Arc::new(
                        ["x", "two\nlines", "crlf\r\nend", "\n", "tab\t"][next(5)].to_owned(),
                    )),
                    _ => Edit::Indent,
                };
                let kind = match &edit {
                    Edit::Insert(_) | Edit::Enter => EditKind::Insert,
                    Edit::Backspace if selection.is_none() => EditKind::DeleteBackward,
                    Edit::Delete if selection.is_none() => EditKind::DeleteForward,
                    _ => EditKind::Other,
                };
                let now = start + Duration::from_millis((round * 100_000 + step * 200) as u64);
                if let Some(edit) = content.perform_edit(edit) {
                    history.record(edit.change, kind, edit.before, content.cursor(), now);
                }
                assert_eq!(
                    content.text_snapshot().text().as_ref(),
                    content.text(),
                    "snapshot round {round} step {step}"
                );
                assert_eq!(content.text_len(), content.text().len());
            }
            let last = content.text();
            let last_lines = native_lines(&content);
            while history.undo(&mut content) == Replay::Applied {
                assert_eq!(content.text_snapshot().text().as_ref(), content.text());
            }
            assert_eq!(content.text(), original, "round {round}");
            assert_eq!(
                native_lines(&content),
                original_lines,
                "native undo, round {round}"
            );
            while history.redo(&mut content) == Replay::Applied {
                assert_eq!(content.text_snapshot().text().as_ref(), content.text());
            }
            assert_eq!(content.text(), last, "round {round}");
            assert_eq!(
                native_lines(&content),
                last_lines,
                "native redo, round {round}"
            );
        }
    }

    #[test]
    fn deleting_lf_then_crlf_preserves_native_line_boundaries_on_replay() {
        use history::{EditKind, History, Replay};
        let original = "a\r\nb\n";
        let mut content: Content = Content::with_text(original);
        let mut history = History::default();
        let cursor = Cursor {
            position: Position { line: 0, column: 1 },
            selection: None,
        };
        content.set_cursor(cursor);
        let now = Instant::now();
        let edit = content.perform_edit(Edit::Enter).unwrap();
        history.record(
            edit.change,
            EditKind::Insert,
            edit.before,
            content.cursor(),
            now,
        );
        content.set_cursor(cursor);
        history.seal();
        for _ in 0..2 {
            let edit = content.perform_edit(Edit::Delete).unwrap();
            history.record(
                edit.change,
                EditKind::DeleteForward,
                edit.before,
                content.cursor(),
                now,
            );
        }
        assert_eq!(content.text(), "ab\n");
        assert_eq!(history.undo(&mut content), Replay::Applied);
        assert_eq!(history.undo(&mut content), Replay::Applied);
        assert_eq!(history.undo(&mut content), Replay::Applied);
        assert_eq!(content.text(), original);
        assert_eq!(history.undo(&mut content), Replay::Empty);
        assert_eq!(history.redo(&mut content), Replay::Applied);
        assert_eq!(history.redo(&mut content), Replay::Applied);
        assert_eq!(history.redo(&mut content), Replay::Applied);
        assert_eq!(content.text(), "ab\n");
    }

    #[test]
    fn replacing_multiple_native_lines_preserves_their_boundaries_on_replay() {
        use history::{EditKind, History, Replay};
        let original = "a\r\nb\n";
        let mut content: Content = Content::with_text(original);
        let mut history = History::default();
        content.set_cursor(Cursor {
            position: Position { line: 0, column: 1 },
            selection: None,
        });
        let now = Instant::now();
        let edit = content.perform_edit(Edit::Enter).unwrap();
        history.record(
            edit.change,
            EditKind::Insert,
            edit.before,
            content.cursor(),
            now,
        );
        content.set_cursor(Cursor {
            position: Position { line: 2, column: 1 },
            selection: Some(Position { line: 0, column: 1 }),
        });
        let edit = content.perform_edit(Edit::Delete).unwrap();
        assert_eq!(edit.change.removed_breaks, [2, 4, 6]);
        history.record(
            edit.change,
            EditKind::Other,
            edit.before,
            content.cursor(),
            now,
        );
        assert_eq!(content.text(), "a\n");
        assert_eq!(history.undo(&mut content), Replay::Applied);
        assert_eq!(content.text(), "a\n\r\nb\n");
        assert_eq!(history.undo(&mut content), Replay::Applied);
        assert_eq!(content.text(), original);
        assert_eq!(history.redo(&mut content), Replay::Applied);
        assert_eq!(history.redo(&mut content), Replay::Applied);
        assert_eq!(content.text(), "a\n");
    }

    #[test]
    fn a_typed_run_is_one_undo_step_that_restores_the_caret() {
        use history::{EditKind, History, Replay};
        let mut content: Content = Content::with_text("let x;\n");
        let mut history = History::default();
        content.set_cursor(Cursor {
            position: Position { line: 0, column: 5 },
            selection: None,
        });
        let now = Instant::now();
        for character in "yz".chars() {
            let edit = content.perform_edit(Edit::Insert(character)).unwrap();
            history.record(
                edit.change,
                EditKind::Insert,
                edit.before,
                content.cursor(),
                now,
            );
        }
        assert_eq!(content.text(), "let xyz;\n");
        assert_eq!(history.undo(&mut content), Replay::Applied);
        assert_eq!(content.text(), "let x;\n");
        assert_eq!(content.cursor().position, Position { line: 0, column: 5 });
        assert_eq!(history.undo(&mut content), Replay::Empty);
        // Backspace at the very start changes nothing and records nothing.
        content.set_cursor(Cursor {
            position: Position { line: 0, column: 0 },
            selection: None,
        });
        assert!(content.perform_edit(Edit::Backspace).is_none());
    }

    /// Next to a repeated character the before and after texts alone cannot
    /// say which copy an edit touched; the caret does, and a run that stays
    /// at the caret stays one undo step.
    #[test]
    fn native_runs_beside_a_repeated_character_are_one_undo_step() {
        use history::{EditKind, History, Replay};
        for (text, column, edits, kind, edited) in [
            (
                "helo\n",
                2,
                vec![Edit::Insert('x'), Edit::Insert('l')],
                EditKind::Insert,
                "hexllo\n",
            ),
            (
                "abbc\n",
                1,
                vec![Edit::Delete, Edit::Delete],
                EditKind::DeleteForward,
                "ac\n",
            ),
            (
                "abbc\n",
                2,
                vec![Edit::Backspace, Edit::Backspace],
                EditKind::DeleteBackward,
                "bc\n",
            ),
        ] {
            let mut content: Content = Content::with_text(text);
            let mut history = History::default();
            let caret = Cursor {
                position: Position { line: 0, column },
                selection: None,
            };
            content.set_cursor(caret);
            let now = Instant::now();
            for edit in edits {
                let edit = content.perform_edit(edit).unwrap();
                history.record(edit.change, kind, edit.before, content.cursor(), now);
            }
            assert_eq!(content.text(), edited);
            assert_eq!(history.undo(&mut content), Replay::Applied);
            assert_eq!(content.text(), text);
            assert_eq!(content.cursor(), caret);
            assert_eq!(history.undo(&mut content), Replay::Empty, "{edited:?}");
        }
    }

    #[test]
    fn undo_reselects_the_whole_word_or_line_a_keystroke_replaced() {
        use history::{EditKind, History};
        for (select, expected) in [
            (Action::SelectWord, "xylophone"),
            (Action::SelectLine, "play the xylophone"),
        ] {
            let mut content: Content = Content::with_text("play the xylophone\nnext\n");
            let mut history = History::default();
            content.set_cursor(Cursor {
                position: Position {
                    line: 0,
                    column: 12,
                },
                selection: None,
            });
            content.perform(select);
            let edit = content.perform_edit(Edit::Insert('x')).unwrap();
            history.record(
                edit.change,
                EditKind::Insert,
                edit.before,
                content.cursor(),
                Instant::now(),
            );
            history.undo(&mut content);
            assert_eq!(content.text(), "play the xylophone\nnext\n");
            assert_eq!(content.selection().as_deref(), Some(expected));
        }
    }

    #[test]
    fn an_empty_selection_is_never_restored() {
        let mut content: Content = Content::with_text("abc\n");
        let at = Position { line: 0, column: 1 };
        content.set_cursor(Cursor {
            position: at,
            selection: Some(at),
        });
        assert_eq!(content.cursor().selection, None);
        content.perform(Action::Edit(Edit::Backspace));
        assert_eq!(
            content.text(),
            "bc\n",
            "Backspace must delete, not clear a selection"
        );
    }

    #[test]
    fn a_replacement_refuses_text_the_buffer_does_not_hold() {
        use history::EditBuffer;
        let mut content: Content = Content::with_text("one\r\ntwo\n");
        let at = Position { line: 0, column: 2 };
        assert_eq!(
            content.range_end(at, "e\r\nt"),
            Some(Position { line: 1, column: 1 })
        );
        assert_eq!(content.range_end(at, "e\nt"), None);
        assert_eq!(
            content.range_end(at, "e\r\ntwo\n"),
            Some(Position { line: 2, column: 0 })
        );
        assert!(!content.replace(at, "x", "y", &[]));
        assert_eq!(content.text(), "one\r\ntwo\n");
        assert!(content.replace(at, "e\r\ntw", "E", &[]));
        assert_eq!(content.text(), "onEo\n");
    }

    #[test]
    fn history_chords_act_only_in_the_focused_editor() {
        let press = |character: &str, modifiers, focused: bool| KeyPress {
            key: keyboard::Key::Character(character.into()),
            modified_key: keyboard::Key::Character(character.into()),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            modifiers,
            text: None,
            status: if focused {
                Status::Focused { is_hovered: false }
            } else {
                Status::Active
            },
        };
        let ctrl = keyboard::Modifiers::CTRL;
        let ctrl_shift = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
        assert_eq!(
            history_command(&press("z", ctrl, true)),
            Some(HistoryCommand::Undo)
        );
        assert_eq!(
            history_command(&press("Z", ctrl_shift, true)),
            Some(HistoryCommand::Redo)
        );
        assert_eq!(
            history_command(&press("y", ctrl, true)),
            Some(HistoryCommand::Redo)
        );
        assert_eq!(history_command(&press("z", ctrl, false)), None);
        assert_eq!(
            history_command(&press("z", keyboard::Modifiers::empty(), true)),
            None
        );
        assert_eq!(
            history_command(&press("z", ctrl | keyboard::Modifiers::ALT, true)),
            None
        );
        let backspace = KeyPress {
            key: keyboard::Key::Named(keyboard::key::Named::Backspace),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Backspace),
            ..press("", ctrl, false)
        };
        assert!(
            default_bindings::<()>(backspace).is_none(),
            "an unfocused editor must not delete a word typed at another widget"
        );
    }
}
