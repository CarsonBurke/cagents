//! A provider's patch as a reviewer reads it.
//!
//! The rows are the lines the patch carries, tinted by what happened to them,
//! numbered where the patch says where in the file they are, with the words
//! that changed picked out and their syntax coloured. A long unchanged run
//! folds to one row, and a long patch shows its first rows until the reader
//! asks for the rest.
//!
//! [`PatchModel`] is built once per patch, where the caller prepares its
//! other text away from the update thread. Drawing it only walks its rows;
//! which folds are open and whether the whole patch is shown belong to the
//! caller, in a [`ViewState`].

use super::*;
use crate::diff_view::{self, DiffPalette, DiffRowKind, DiffSyntax};
use std::ops::Range;

/// The rows a patch shows before the reader asks for more. A folded run is
/// one row.
pub(super) const SHOWN_ROWS: usize = 14;
/// The lines a patch shows for each time the reader asks for more. Every
/// drawn line is a widget the window lays out and routes each event
/// through, so a patch of thousands of lines grows by this much at a time
/// rather than all at once.
pub(super) const MORE_LINES: usize = 200;
/// Below this width a patch keeps one line-number gutter and wraps its lines
/// under their own text instead of scrolling sideways.
pub(super) const NARROW_WIDTH: f32 = 480.0;
const CODE_SIZE: f32 = design::TEXT_BODY;
/// The editor's 1.5 line spacing at [`CODE_SIZE`], on whole pixels.
const LINE_HEIGHT: f32 = 20.0;
const MARKER_WIDTH: f32 = 16.0;
const CODE_PADDING: f32 = space::XS;
/// Fold and gap rows carry a control or a caption, not a line of code.
const SEPARATOR_HEIGHT: f32 = 24.0;

/// One line the patch carries.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Line {
    pub(super) kind: DiffRowKind,
    pub(super) old_line: Option<u32>,
    pub(super) new_line: Option<u32>,
    /// The line's text in the patch: after its marker, without its ending.
    pub(super) text: Range<usize>,
    /// The words that changed, as byte ranges of the line's text.
    pub(super) changed: Vec<Range<usize>>,
    /// The line's position in the side it is highlighted on, from 1: the
    /// old side for a deletion, the new side for the rest.
    side_line: u32,
    /// Its widest drawn extent over both palettes, at
    /// [`diff_view::DIFF_FONT_SIZE`].
    width: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Row {
    Line(Line),
    /// Unchanged lines between two hunks that the patch does not carry, so
    /// they can be counted but not shown. The count is absent where the
    /// hunks are not numbered as file positions.
    Gap {
        hidden: Option<u32>,
    },
}

/// Where a patch changes its file, for a heading such as "3 changes, lines
/// 3–17".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Location {
    /// Runs of changed lines.
    pub(super) changes: usize,
    pub(super) first: u32,
    pub(super) last: u32,
}

#[derive(Debug, Clone, PartialEq)]
struct Rows {
    rows: Vec<Row>,
    /// Row ranges a fold hides, in order and apart.
    folds: Vec<Range<usize>>,
    lines: usize,
    additions: u32,
    deletions: u32,
    /// Whether its last hunk has every line it declares.
    complete: bool,
    location: Option<Location>,
    /// The widest line number either gutter shows.
    widest_number: u32,
    syntax: DiffSyntax,
}

/// One patch, parsed for drawing. Immutable once built.
#[derive(Debug, Clone, PartialEq)]
pub struct PatchModel {
    text: Arc<str>,
    origin: chat::LineOrigin,
    theme: Option<crate::settings::EditorSyntaxTheme>,
    /// `None` when the text is not a patch this can read: no hunk, or an
    /// excerpt of one that starts partway through. It is shown as the text
    /// it is.
    rows: Option<Rows>,
}

/// A patch's file metadata, which sits before its first hunk.
fn is_header(line: &str) -> bool {
    [
        "diff --git ",
        "index ",
        "--- ",
        "+++ ",
        "old mode ",
        "new mode ",
        "new file mode ",
        "deleted file mode ",
        "similarity index ",
        "dissimilarity index ",
        "rename from ",
        "rename to ",
        "copy from ",
        "copy to ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

/// A hunk's `@@ -start,count +start,count @@` ranges. A range without a
/// count is one line long.
fn hunk_header(line: &str) -> Option<[(u32, u32); 2]> {
    let ranges = line.strip_prefix("@@ -")?;
    let (old, rest) = ranges.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    let range = |text: &str| -> Option<(u32, u32)> {
        let (start, count) = text.split_once(',').unwrap_or((text, "1"));
        let range: (u32, u32) = (start.parse().ok()?, count.parse().ok()?);
        // Every number the range gives its lines, and the one after, fits.
        range.0.checked_add(range.1)?.checked_add(1)?;
        Some(range)
    };
    Some([range(old)?, range(new)?])
}

/// The first line a hunk's range covers and the line after its last. A range
/// of no lines is numbered by the line before it, as `diff` has it.
fn covered((start, count): (u32, u32)) -> Range<u32> {
    let first = if count == 0 {
        start.saturating_add(1)
    } else {
        start
    };
    first..first.saturating_add(count)
}

struct Parser<'a> {
    text: &'a str,
    origin: chat::LineOrigin,
    rows: Vec<Row>,
    folds: Vec<Range<usize>>,
    /// The old and new sides as far as the patch shows them, to highlight.
    sides: [String; 2],
    side_lines: [u32; 2],
    additions: u32,
    deletions: u32,
    changes: usize,
    changed_lines: Option<Range<u32>>,
    pairing_budget: usize,
    intraline_budget: usize,
    uncancelled: crate::repository::CancellationToken,
}

impl<'a> Parser<'a> {
    /// Folds the hunk's long unchanged runs and marks the words its changes
    /// replaced. `start` is its first row.
    fn close(&mut self, start: usize) {
        let end = self.rows.len();
        let mut run = start;
        while run < end {
            let context =
                |row: &Row| matches!(row, Row::Line(line) if line.kind == DiffRowKind::Context);
            let length = self.rows[run..end]
                .iter()
                .take_while(|row| context(row))
                .count();
            if length > 0 {
                // A run at either end of the hunk has no change on that side
                // to keep context for.
                let keep = |edge: bool| if edge { 0 } else { diff_view::FOLD_CONTEXT };
                let first = run + keep(run == start);
                let last = (run + length).saturating_sub(keep(run + length == end));
                if last.saturating_sub(first) >= diff_view::FOLD_MIN_HIDDEN {
                    self.folds.push(first..last);
                }
                run += length;
                continue;
            }
            let changed = self.rows[run..end]
                .iter()
                .take_while(|row| !context(row))
                .count();
            self.emphasise(run..run + changed);
            self.changes += 1;
            run += changed;
        }
    }

    /// Pairs a change's removed lines with the added lines that replaced
    /// them, as the repository diff does, and marks the words that differ.
    fn emphasise(&mut self, block: Range<usize>) {
        let source = self.text;
        // Each side's rows, with their text.
        let (mut removed, mut added) = (Vec::new(), Vec::new());
        for index in block {
            if let Row::Line(line) = &self.rows[index] {
                let side = match line.kind {
                    DiffRowKind::Deletion => &mut removed,
                    _ => &mut added,
                };
                side.push((index, &source[line.text.clone()]));
            }
        }
        let display = |side: &[(usize, &'a str)]| -> Vec<_> {
            side.iter()
                .map(|(_, text)| diff_view::display_line(text))
                .collect()
        };
        let matches = diff_view::replacement_matches(
            &display(&removed),
            &display(&added),
            &mut self.pairing_budget,
            &self.uncancelled,
        )
        .unwrap_or_default();
        let mut matches = matches.into_iter().peekable();
        let (mut old, mut new) = (0, 0);
        let mut changes = Vec::with_capacity(removed.len() + added.len());
        while old < removed.len() || new < added.len() {
            let (next_old, next_new) = matches
                .peek()
                .copied()
                .unwrap_or((removed.len(), added.len()));
            if old == next_old && new == next_new {
                matches.next();
                let (before, after) = diff_view::intraline_changes(
                    removed[old].1,
                    added[new].1,
                    &mut self.intraline_budget,
                );
                changes.push((removed[old].0, before));
                changes.push((added[new].0, after));
                old += 1;
                new += 1;
            } else if old < next_old {
                // Adjacency is not evidence that a line replaced another.
                let (index, text) = removed[old];
                changes.push((index, diff_view::whole_line_change(text)));
                old += 1;
            } else {
                let (index, text) = added[new];
                changes.push((index, diff_view::whole_line_change(text)));
                new += 1;
            }
        }
        for (index, changed) in changes {
            if let Row::Line(line) = &mut self.rows[index] {
                line.changed = changed;
            }
        }
    }

    fn push(&mut self, kind: DiffRowKind, text: Range<usize>, numbers: &mut [u32; 2]) {
        let source = self.text;
        let line = &source[text.clone()];
        let side = usize::from(kind != DiffRowKind::Deletion);
        let mut push_side = |side: usize| {
            self.sides[side].push_str(line);
            self.sides[side].push('\n');
            self.side_lines[side] += 1;
        };
        let (old_line, new_line) = match kind {
            DiffRowKind::Deletion => {
                push_side(0);
                self.deletions += 1;
                (Some(numbers[0]), None)
            }
            DiffRowKind::Addition => {
                push_side(1);
                self.additions += 1;
                (None, Some(numbers[1]))
            }
            _ => {
                push_side(0);
                push_side(1);
                (Some(numbers[0]), Some(numbers[1]))
            }
        };
        if kind != DiffRowKind::Context {
            let at = new_line.or(old_line).unwrap_or_default();
            self.changed_lines = Some(match self.changed_lines.take() {
                Some(lines) => lines.start.min(at)..lines.end.max(at + 1),
                None => at..at + 1,
            });
        }
        if old_line.is_some() {
            numbers[0] += 1;
        }
        if new_line.is_some() {
            numbers[1] += 1;
        }
        self.rows.push(Row::Line(Line {
            kind,
            old_line,
            new_line,
            text,
            changed: Vec::new(),
            side_line: self.side_lines[side],
            width: 0.0,
        }));
    }

    /// The patch's rows and the two sides it shows, or `None` where it is
    /// not a patch this can read.
    fn parse(mut self) -> Option<(Rows, [String; 2])> {
        let text = self.text;
        // What the open hunk has still to show of each side; `None` before
        // the first hunk.
        let mut remaining: Option<[u32; 2]> = None;
        let mut numbers = [0u32; 2];
        let mut hunk_start = 0;
        let mut previous: Option<Range<u32>> = None;
        let mut offset = 0;
        for raw in text.split_inclusive('\n') {
            let start = offset;
            offset += raw.len();
            let line = raw.strip_suffix('\n').unwrap_or(raw);
            let line = line.strip_suffix('\r').unwrap_or(line);
            if remaining.is_none_or(|remaining| remaining == [0, 0]) {
                if let Some([old, new]) = hunk_header(line) {
                    if remaining.is_some() {
                        self.close(hunk_start);
                    }
                    let covers = covered(old);
                    if let Some(previous) = previous {
                        let hidden = (self.origin == chat::LineOrigin::File
                            && covers.start >= previous.end)
                            .then(|| covers.start - previous.end);
                        if hidden != Some(0) {
                            self.rows.push(Row::Gap { hidden });
                        }
                    }
                    previous = Some(covers.clone());
                    numbers = [covers.start, covered(new).start];
                    remaining = Some([old.1, new.1]);
                    hunk_start = self.rows.len();
                    continue;
                }
                // A file's header, the next file's, and what else a tool says
                // between hunks, such as that the line before has no newline,
                // are not lines of the file. A changed or unchanged line
                // outside a hunk is a patch read from partway through, or one
                // whose ranges do not hold its lines.
                if is_header(line) || !matches!(line.as_bytes().first(), Some(b' ' | b'+' | b'-')) {
                    continue;
                }
                return None;
            }
            let counts = remaining.as_mut().expect("inside a hunk");
            // An empty line is an unchanged empty line whose space a tool
            // trimmed.
            let (kind, body) = match line.as_bytes().first() {
                None => (DiffRowKind::Context, 0),
                Some(b' ') => (DiffRowKind::Context, 1),
                Some(b'-') => (DiffRowKind::Deletion, 1),
                Some(b'+') => (DiffRowKind::Addition, 1),
                Some(b'\\') => continue,
                Some(_) => return None,
            };
            let [old, new] = counts;
            if kind != DiffRowKind::Addition {
                *old = old.checked_sub(1)?;
            }
            if kind != DiffRowKind::Deletion {
                *new = new.checked_sub(1)?;
            }
            self.push(kind, start + body..start + line.len(), &mut numbers);
        }
        // A patch cut short ends partway through its last hunk; what it has
        // is still numbered correctly, but its totals are not the edit's.
        let complete = remaining? == [0, 0];
        self.close(hunk_start);
        let lines = self
            .rows
            .iter()
            .filter(|row| matches!(row, Row::Line(_)))
            .count();
        let widest_number = self
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Line(line) => line.old_line.max(line.new_line),
                Row::Gap { .. } => None,
            })
            .max()
            .unwrap_or(0);
        let rows = Rows {
            rows: self.rows,
            folds: self.folds,
            lines,
            additions: self.additions,
            deletions: self.deletions,
            complete,
            location: (self.origin == chat::LineOrigin::File)
                .then(|| {
                    self.changed_lines.map(|lines| Location {
                        changes: self.changes,
                        first: lines.start,
                        last: lines.end - 1,
                    })
                })
                .flatten(),
            widest_number,
            syntax: DiffSyntax::default(),
        };
        Some((rows, self.sides))
    }
}

impl PatchModel {
    /// Reads `text` as a unified diff of `path`.
    ///
    /// `theme` colours its syntax, read from `path`'s name as the repository
    /// diff reads it; without one the lines are drawn in the text colour, as
    /// a caller chooses for a patch too long to highlight.
    pub(super) fn new(
        text: Arc<str>,
        path: &Path,
        origin: chat::LineOrigin,
        theme: Option<crate::settings::EditorSyntaxTheme>,
    ) -> Self {
        let rows = Self::rows_of(&text, path, origin, theme);
        Self {
            text,
            origin,
            theme,
            rows,
        }
    }

    fn rows_of(
        text: &str,
        path: &Path,
        origin: chat::LineOrigin,
        theme: Option<crate::settings::EditorSyntaxTheme>,
    ) -> Option<Rows> {
        let uncancelled = crate::repository::CancellationToken::new();
        let parser = Parser {
            text,
            origin,
            rows: Vec::new(),
            folds: Vec::new(),
            sides: Default::default(),
            side_lines: [0; 2],
            additions: 0,
            deletions: 0,
            changes: 0,
            changed_lines: None,
            // The repository diff's budgets for one comparison.
            pairing_budget: 1_000_000,
            intraline_budget: 4_000_000,
            uncancelled: uncancelled.clone(),
        };
        let (mut rows, sides) = parser.parse()?;
        if let Some(theme) = theme {
            rows.syntax =
                diff_view::highlight_comparison(&sides[0], &sides[1], path, theme, &uncancelled)
                    .unwrap_or_else(|_| unreachable!("a fresh token is never cancelled"));
        }
        let mut measurer = diff_view::LineMeasurer::default();
        let palettes = if theme.is_some() { 2 } else { 1 };
        for row in &mut rows.rows {
            let Row::Line(line) = row else { continue };
            let old = line.kind == DiffRowKind::Deletion;
            line.width = (0..palettes)
                .map(|palette| {
                    let syntax = rows.syntax.line(old, Some(line.side_line), palette == 1);
                    measurer.width(&text[line.text.clone()], syntax, iced::Font::MONOSPACE)
                })
                .fold(0.0, f32::max);
        }
        Some(rows)
    }

    #[cfg(test)]
    pub(super) fn text(&self) -> &Arc<str> {
        &self.text
    }

    /// Whether the text is shown as it is, not as rows.
    pub(super) fn is_raw(&self) -> bool {
        self.rows.is_none()
    }

    /// The lines the patch adds and removes, if it has all of them.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the transcript's edit step (#3452) shows them")
    )]
    pub(super) fn counts(&self) -> Option<(u32, u32)> {
        self.rows
            .as_ref()
            .filter(|rows| rows.complete)
            .map(|rows| (rows.additions, rows.deletions))
    }

    /// Where it changes the file. Only a patch numbered by file position
    /// can say.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the transcript's edit step (#3452) shows them")
    )]
    pub(super) fn location(&self) -> Option<Location> {
        self.rows.as_ref().and_then(|rows| rows.location)
    }

    #[cfg(test)]
    pub(super) fn rows(&self) -> &[Row] {
        self.rows.as_ref().map_or(&[], |rows| &rows.rows)
    }
}

/// Which of a patch's folds are open and how far past its first rows it is
/// shown. A view keeps one per patch it shows, and starts it again for a
/// different patch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ViewState {
    unfolded: Vec<usize>,
    /// How many times the reader asked for more: each shows up to
    /// [`MORE_LINES`] lines in all, and the first opens every fold.
    more: usize,
}

/// What a reader asked of a drawn patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Show the unchanged lines a fold hides, by the fold's index.
    Unfold(usize),
    /// Show more of the patch: past its first [`SHOWN_ROWS`] rows, up to
    /// another [`MORE_LINES`] lines, with every fold open.
    ShowMore,
}

impl ViewState {
    pub(super) fn apply(&mut self, event: Event) {
        match event {
            Event::Unfold(fold) => {
                if !self.unfolded.contains(&fold) {
                    self.unfolded.push(fold);
                }
            }
            Event::ShowMore => self.more += 1,
        }
    }

    fn folded(&self, fold: usize) -> bool {
        self.more == 0 && !self.unfolded.contains(&fold)
    }
}

/// What the drawn patch fits into.
#[derive(Debug, Clone, Copy)]
pub(super) struct Options {
    /// The width the patch is drawn in, which decides whether it keeps two
    /// gutters and scrolls sideways or keeps one and wraps.
    pub(super) width: f32,
    /// The syntax theme the view is drawn in. A model coloured in another
    /// is drawn in the text colour until it is built again.
    pub(super) theme: crate::settings::EditorSyntaxTheme,
}

/// One line of code to draw, for the caller's code-text builder.
pub(super) struct Code<'a, Link> {
    /// Its row's index in the model's rows.
    #[expect(
        dead_code,
        reason = "the transcript's selectable code text (#3452) maps selection by row"
    )]
    pub(super) row: usize,
    #[expect(
        dead_code,
        reason = "the transcript's selectable code text (#3452) maps selection by row"
    )]
    pub(super) line: &'a Line,
    /// The line, coloured and emphasised as it is drawn, in the monospace
    /// family.
    pub(super) spans: Vec<Span<'a, Link>>,
    /// Whether the line wraps under its own text: the view is too narrow to
    /// scroll it sideways.
    pub(super) wrap: bool,
}

pub(super) type Span<'a, Link> = iced::widget::text::Span<'a, Link>;

/// The code text for a view that only shows the patch.
pub(super) fn plain<'a, M: 'a>(code: Code<'a, ()>) -> Element<'a, M> {
    code_text(code.spans, code.wrap)
}

/// `spans` drawn at the patch's size and spacing.
pub(super) fn code_text<'a, Link: Clone + 'static, M: 'a>(
    spans: Vec<Span<'a, Link>>,
    wrap: bool,
) -> Element<'a, M> {
    iced::widget::rich_text(spans)
        .size(CODE_SIZE)
        .font(iced::Font::MONOSPACE)
        .line_height(iced::widget::text::LineHeight::Absolute(iced::Pixels(
            LINE_HEIGHT,
        )))
        .wrapping(if wrap {
            iced::widget::text::Wrapping::WordOrGlyph
        } else {
            iced::widget::text::Wrapping::None
        })
        .into()
}

/// A row as the view shows it: a line, or a fold or gap in its place.
enum Shown {
    Line(usize),
    Fold { fold: usize, hidden: usize },
    Gap(Option<u32>),
}

/// Which gutters a view keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gutters {
    /// Excerpt numbers say nothing about where the lines are.
    None,
    /// The new line's number, or the old one for a removed line.
    One,
    Both,
}

impl Gutters {
    fn of(origin: chat::LineOrigin, narrow: bool) -> Self {
        match (origin, narrow) {
            (chat::LineOrigin::Excerpt, _) => Self::None,
            (chat::LineOrigin::File, true) => Self::One,
            (chat::LineOrigin::File, false) => Self::Both,
        }
    }
}

/// The label of the control that shows more of a patch whose first `shown`
/// of `total` lines are drawn, after the reader asked for more `more` times.
fn more_label(shown: usize, total: usize, more: usize) -> String {
    let next = ((more + 1) * MORE_LINES).saturating_sub(shown);
    let left = total.saturating_sub(shown);
    if next >= left {
        format!("Show all {total} lines")
    } else {
        format!("Show {next} more lines")
    }
}

fn tinted<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    background: iced::Color,
) -> widget::Container<'a, M, cosmic::Theme> {
    widget::container(content).class(cosmic::theme::Container::custom(move |_| {
        iced::widget::container::Style {
            background: Some(iced::Background::Color(background)),
            ..Default::default()
        }
    }))
}

impl PatchModel {
    /// The rows the view shows, folded and capped as `state` has them, how
    /// many of the patch's lines they show, and whether the cap left rows
    /// out.
    fn shown(&self, rows: &Rows, state: &ViewState) -> (Vec<Shown>, usize, bool) {
        let mut shown = Vec::new();
        let mut lines = 0;
        let mut folds = rows.folds.iter().enumerate().peekable();
        let mut index = 0;
        while index < rows.rows.len() {
            let full = if state.more == 0 {
                shown.len() == SHOWN_ROWS
            } else {
                lines == state.more * MORE_LINES && matches!(rows.rows[index], Row::Line(_))
            };
            if full {
                return (shown, lines, true);
            }
            while folds.next_if(|(_, fold)| fold.end <= index).is_some() {}
            if let Some(&(fold, range)) = folds.peek()
                && range.start == index
                && state.folded(fold)
            {
                shown.push(Shown::Fold {
                    fold,
                    hidden: range.len(),
                });
                index = range.end;
                continue;
            }
            shown.push(match &rows.rows[index] {
                Row::Line(_) => {
                    lines += 1;
                    Shown::Line(index)
                }
                Row::Gap { hidden } => Shown::Gap(*hidden),
            });
            index += 1;
        }
        (shown, lines, false)
    }

    /// The patch drawn as rows, or `None` for text that is to be shown as
    /// it is.
    ///
    /// `code` builds each line's text from its spans, so a caller can make
    /// it selectable or searchable; [`plain`] only draws it.
    pub(super) fn view<'a, M: Clone + 'static, Link: Clone + 'static>(
        &'a self,
        state: &ViewState,
        options: Options,
        on_event: impl Fn(Event) -> M,
        code: impl Fn(Code<'a, Link>) -> Element<'a, M>,
    ) -> Option<Element<'a, M>> {
        let rows = self.rows.as_ref()?;
        let theme = cosmic::theme::active();
        let palette = DiffPalette::new(&theme);
        let syntax = self.theme == Some(options.theme);
        let narrow = options.width < NARROW_WIDTH;
        let gutters = Gutters::of(self.origin, narrow);
        let digits = rows.widest_number.max(1).ilog10() as f32 + 1.0;
        let gutter = digits.max(2.0) * design::TEXT_CAPTION * 2.0 / 3.0 + 2.0 * space::XS;
        let gutter_width = match gutters {
            Gutters::None => 0.0,
            Gutters::One => gutter,
            Gutters::Both => 2.0 * gutter,
        };
        let (shown, lines, capped) = self.shown(rows, state);
        let number = |line: Option<u32>| -> Element<'a, M> {
            widget::container(
                widget::text(line.map_or_else(String::new, |line| line.to_string()))
                    .size(design::TEXT_CAPTION)
                    .font(iced::Font::MONOSPACE)
                    .line_height(iced::widget::text::LineHeight::Absolute(iced::Pixels(
                        LINE_HEIGHT,
                    )))
                    .class(cosmic::theme::Text::Custom(diff_view::diff_gutter_text))
                    .align_x(Alignment::End)
                    .width(Length::Fill),
            )
            .padding([0.0, space::XS])
            .width(gutter)
            .into()
        };
        let numbers = |line: &Line| -> Vec<Element<'a, M>> {
            match gutters {
                Gutters::None => Vec::new(),
                Gutters::One => vec![number(line.new_line.or(line.old_line))],
                Gutters::Both => vec![number(line.old_line), number(line.new_line)],
            }
        };
        let marker = |kind: DiffRowKind| -> Element<'a, M> {
            widget::text(match kind {
                DiffRowKind::Addition => "+",
                DiffRowKind::Deletion => "−",
                _ => "",
            })
            .size(CODE_SIZE)
            .font(iced::Font::MONOSPACE)
            .line_height(iced::widget::text::LineHeight::Absolute(iced::Pixels(
                LINE_HEIGHT,
            )))
            .class(cosmic::theme::Text::Custom(diff_view::diff_gutter_text))
            .align_x(Alignment::Center)
            .width(MARKER_WIDTH)
            .into()
        };
        let line_code = |index: usize, line: &'a Line| {
            let spans = diff_view::styled_line_spans(
                &self.text[line.text.clone()],
                &line.changed,
                if syntax {
                    rows.syntax.line(
                        line.kind == DiffRowKind::Deletion,
                        Some(line.side_line),
                        palette.dark,
                    )
                } else {
                    &[]
                },
                line.kind,
                &palette,
                Some(iced::font::Family::Monospace),
            );
            code(Code {
                row: index,
                line,
                spans,
                wrap: narrow,
            })
        };
        let line_of = |index: usize| match &rows.rows[index] {
            Row::Line(line) => line,
            Row::Gap { .. } => unreachable!("a shown line is a line"),
        };
        let fold_label = |hidden: usize| {
            format!(
                "{hidden} unchanged {}",
                if hidden == 1 { "line" } else { "lines" }
            )
        };
        let gap_label = |hidden: Option<u32>| match hidden {
            Some(1) => "1 line not in this patch".to_owned(),
            Some(hidden) => format!("{hidden} lines not in this patch"),
            None => "Lines not in this patch".to_owned(),
        };
        let separator_background = diff_view::diff_fold_background(&theme);
        let fold_button = |fold: usize, hidden: usize| -> Element<'a, M> {
            let label = fold_label(hidden);
            widget::button::custom(
                widget::row::with_children(vec![
                    crate::ui_controls::passive_icon(
                        crate::ui_controls::Icon::ExpandAll,
                        design::COMPACT_ICON_SIZE,
                    ),
                    design::caption(label.clone()).into(),
                ])
                .spacing(space::XS)
                .align_y(Alignment::Center),
            )
            .name(format!("Show {label}"))
            .class(crate::ui_controls::button::quiet_class())
            .padding([0.0, space::XS])
            .height(SEPARATOR_HEIGHT)
            .on_press(on_event(Event::Unfold(fold)))
            .into()
        };
        let gap = |hidden: Option<u32>| -> Element<'a, M> {
            widget::container(design::caption(gap_label(hidden)))
                .padding([0.0, space::XS])
                .height(SEPARATOR_HEIGHT)
                .align_y(Alignment::Center)
                .into()
        };
        let body: Element<'a, M> = if narrow {
            let rows: Vec<Element<'a, M>> = shown
                .iter()
                .map(|shown| match *shown {
                    Shown::Line(index) => {
                        let line = line_of(index);
                        let mut cells = numbers(line);
                        cells.push(marker(line.kind));
                        cells.push(
                            widget::container(line_code(index, line))
                                .padding([0.0, CODE_PADDING])
                                .width(Length::Fill)
                                .into(),
                        );
                        tinted(
                            widget::row::with_children(cells).align_y(Alignment::Start),
                            palette.row_background(line.kind),
                        )
                        .width(Length::Fill)
                        .into()
                    }
                    Shown::Fold { fold, hidden } => {
                        tinted(fold_button(fold, hidden), separator_background)
                            .padding(iced::Padding::ZERO.left(gutter_width))
                            .width(Length::Fill)
                            .into()
                    }
                    Shown::Gap(hidden) => tinted(gap(hidden), separator_background)
                        .padding(iced::Padding::ZERO.left(gutter_width))
                        .width(Length::Fill)
                        .into(),
                })
                .collect();
            widget::column::with_children(rows)
                .width(Length::Fill)
                .into()
        } else {
            let scale = CODE_SIZE / diff_view::DIFF_FONT_SIZE;
            let widest = shown
                .iter()
                .filter_map(|shown| match *shown {
                    Shown::Line(index) => Some(line_of(index).width),
                    _ => None,
                })
                .fold(0.0, f32::max);
            let available = (options.width - gutter_width).max(0.0);
            let needed = (widest * scale + MARKER_WIDTH + 2.0 * CODE_PADDING).ceil();
            let code_width = needed.max(available);
            // The thumb of a line too long for the view gets its own rail
            // rather than covering the last row.
            let overflows = needed > available;
            let mut gutter_cells: Vec<Element<'a, M>> = Vec::new();
            let mut code_cells: Vec<Element<'a, M>> = Vec::new();
            for shown in &shown {
                match *shown {
                    Shown::Line(index) => {
                        let line = line_of(index);
                        let background = palette.row_background(line.kind);
                        if gutters != Gutters::None {
                            gutter_cells.push(
                                tinted(widget::row::with_children(numbers(line)), background)
                                    .height(LINE_HEIGHT)
                                    .into(),
                            );
                        }
                        code_cells.push(
                            tinted(
                                widget::row::with_children(vec![
                                    marker(line.kind),
                                    widget::container(line_code(index, line))
                                        .padding([0.0, CODE_PADDING])
                                        .into(),
                                ]),
                                background,
                            )
                            .width(code_width)
                            .height(LINE_HEIGHT)
                            .into(),
                        );
                    }
                    Shown::Fold { fold, hidden } => {
                        if gutters != Gutters::None {
                            // The fold's icon sits over the numbers it hides;
                            // the labelled control beside it is the one focus
                            // reaches.
                            gutter_cells.push(
                                widget::mouse_area(
                                    tinted(
                                        crate::ui_controls::passive_icon(
                                            crate::ui_controls::Icon::ExpandAll,
                                            design::COMPACT_ICON_SIZE,
                                        ),
                                        separator_background,
                                    )
                                    .center_x(gutter_width)
                                    .center_y(SEPARATOR_HEIGHT),
                                )
                                .on_press(on_event(Event::Unfold(fold)))
                                .into(),
                            );
                        }
                        code_cells.push(
                            tinted(
                                widget::button::custom(design::caption(fold_label(hidden)))
                                    .name(format!("Show {}", fold_label(hidden)))
                                    .class(crate::ui_controls::button::quiet_class())
                                    .padding([0.0, space::XS])
                                    .height(SEPARATOR_HEIGHT)
                                    .on_press(on_event(Event::Unfold(fold))),
                                separator_background,
                            )
                            .width(code_width)
                            .into(),
                        );
                    }
                    Shown::Gap(hidden) => {
                        if gutters != Gutters::None {
                            gutter_cells.push(
                                tinted(
                                    widget::Space::new()
                                        .width(gutter_width)
                                        .height(SEPARATOR_HEIGHT),
                                    separator_background,
                                )
                                .into(),
                            );
                        }
                        code_cells.push(
                            tinted(gap(hidden), separator_background)
                                .width(code_width)
                                .into(),
                        );
                    }
                }
            }
            let code = widget::scrollable(widget::column::with_children(code_cells).padding(
                if overflows {
                    design::with_horizontal_rail(iced::Padding::ZERO)
                } else {
                    iced::Padding::ZERO
                },
            ))
            .direction(iced::widget::scrollable::Direction::Horizontal(
                design::scrollbar(),
            ))
            .width(Length::Fill);
            let mut columns: Vec<Element<'a, M>> = Vec::new();
            if gutters != Gutters::None {
                columns.push(
                    widget::column::with_children(gutter_cells)
                        .width(gutter_width)
                        .into(),
                );
            }
            columns.push(code.into());
            widget::row::with_children(columns)
                .align_y(Alignment::Start)
                .width(Length::Fill)
                .into()
        };
        let mut parts = vec![body];
        if capped {
            parts.push(
                widget::row::with_children(vec![
                    widget::button::link(more_label(lines, rows.lines, state.more))
                        .on_press(on_event(Event::ShowMore))
                        .into(),
                    widget::Space::new().width(Length::Fill).into(),
                    design::caption(format!("{lines} of {} lines shown", rows.lines)).into(),
                ])
                .align_y(Alignment::Center)
                .padding([space::XXS, space::XS])
                .into(),
            );
        }
        Some(
            widget::container(widget::column::with_children(parts))
                .width(Length::Fill)
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(text: &str, origin: chat::LineOrigin) -> PatchModel {
        PatchModel::new(text.into(), Path::new("src/lib.rs"), origin, None)
    }

    fn file(text: &str) -> PatchModel {
        model(text, chat::LineOrigin::File)
    }

    /// Each row as `kind old new text`, or a gap as `gap N`.
    fn outline(model: &PatchModel) -> Vec<String> {
        model
            .rows()
            .iter()
            .map(|row| match row {
                Row::Line(line) => format!(
                    "{} {} {} {}",
                    match line.kind {
                        DiffRowKind::Addition => "+",
                        DiffRowKind::Deletion => "-",
                        _ => " ",
                    },
                    line.old_line.map_or("_".into(), |line| line.to_string()),
                    line.new_line.map_or("_".into(), |line| line.to_string()),
                    &model.text()[line.text.clone()],
                ),
                Row::Gap { hidden } => {
                    format!(
                        "gap {}",
                        hidden.map_or("?".into(), |hidden| hidden.to_string())
                    )
                }
            })
            .collect()
    }

    fn context(lines: std::ops::RangeInclusive<u32>) -> String {
        lines.map(|line| format!(" line {line}\n")).collect()
    }

    #[test]
    fn file_headers_are_metadata_and_hunk_headers_become_numbers() {
        let patch = file(
            "diff --git a/src/lib.rs b/src/lib.rs\nindex 1234567..89abcde 100644\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -3,3 +3,3 @@ fn main() {\n keep\r\n-old\n+new\n tail\n",
        );
        assert_eq!(
            outline(&patch),
            ["  3 3 keep", "- 4 _ old", "+ _ 4 new", "  5 5 tail"]
        );
        assert_eq!(patch.counts(), Some((1, 1)));
        assert_eq!(
            patch.location(),
            Some(Location {
                changes: 1,
                first: 4,
                last: 4
            })
        );
    }

    #[test]
    fn hunks_are_separated_by_the_lines_between_them() {
        let patch = file(
            "@@ -1,2 +1,2 @@\n-a\n+b\n c\n@@ -3,1 +3,1 @@\n-d\n+e\n@@ -10,1 +10,2 @@\n f\n+g\n",
        );
        assert_eq!(
            outline(&patch),
            [
                "- 1 _ a",
                "+ _ 1 b",
                "  2 2 c",
                // Line 3 follows line 2 directly: nothing lies between.
                "- 3 _ d",
                "+ _ 3 e",
                "gap 6",
                "  10 10 f",
                "+ _ 11 g",
            ]
        );
        assert_eq!(
            patch.location(),
            Some(Location {
                changes: 3,
                first: 1,
                last: 11
            })
        );
        let excerpt = model(
            "@@ -1,1 +1,1 @@\n-a\n+b\n@@ -1,1 +1,1 @@\n-c\n+d\n",
            chat::LineOrigin::Excerpt,
        );
        assert_eq!(
            outline(&excerpt)[2],
            "gap ?",
            "an excerpt's hunks are unplaced"
        );
        assert_eq!(excerpt.location(), None);
    }

    #[test]
    fn an_empty_range_is_numbered_from_the_line_before_it() {
        let added = file("--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,2 @@\n+one\n+two\n");
        assert_eq!(outline(&added), ["+ _ 1 one", "+ _ 2 two"]);
        let deleted = file("@@ -1,2 +0,0 @@\n-one\n-two\n");
        assert_eq!(outline(&deleted), ["- 1 _ one", "- 2 _ two"]);
        assert_eq!(deleted.counts(), Some((0, 2)));
    }

    #[test]
    fn no_newline_markers_and_trimmed_blank_lines_are_not_rows() {
        let patch = file(
            "@@ -1,3 +1,3 @@\n a\n\n-b\n\\ No newline at end of file\n+c\n\\ No newline at end of file\n",
        );
        assert_eq!(outline(&patch), ["  1 1 a", "  2 2 ", "- 3 _ b", "+ _ 3 c"]);
    }

    #[test]
    fn long_unchanged_runs_fold_and_keep_context_beside_each_change() {
        let mut text = String::from("@@ -1,24 +1,24 @@\n-a\n+b\n");
        text += &context(2..=11);
        text += "-c\n+d\n";
        // Seven lines between changes keep three each side and hide one,
        // too few to be worth a row.
        text += &context(13..=19);
        text += "-e\n+f\n";
        text += &context(21..=24);
        // Rows: a and b, ten unchanged (2..12), c and d, seven (14..21), e
        // and f, and four that end the hunk (23..27), with no change after
        // them to keep context for but too few to fold.
        assert_eq!(file(&text).rows.unwrap().folds, [5..9]);
    }

    #[test]
    fn malformed_text_and_a_page_that_starts_inside_a_hunk_are_shown_as_text() {
        for text in [
            "opaque provider patch",
            "@@ gibberish\n+new\n",
            "@@ -0,0 +1,0 @@\n+unaccounted\n",
            "@@ -1 +1 @@\n-old\n+new\n+extra\n",
            "@@ -1,2 +1,2 @@\n x\nstray\n",
            // The second page of a long patch.
            " context\n+added\n@@ -9 +9 @@\n-a\n+b\n",
            // Numbers past the last a line can have.
            "@@ -4294967295,2 +1 @@\n x\n-y\n",
            "",
        ] {
            assert!(file(text).is_raw(), "{text:?}");
        }
    }

    /// The conversation's file list counts a patch's lines with the store's
    /// own reader; a patch the view reads must total the same.
    #[test]
    fn totals_agree_with_the_file_list_counts() {
        for (text, counts) in [
            (
                "@@ -1,2 +1,2 @@\n--- marker\n-old\n+++ marker\n+new\n",
                Some((2, 2)),
            ),
            ("@@ -0,0 +1,0 @@\n", Some((0, 0))),
            ("@@ -1 +1 @@\n-old\n", None),
            (
                "@@ -1 +1 @@\n-old\n+new\n\\ No newline at end of file\n--- a/b.rs\n+++ b/b.rs\n@@ -0,0 +1 @@\n+another\n",
                Some((2, 1)),
            ),
        ] {
            assert_eq!(file(text).counts(), counts, "{text:?}");
        }
        let truncated = file("@@ -1,3 +1,3 @@\n a\n-b\n");
        assert!(
            !truncated.is_raw(),
            "a cut-short page still shows its lines"
        );
        assert_eq!(outline(&truncated), ["  1 1 a", "- 2 _ b"]);
    }

    #[test]
    fn replaced_lines_mark_only_the_words_that_changed() {
        let patch =
            file("@@ -1,2 +1,2 @@\n-let total = count + 1;\n-unrelated\n+let total = count + 2;\n");
        let Row::Line(old) = &patch.rows()[0] else {
            panic!("a line")
        };
        let Row::Line(new) = &patch.rows()[2] else {
            panic!("a line")
        };
        let line = &patch.text()[old.text.clone()];
        assert_eq!(
            old.changed
                .iter()
                .map(|range| &line[range.clone()])
                .collect::<Vec<_>>(),
            ["1"]
        );
        let line = &patch.text()[new.text.clone()];
        assert_eq!(
            new.changed
                .iter()
                .map(|range| &line[range.clone()])
                .collect::<Vec<_>>(),
            ["2"]
        );
        let Row::Line(unpaired) = &patch.rows()[1] else {
            panic!("a line")
        };
        assert_eq!(unpaired.changed, [0.."unrelated".len()]);
    }

    #[test]
    fn excerpts_show_no_numbers_and_narrow_views_keep_one() {
        assert_eq!(Gutters::of(chat::LineOrigin::Excerpt, false), Gutters::None);
        assert_eq!(Gutters::of(chat::LineOrigin::Excerpt, true), Gutters::None);
        assert_eq!(Gutters::of(chat::LineOrigin::File, true), Gutters::One);
        assert_eq!(Gutters::of(chat::LineOrigin::File, false), Gutters::Both);
    }

    #[test]
    fn a_long_patch_shows_its_first_rows_until_asked_and_folds_open_in_place() {
        let mut text = String::from("@@ -1,40 +1,41 @@\n+first\n");
        text += &context(1..=40);
        let patch = file(&text);
        let rows = patch.rows.as_ref().unwrap();
        // The run ends the hunk, so only the side by the change keeps
        // context.
        assert_eq!(rows.folds, [4..41]);
        let mut state = ViewState::default();
        let (shown, lines, capped) = patch.shown(rows, &state);
        assert!(!capped, "a fold is one row, so these fit");
        assert!(matches!(
            shown[..],
            [
                Shown::Line(0),
                Shown::Line(1),
                Shown::Line(2),
                Shown::Line(3),
                Shown::Fold {
                    fold: 0,
                    hidden: 37
                }
            ]
        ));
        assert_eq!(lines, 4);
        state.apply(Event::Unfold(0));
        let (shown, lines, capped) = patch.shown(rows, &state);
        assert_eq!(shown.len(), SHOWN_ROWS);
        assert_eq!(lines, SHOWN_ROWS);
        assert!(capped);
        assert_eq!(more_label(lines, rows.lines, 0), "Show all 41 lines");
        state.apply(Event::ShowMore);
        let (_, lines, capped) = patch.shown(rows, &state);
        assert_eq!((lines, capped), (41, false));
        // Showing more opens the folds that were still closed.
        let mut state = ViewState::default();
        state.apply(Event::ShowMore);
        let (shown, lines, capped) = patch.shown(rows, &state);
        assert_eq!((shown.len(), lines, capped), (41, 41, false));
    }

    #[test]
    fn a_huge_patch_grows_a_step_at_a_time_and_says_by_how_much() {
        let total = 2 * MORE_LINES + 50;
        let mut text = format!("@@ -0,0 +1,{total} @@\n");
        for line in 1..=total {
            text += &format!("+line {line}\n");
        }
        let patch = file(&text);
        let rows = patch.rows.as_ref().unwrap();
        let mut state = ViewState::default();
        let mut steps = Vec::new();
        loop {
            let (shown, lines, capped) = patch.shown(rows, &state);
            assert_eq!(shown.len(), lines, "an added file has only lines");
            if !capped {
                steps.push((lines, String::new()));
                break;
            }
            steps.push((lines, more_label(lines, rows.lines, state.more)));
            state.apply(Event::ShowMore);
        }
        assert_eq!(
            steps,
            [
                (
                    SHOWN_ROWS,
                    format!("Show {} more lines", MORE_LINES - SHOWN_ROWS)
                ),
                (MORE_LINES, format!("Show {MORE_LINES} more lines")),
                (2 * MORE_LINES, format!("Show all {total} lines")),
                (total, String::new()),
            ]
        );
    }
}
