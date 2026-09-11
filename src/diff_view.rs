//! Immutable native diff models computed away from the application update thread.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    ops::Range,
    path::Path,
    sync::LazyLock,
};

use imara_diff::{Algorithm, Diff, InternedInput};

use crate::repository::{
    CancellationToken, DiffBasis, DiffContent, DiffRequest, RepoPath, RepositoryService,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    Inline,
    Split,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRowKind {
    Context,
    Addition,
    Deletion,
    Modification,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineDiffRow {
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
    pub kind: DiffRowKind,
    /// UTF-8 byte ranges in the displayed text, computed once on the diff worker.
    pub changed: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitDiffRow {
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub old_text: Option<String>,
    pub new_text: Option<String>,
    pub kind: DiffRowKind,
    pub old_changed: Vec<Range<usize>>,
    pub new_changed: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffModel {
    pub inline_rows: Vec<InlineDiffRow>,
    pub split_rows: Vec<SplitDiffRow>,
    pub inline_change_starts: Vec<usize>,
    pub split_change_starts: Vec<usize>,
    pub additions: u32,
    pub deletions: u32,
    /// Shaped logical-pixel extents for [light, dark], rounded up on the worker.
    pub max_line_width: [u32; 2],
    pub syntax: DiffSyntax,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxSpan {
    range: Range<usize>,
    color: Option<[u32; 4]>,
    font: Option<cosmic::iced::Font>,
}

/// Full-side lexical state, cached for both application palettes. A context
/// row can have different syntax on each side after a comment delimiter changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffSyntax {
    old: [Vec<Vec<SyntaxSpan>>; 2],
    new: [Vec<Vec<SyntaxSpan>>; 2],
}

impl DiffSyntax {
    pub(crate) fn line(&self, old: bool, line: Option<u32>, dark: bool) -> &[SyntaxSpan] {
        let side = if old { &self.old } else { &self.new };
        line.and_then(|line| line.checked_sub(1))
            .and_then(|line| side[usize::from(dark)].get(line as usize))
            .map_or(&[], Vec::as_slice)
    }
}

/// Native bundled grammar names are shared by the editable buffer and diffs.
/// CUDA uses the bundled C++ grammar (there is no bundled CUDA grammar).
pub(crate) fn highlighter_settings(path: &Path, dark: bool) -> cosmic::iced::highlighter::Settings {
    use cosmic::iced::highlighter;

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase());
    let token = match name {
        "CMakeLists.txt" => "CMake".to_owned(),
        "Cargo.lock" => "TOML".to_owned(),
        ".bashrc" | ".bash_profile" | ".bash_login" => "Bash".to_owned(),
        ".env" => "DotENV".to_owned(),
        _ if name.starts_with("Dockerfile.") => "Dockerfile".to_owned(),
        _ => match extension.as_deref() {
            Some("cu" | "cuh") => "C++".to_owned(),
            Some("mjs" | "cjs") => "JavaScript".to_owned(),
            Some("mts" | "cts") => "TypeScript".to_owned(),
            Some(extension) => extension.to_owned(),
            None => name.to_owned(),
        },
    };
    highlighter::Settings {
        theme: if dark {
            highlighter::Theme::Base16Ocean
        } else {
            highlighter::Theme::InspiredGitHub
        },
        token,
    }
}

type SyntaxLines = [Vec<Vec<SyntaxSpan>>; 2];

/// Immutable grammar/theme tables, identical to iced's highlighter. Parsing is
/// theme-independent: advance one parser and scope stack, then color each scope
/// for both palettes without reparsing or cloning the parser at every line.
fn highlight_side(
    text: &str,
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<SyntaxLines, Cancelled> {
    use two_face::re_exports::syntect::{highlighting, parsing};
    static SYNTAXES: LazyLock<parsing::SyntaxSet> =
        LazyLock::new(two_face::syntax::extra_no_newlines);
    static THEMES: LazyLock<highlighting::ThemeSet> =
        LazyLock::new(highlighting::ThemeSet::load_defaults);

    check_cancelled(cancellation)?;
    let settings = highlighter_settings(path, false);
    let syntax = SYNTAXES
        .find_syntax_by_token(&settings.token)
        .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text());
    let mut parser = parsing::ParseState::new(syntax);
    let mut stack = parsing::ScopeStack::new();
    let highlighters = [
        highlighting::Highlighter::new(&THEMES.themes["InspiredGitHub"]),
        highlighting::Highlighter::new(&THEMES.themes["base16-ocean.dark"]),
    ];
    let mut lines: SyntaxLines = Default::default();
    for line in text.split_inclusive('\n') {
        check_cancelled(cancellation)?;
        let line = line
            .strip_suffix("\r\n")
            .or_else(|| line.strip_suffix('\n'))
            .unwrap_or(line);
        let ops = parser.parse_line(line, &SYNTAXES).unwrap_or_default();
        check_cancelled(cancellation)?;
        let mut spans: [Vec<SyntaxSpan>; 2] = Default::default();
        let mut start = 0;
        // Apply even zero-length operations: they carry multiline lexical
        // state. This is iced's scope iteration, shared across both themes.
        for (end, op) in ops
            .iter()
            .map(|(end, op)| (*end, Some(op)))
            .chain(std::iter::once((line.len(), None)))
        {
            check_cancelled(cancellation)?;
            if start < end {
                for (palette, highlighter) in highlighters.iter().enumerate() {
                    let style = highlighter.style_mod_for_stack(&stack.scopes);
                    let color = style.foreground.map(|color| {
                        let color = cosmic::iced::Color::from_rgba8(
                            color.r,
                            color.g,
                            color.b,
                            color.a as f32 / 255.0,
                        );
                        [
                            color.r.to_bits(),
                            color.g.to_bits(),
                            color.b.to_bits(),
                            color.a.to_bits(),
                        ]
                    });
                    let font = style.font_style.and_then(|style| {
                        use cosmic::iced::{Font, font};
                        let bold = style.contains(highlighting::FontStyle::BOLD);
                        let italic = style.contains(highlighting::FontStyle::ITALIC);
                        (bold || italic).then_some(Font {
                            weight: if bold {
                                font::Weight::Bold
                            } else {
                                font::Weight::Normal
                            },
                            style: if italic {
                                font::Style::Italic
                            } else {
                                font::Style::Normal
                            },
                            ..Font::MONOSPACE
                        })
                    });
                    spans[palette].push(SyntaxSpan {
                        range: start..end,
                        color,
                        font,
                    });
                }
            }
            if let Some(op) = op {
                let _ = stack.apply(op);
            }
            start = end;
        }
        for (lines, spans) in lines.iter_mut().zip(spans) {
            lines.push(spans);
        }
    }
    Ok(lines)
}

#[derive(Debug)]
struct Cancelled;

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), Cancelled> {
    if cancellation.is_cancelled() {
        Err(Cancelled)
    } else {
        Ok(())
    }
}

fn build_highlighted_diff(
    before: &str,
    after: &str,
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<DiffModel, Cancelled> {
    let mut model = build_diff_rows(before, after, cancellation)?;
    model.syntax = DiffSyntax {
        old: highlight_side(before, path, cancellation)?,
        new: highlight_side(after, path, cancellation)?,
    };
    measure_diff(&mut model, cancellation)?;
    Ok(model)
}

/// Colors do not affect glyph advances; fonts and every span boundary do.
/// Keep the exact rich-text partition (including intraline boundaries), rather
/// than measuring plain text or estimating widths from character counts.
#[derive(Eq)]
struct ShapeKey<'a> {
    text: &'a str,
    changed: &'a [Range<usize>],
    syntax: &'a [SyntaxSpan],
}

impl PartialEq for ShapeKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
            && self.changed == other.changed
            && self.syntax.len() == other.syntax.len()
            && self
                .syntax
                .iter()
                .zip(other.syntax)
                .all(|(a, b)| a.range == b.range && a.font == b.font)
    }
}

impl Hash for ShapeKey<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text.hash(state);
        self.changed.hash(state);
        self.syntax.len().hash(state);
        for span in self.syntax {
            span.range.hash(state);
            span.font.hash(state);
        }
    }
}

fn measure_diff(model: &mut DiffModel, cancellation: &CancellationToken) -> Result<(), Cancelled> {
    let palettes = if model
        .syntax
        .old
        .iter()
        .chain(&model.syntax.new)
        .all(Vec::is_empty)
    {
        1
    } else {
        2
    };
    let mut widths = [0.0_f32; 2];
    // Worker-local, bounded, borrowing the existing model: no retained native
    // paragraphs or copies of source text, and no cross-repository cache.
    let mut measured = HashMap::new();
    for row in &model.split_rows {
        for (old, line, text, changed) in [
            (
                true,
                row.old_line,
                row.old_text.as_deref(),
                &row.old_changed,
            ),
            (
                false,
                row.new_line,
                row.new_text.as_deref(),
                &row.new_changed,
            ),
        ] {
            if let Some(text) = text {
                for (palette, width) in widths.iter_mut().enumerate().take(palettes) {
                    check_cancelled(cancellation)?;
                    let syntax = model.syntax.line(old, line, palette == 1);
                    let key = ShapeKey {
                        text,
                        changed,
                        syntax,
                    };
                    let shaped = if let Some(width) = measured.get(&key) {
                        *width
                    } else {
                        let width = shaped_line_width(text, changed, syntax);
                        check_cancelled(cancellation)?;
                        if measured.len() == 4096 {
                            measured.clear();
                        }
                        measured.insert(key, width);
                        width
                    };
                    *width = width.max(shaped);
                }
            }
        }
    }
    if palettes == 1 {
        widths[1] = widths[0];
    }
    model.max_line_width = widths.map(|width| width.ceil() as u32);
    Ok(())
}
impl DiffModel {
    pub fn is_empty(&self) -> bool {
        self.additions == 0 && self.deletions == 0
    }

    pub fn change_starts(&self, mode: DiffMode) -> &[usize] {
        match mode {
            DiffMode::Inline => &self.inline_change_starts,
            DiffMode::Split => &self.split_change_starts,
        }
    }

    pub fn row_count(&self, mode: DiffMode) -> usize {
        match mode {
            DiffMode::Inline => self.inline_rows.len(),
            DiffMode::Split => self.split_rows.len(),
        }
    }
}

/// What the diff surface draws for one path and one comparison basis.
///
/// Built on a worker thread: reading both sides and turning them into rows are
/// each unbounded in the size of the file, and neither belongs on the update
/// thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderedDiff {
    /// Line rows, with the labels of the two sides they were computed from.
    ///
    /// The labels are the service's own, so the chrome names the comparison
    /// being shown — `Index`/`Working tree`, `HEAD`/`Index`,
    /// `main (a1b2c3d4)`/`HEAD`, or a pair of commit ids — instead of one
    /// wording for every basis.
    Text {
        model: DiffModel,
        old_label: String,
        new_label: String,
    },
    /// This comparison has no text rows. The service's account of the sides is
    /// kept verbatim, so the surface reports what they are — absent, binary,
    /// not UTF-8, oversize, a symlink, a submodule, unreadable, or the three
    /// stages of a conflict — rather than an empty or invented diff.
    NoText(DiffContent),
    /// The comparison could not be read at all.
    Error(String),
}

impl RenderedDiff {
    /// Turns one comparison into rows, or into the fact that it has none.
    ///
    /// An added or deleted path still diffs:
    /// [`Absent`](crate::repository::DiffSide::Absent) reads as an empty side.
    /// An unmerged path never does — it has three index stages and no single
    /// old side — so it is passed through instead of being flattened into a
    /// two-sided diff.
    ///
    /// Cancellation discards the entire model, never exposing partially built
    /// rows, syntax, or widths.
    pub fn from_content(
        content: DiffContent,
        path: &Path,
        cancellation: &CancellationToken,
    ) -> Self {
        if cancellation.is_cancelled() {
            return Self::Error("repository operation cancelled".into());
        }
        match content {
            DiffContent::Comparison {
                old,
                new,
                old_label,
                new_label,
            } => {
                if !old.is_diffable() || !new.is_diffable() {
                    return Self::NoText(DiffContent::Comparison {
                        old,
                        new,
                        old_label,
                        new_label,
                    });
                }
                let model = match build_highlighted_diff(
                    old.text().unwrap_or_default(),
                    new.text().unwrap_or_default(),
                    path,
                    cancellation,
                ) {
                    Ok(model) => model,
                    Err(Cancelled) => return Self::Error("repository operation cancelled".into()),
                };
                Self::Text {
                    model,
                    old_label,
                    new_label,
                }
            }
            unmerged @ DiffContent::Unmerged { .. } => Self::NoText(unmerged),
        }
    }
}

/// Reads one path's comparison through [`RepositoryService::diff`] and renders
/// it.
///
/// `previous_path` is a rename or copy source: the old side reads it instead of
/// `relative_path`, so a renamed file compares against its own former content
/// rather than against nothing.
///
/// The caller owns cancellation and must fence completed results by request.
/// Git is interruptible; CPU work checks between rows, lexical scope operations
/// and native paragraphs. A single parser line, native shaping call, or imara
/// diff operation is indivisible and finishes before the next checkpoint.
pub fn load_repository_diff(
    worktree: &Path,
    relative_path: &Path,
    basis: &DiffBasis,
    previous_path: Option<&Path>,
    cancellation: &CancellationToken,
) -> RenderedDiff {
    // `RepoPath` *is* the path validation: it rejects an absolute path, `.`,
    // `..`, an empty component and an interior NUL, which is what makes the
    // pathspec safe to hand to Git.
    let path = match RepoPath::parse(relative_path) {
        Ok(path) => path,
        Err(error) => return RenderedDiff::Error(error.to_string()),
    };
    let old_path = match previous_path.map(RepoPath::parse).transpose() {
        Ok(old_path) => old_path,
        Err(error) => return RenderedDiff::Error(error.to_string()),
    };
    let request = DiffRequest {
        worktree: worktree.to_path_buf(),
        basis: basis.clone(),
        path,
        old_path,
    };
    match RepositoryService::system("git").diff(&request, cancellation) {
        Ok(content) => RenderedDiff::from_content(content, relative_path, cancellation),
        Err(error) => RenderedDiff::Error(error.to_string()),
    }
}

pub fn build_diff(before: &str, after: &str) -> DiffModel {
    let cancellation = CancellationToken::new();
    let mut model =
        build_diff_rows(before, after, &cancellation).expect("local token is not cancelled");
    measure_diff(&mut model, &cancellation).expect("local token is not cancelled");
    model
}

fn build_diff_rows(
    before: &str,
    after: &str,
    cancellation: &CancellationToken,
) -> Result<DiffModel, Cancelled> {
    check_cancelled(cancellation)?;
    let input = InternedInput::new(before, after);
    check_cancelled(cancellation)?;
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    check_cancelled(cancellation)?;
    diff.postprocess_lines(&input);
    check_cancelled(cancellation)?;

    let before_lines = input
        .before
        .iter()
        .map(|token| {
            check_cancelled(cancellation)?;
            Ok(display_line(input.interner[*token]))
        })
        .collect::<Result<Vec<_>, Cancelled>>()?;
    let after_lines = input
        .after
        .iter()
        .map(|token| {
            check_cancelled(cancellation)?;
            Ok(display_line(input.interner[*token]))
        })
        .collect::<Result<Vec<_>, Cancelled>>()?;
    let mut inline_rows = Vec::new();
    let mut split_rows = Vec::new();
    let mut inline_change_starts = Vec::new();
    let mut split_change_starts = Vec::new();
    let mut old_cursor = 0usize;
    let mut new_cursor = 0usize;
    let mut before_endings = HashMap::<&str, u8>::new();
    let mut after_endings = HashMap::<&str, u8>::new();
    let mut intraline_budget = 4_000_000usize;

    for hunk in diff.hunks() {
        check_cancelled(cancellation)?;
        append_context(
            &before_lines,
            &after_lines,
            &mut old_cursor,
            hunk.before.start as usize,
            &mut new_cursor,
            hunk.after.start as usize,
            &mut inline_rows,
            &mut split_rows,
            cancellation,
        )?;
        inline_change_starts.push(inline_rows.len());
        split_change_starts.push(split_rows.len());

        let old_start = hunk.before.start as usize;
        let old_end = hunk.before.end as usize;
        let new_start = hunk.after.start as usize;
        let new_end = hunk.after.end as usize;
        before_endings.clear();
        after_endings.clear();
        for line in &before_lines[old_start..old_end] {
            check_cancelled(cancellation)?;
            *before_endings.entry(&line.text).or_default() |= line.ending.bit();
        }
        for line in &after_lines[new_start..new_end] {
            check_cancelled(cancellation)?;
            *after_endings.entry(&line.text).or_default() |= line.ending.bit();
        }
        for (offset, line) in before_lines[old_start..old_end].iter().enumerate() {
            check_cancelled(cancellation)?;
            let ending_changed = line.ending_differs(&after_endings);
            inline_rows.push(InlineDiffRow {
                old_line: Some((old_start + offset + 1) as u32),
                new_line: None,
                text: if ending_changed {
                    line.changed_text()
                } else {
                    line.text.clone()
                },
                kind: DiffRowKind::Deletion,
                changed: Vec::new(),
            });
        }
        for (offset, line) in after_lines[new_start..new_end].iter().enumerate() {
            check_cancelled(cancellation)?;
            let ending_changed = line.ending_differs(&before_endings);
            inline_rows.push(InlineDiffRow {
                old_line: None,
                new_line: Some((new_start + offset + 1) as u32),
                text: if ending_changed {
                    line.changed_text()
                } else {
                    line.text.clone()
                },
                kind: DiffRowKind::Addition,
                changed: Vec::new(),
            });
        }

        let paired = (old_end - old_start).max(new_end - new_start);
        for offset in 0..paired {
            check_cancelled(cancellation)?;
            let old_text = before_lines
                .get(old_start + offset)
                .filter(|_| old_start + offset < old_end);
            let new_text = after_lines
                .get(new_start + offset)
                .filter(|_| new_start + offset < new_end);
            let old_ending_changed =
                old_text.is_some_and(|line| line.ending_differs(&after_endings));
            let new_ending_changed =
                new_text.is_some_and(|line| line.ending_differs(&before_endings));
            let old_display = old_text.map(|line| {
                if old_ending_changed {
                    line.changed_text()
                } else {
                    line.text.clone()
                }
            });
            let new_display = new_text.map(|line| {
                if new_ending_changed {
                    line.changed_text()
                } else {
                    line.text.clone()
                }
            });
            let (old_changed, new_changed) = intraline_changes(
                old_display.as_deref().unwrap_or(""),
                new_display.as_deref().unwrap_or(""),
                &mut intraline_budget,
            );
            let inline_start = *inline_change_starts.last().expect("current hunk");
            if old_text.is_some() {
                inline_rows[inline_start + offset]
                    .changed
                    .clone_from(&old_changed);
            }
            if new_text.is_some() {
                inline_rows[inline_start + old_end - old_start + offset]
                    .changed
                    .clone_from(&new_changed);
            }
            split_rows.push(SplitDiffRow {
                old_line: old_text.map(|_| (old_start + offset + 1) as u32),
                new_line: new_text.map(|_| (new_start + offset + 1) as u32),
                old_text: old_display,
                new_text: new_display,
                old_changed,
                new_changed,
                kind: match (old_text, new_text) {
                    (Some(_), Some(_)) => DiffRowKind::Modification,
                    (Some(_), None) => DiffRowKind::Deletion,
                    (None, Some(_)) => DiffRowKind::Addition,
                    (None, None) => unreachable!("a diff hunk cannot be empty on both sides"),
                },
            });
        }
        old_cursor = old_end;
        new_cursor = new_end;
    }
    append_context(
        &before_lines,
        &after_lines,
        &mut old_cursor,
        before_lines.len(),
        &mut new_cursor,
        after_lines.len(),
        &mut inline_rows,
        &mut split_rows,
        cancellation,
    )?;

    Ok(DiffModel {
        inline_rows,
        split_rows,
        inline_change_starts,
        split_change_starts,
        additions: diff.count_additions(),
        deletions: diff.count_removals(),
        max_line_width: [0; 2],
        syntax: DiffSyntax::default(),
    })
}

pub(crate) const DIFF_FONT_SIZE: f32 = 14.0;
pub(crate) const DIFF_LINE_HEIGHT: f32 = 24.0;

/// Use the same native paragraph, advanced shaping, font and no-wrap policy as
/// Rich text. Character counts are not pixel advances: combining marks, tabs,
/// fallback glyphs and ligatures must be measured by the renderer.
fn shaped_line_width(line: &str, changed: &[Range<usize>], syntax: &[SyntaxSpan]) -> f32 {
    use cosmic::iced::{
        Font, Pixels, Size,
        advanced::text::{self, Paragraph as _},
    };

    let spans = line_spans(line, changed, syntax, cosmic::iced::Color::TRANSPARENT);
    let paragraph = <cosmic::Renderer as text::Renderer>::Paragraph::with_spans(text::Text {
        content: spans.as_slice(),
        bounds: Size::new(f32::INFINITY, f32::INFINITY),
        size: Pixels(DIFF_FONT_SIZE),
        line_height: text::LineHeight::Absolute(Pixels(DIFF_LINE_HEIGHT)),
        font: Font::MONOSPACE,
        align_x: text::Alignment::Left,
        align_y: cosmic::iced::alignment::Vertical::Top,
        shaping: text::Shaping::Advanced,
        wrapping: text::Wrapping::None,
        ellipsize: text::Ellipsize::None,
    });
    paragraph.min_bounds().width
}

/// Span boundaries also participate in native shaping, so measurement and
/// drawing must partition intraline highlights identically.
pub(crate) fn line_spans<'a>(
    text: &'a str,
    changed: &[Range<usize>],
    syntax: &[SyntaxSpan],
    background: cosmic::iced::Color,
) -> Vec<cosmic::iced::widget::text::Span<'a>> {
    use cosmic::iced::widget::text::Span;

    let mut spans = Vec::with_capacity(changed.len() * 2 + syntax.len() + 1);
    let mut cursor = 0;
    let mut change_index = 0;
    let mut syntax_index = 0;
    while cursor < text.len() {
        while changed
            .get(change_index)
            .is_some_and(|range| range.end <= cursor)
        {
            change_index += 1;
        }
        while syntax
            .get(syntax_index)
            .is_some_and(|span| span.range.end <= cursor)
        {
            syntax_index += 1;
        }
        let change = changed.get(change_index);
        let style = syntax.get(syntax_index);
        let next_boundary = |range: &Range<usize>| {
            if range.start > cursor {
                range.start
            } else {
                range.end
            }
        };
        let end = change
            .map_or(text.len(), next_boundary)
            .min(style.map_or(text.len(), |span| next_boundary(&span.range)))
            .min(text.len());
        let mut span = Span::new(&text[cursor..end]);
        if change.is_some_and(|range| range.contains(&cursor)) {
            span = span.background(background);
        }
        if let Some(style) = style.filter(|span| span.range.contains(&cursor)) {
            if let Some(color) = style.color {
                span = span.color(cosmic::iced::Color::from_rgba(
                    f32::from_bits(color[0]),
                    f32::from_bits(color[1]),
                    f32::from_bits(color[2]),
                    f32::from_bits(color[3]),
                ));
            }
            if let Some(font) = style.font {
                span = span.font(font);
            }
        }
        spans.push(span);
        cursor = end;
    }
    spans
}

/// Word/whitespace/punctuation tokens preserve the original UTF-8 slices. The
/// byte, token-product and whole-file budgets bound pathological replacement
/// hunks; when exhausted the actual entire changed line remains highlighted.
fn intraline_changes(
    before: &str,
    after: &str,
    budget: &mut usize,
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let whole = |text: &str| {
        if text.is_empty() {
            Vec::new()
        } else {
            vec![0..text.len()]
        }
    };
    if before == after {
        return (Vec::new(), Vec::new());
    }
    if before.is_empty() || after.is_empty() || before.len() + after.len() > 8192 || *budget == 0 {
        return (whole(before), whole(after));
    }
    let old_tokens = word_ranges(before);
    let new_tokens = word_ranges(after);
    let work = old_tokens.len().saturating_mul(new_tokens.len());
    if work > 262_144 || work > *budget {
        return (whole(before), whole(after));
    }
    *budget -= work;
    let mut input = InternedInput::default();
    input.update_before(old_tokens.iter().map(|range| &before[range.clone()]));
    input.update_after(new_tokens.iter().map(|range| &after[range.clone()]));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let mut old_changed = Vec::new();
    let mut new_changed = Vec::new();
    for hunk in diff.hunks() {
        if !hunk.before.is_empty() {
            old_changed.push(
                old_tokens[hunk.before.start as usize].start
                    ..old_tokens[hunk.before.end as usize - 1].end,
            );
        }
        if !hunk.after.is_empty() {
            new_changed.push(
                new_tokens[hunk.after.start as usize].start
                    ..new_tokens[hunk.after.end as usize - 1].end,
            );
        }
    }
    (old_changed, new_changed)
}

fn word_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut previous = 0;
    for (start, ch) in text.char_indices() {
        let kind = if ch.is_whitespace() {
            1
        } else if ch.is_alphanumeric() || ch == '_' {
            2
        } else {
            0
        };
        if kind != 0 && kind == previous {
            ranges.last_mut().expect("preceding token").end = start + ch.len_utf8();
        } else {
            ranges.push(start..start + ch.len_utf8());
        }
        previous = kind;
    }
    ranges
}

#[allow(clippy::too_many_arguments)]
fn append_context(
    before: &[DisplayLine],
    after: &[DisplayLine],
    old_cursor: &mut usize,
    old_end: usize,
    new_cursor: &mut usize,
    new_end: usize,
    inline: &mut Vec<InlineDiffRow>,
    split: &mut Vec<SplitDiffRow>,
    cancellation: &CancellationToken,
) -> Result<(), Cancelled> {
    debug_assert_eq!(old_end - *old_cursor, new_end - *new_cursor);
    while *old_cursor < old_end && *new_cursor < new_end {
        check_cancelled(cancellation)?;
        let old_line = *old_cursor + 1;
        let new_line = *new_cursor + 1;
        let text = before[*old_cursor].text.clone();
        inline.push(InlineDiffRow {
            old_line: Some(old_line as u32),
            new_line: Some(new_line as u32),
            text: text.clone(),
            kind: DiffRowKind::Context,
            changed: Vec::new(),
        });
        split.push(SplitDiffRow {
            old_line: Some(old_line as u32),
            new_line: Some(new_line as u32),
            old_text: Some(text),
            new_text: Some(after[*new_cursor].text.clone()),
            kind: DiffRowKind::Context,
            old_changed: Vec::new(),
            new_changed: Vec::new(),
        });
        *old_cursor += 1;
        *new_cursor += 1;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    Lf,
    CrLf,
    None,
}

impl LineEnding {
    const fn bit(self) -> u8 {
        match self {
            Self::Lf => 1,
            Self::CrLf => 2,
            Self::None => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DisplayLine {
    text: String,
    ending: LineEnding,
}

impl DisplayLine {
    fn ending_differs(&self, other: &HashMap<&str, u8>) -> bool {
        other
            .get(self.text.as_str())
            .is_some_and(|endings| endings & !self.ending.bit() != 0)
    }

    fn changed_text(&self) -> String {
        let ending = match self.ending {
            LineEnding::Lf => "[LF]",
            LineEnding::CrLf => "[CRLF]",
            LineEnding::None => "[no newline]",
        };
        format!("{} {ending}", self.text)
    }
}

fn display_line(line: &str) -> DisplayLine {
    if let Some(text) = line.strip_suffix("\r\n") {
        DisplayLine {
            text: text.into(),
            ending: LineEnding::CrLf,
        }
    } else if let Some(text) = line.strip_suffix('\n') {
        DisplayLine {
            text: text.into(),
            ending: LineEnding::Lf,
        }
    } else {
        DisplayLine {
            text: line.into(),
            ending: LineEnding::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, process::Command};

    use tempfile::tempdir;

    use super::*;
    use crate::repository::{DiffSide, Oid, ValidatedRefName};

    #[test]
    fn shared_parser_matches_editor_highlighting_across_multiline_and_unicode() {
        for (path, source) in [
            (
                "sample.rs",
                "/* outer\n/* nested */ still comment\n*/\nlet café = \"a\\\"b\";\r\n/// docs\nfn office() {}\n",
            ),
            (
                "sample.py",
                "value = '''multiline\n雪 e\u{301}\n'''\ndef office():\n\treturn \"ffi\"\n",
            ),
            (
                "sample.js",
                "const template = `a\n${value + \"雪\"}\nz`;\n/* comment\n*/ let b = /x+/g;",
            ),
            ("unknown.unsupported", "plain\t雪\r\n\nlast"),
        ] {
            let actual =
                highlight_side(source, Path::new(path), &CancellationToken::new()).unwrap();
            for (palette, dark) in [false, true].into_iter().enumerate() {
                let mut stream = cosmic::iced::highlighter::Stream::new(&highlighter_settings(
                    Path::new(path),
                    dark,
                ));
                let expected = source
                    .split_inclusive('\n')
                    .map(|line| {
                        let line = line
                            .strip_suffix("\r\n")
                            .or_else(|| line.strip_suffix('\n'))
                            .unwrap_or(line);
                        let spans = stream
                            .highlight_line(line)
                            .map(|(range, highlight)| SyntaxSpan {
                                range,
                                color: highlight.color().map(|color| {
                                    [
                                        color.r.to_bits(),
                                        color.g.to_bits(),
                                        color.b.to_bits(),
                                        color.a.to_bits(),
                                    ]
                                }),
                                font: highlight.font(),
                            })
                            .collect::<Vec<_>>();
                        stream.commit();
                        spans
                    })
                    .collect::<Vec<_>>();
                assert_eq!(actual[palette], expected, "{path}, dark={dark}");
            }
        }
    }

    #[test]
    fn cached_measurement_matches_every_native_paragraph_in_both_palettes() {
        let before = "/*\nlet office = \"ffi 雪 e\u{301}\";\n*/\n\tlet repeated = 42;\n\tlet repeated = 42;\n";
        let after = "//\nlet office = \"ffl 雪 e\u{301}\";\n//\n\tlet repeated = 42;\n\tlet repeated = 42;\n";
        let model = build_highlighted_diff(
            before,
            after,
            Path::new("sample.rs"),
            &CancellationToken::new(),
        )
        .unwrap();
        for dark in [false, true] {
            let mut expected = 0.0_f32;
            for row in &model.split_rows {
                for (old, line, text, changed) in [
                    (
                        true,
                        row.old_line,
                        row.old_text.as_deref(),
                        &row.old_changed,
                    ),
                    (
                        false,
                        row.new_line,
                        row.new_text.as_deref(),
                        &row.new_changed,
                    ),
                ] {
                    if let Some(text) = text {
                        expected = expected.max(shaped_line_width(
                            text,
                            changed,
                            model.syntax.line(old, line, dark),
                        ));
                    }
                }
            }
            assert_eq!(
                model.max_line_width[usize::from(dark)],
                expected.ceil() as u32
            );
        }
    }

    #[test]
    fn cancelled_comparison_never_publishes_a_partial_diff() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let result = RenderedDiff::from_content(
            DiffContent::Comparison {
                old: crate::repository::DiffSide::Text {
                    text: "old\n".into(),
                    bytes: 4,
                },
                new: crate::repository::DiffSide::Text {
                    text: "new\n".into(),
                    bytes: 4,
                },
                old_label: "old".into(),
                new_label: "new".into(),
            },
            Path::new("sample.rs"),
            &cancellation,
        );
        assert!(matches!(result, RenderedDiff::Error(_)));
        assert!(highlight_side("let value = 1;", Path::new("sample.rs"), &cancellation).is_err());
        assert!(build_diff_rows("old\n", "new\n", &cancellation).is_err());
    }

    #[test]
    fn cuda_and_named_files_resolve_to_real_bundled_syntax() {
        for (path, source) in [
            (
                "kernel.cu",
                "__global__ void kernel() { int value = 42; } // CUDA",
            ),
            (
                "kernel.CUH",
                "template<class T> __device__ T value(T x) { return x; }",
            ),
            ("CMakeLists.txt", "set(VERSION \"1.0\") # version"),
            (
                "Cargo.lock",
                "[[package]]\nname = \"example\"\nversion = \"1.0.0\"",
            ),
            (".bashrc", "export PATH=\"$HOME/bin:$PATH\" # shell"),
            (".env", "PORT=8080 # service"),
            ("Dockerfile.dev", "FROM alpine\nENV MODE=\"development\""),
            (
                "module.mjs",
                "export const value = \"module\"; // JavaScript",
            ),
            (
                "module.cts",
                "const value: string = \"module\"; // TypeScript",
            ),
        ] {
            let palettes =
                highlight_side(source, Path::new(path), &CancellationToken::new()).unwrap();
            for lines in palettes {
                let colors = lines
                    .iter()
                    .flatten()
                    .filter_map(|span| span.color)
                    .collect::<std::collections::BTreeSet<_>>();
                assert!(
                    colors.len() > 1,
                    "{path} must distinguish source tokens in both palettes"
                );
            }
        }
        let source = "template<class T> T value(T input) { return input; }";
        assert_eq!(
            highlight_side(source, Path::new("kernel.cu"), &CancellationToken::new()).unwrap(),
            highlight_side(source, Path::new("kernel.cpp"), &CancellationToken::new()).unwrap(),
            "CUDA's C++ constructs must use the actual bundled C++ grammar",
        );
        assert!(
            highlight_side(
                source,
                Path::new("unknown.unsupported"),
                &CancellationToken::new()
            )
            .unwrap()[1]
                .iter()
                .flatten()
                .all(|span| span.color.is_none())
        );
    }

    #[test]
    fn diff_syntax_keeps_side_specific_multiline_state_and_change_backgrounds() {
        let before = "/*\nint value = 1;\n*/\nvoid old_kernel() {}\n";
        let after = "// comment\nint value = 1;\n// end\nvoid new_kernel() {}\n";
        let model = build_highlighted_diff(
            before,
            after,
            Path::new("kernel.cu"),
            &CancellationToken::new(),
        )
        .unwrap();
        let context = model
            .split_rows
            .iter()
            .find(|row| row.old_line == Some(2))
            .unwrap();
        assert_eq!(context.kind, DiffRowKind::Context);
        let color_at = |spans: &[SyntaxSpan], index| {
            spans
                .iter()
                .find(|span| span.range.contains(&index))
                .and_then(|span| span.color)
        };
        for dark in [false, true] {
            let old = model.syntax.line(true, context.old_line, dark);
            let new = model.syntax.line(false, context.new_line, dark);
            assert_ne!(
                color_at(old, 0),
                color_at(new, 0),
                "unchanged text is a comment only on the old side"
            );
            assert!(color_at(old, 0).is_some() && color_at(new, 0).is_some());

            // Every inline row selects its actual source side; split rows use
            // both side maps, even for unchanged text after a delimiter edit.
            for row in &model.inline_rows {
                let syntax =
                    model
                        .syntax
                        .line(row.new_line.is_none(), row.new_line.or(row.old_line), dark);
                let background = cosmic::iced::Color::from_rgba(0.2, 0.8, 0.3, 0.46);
                let spans = line_spans(&row.text, &row.changed, syntax, background);
                let mut offset = 0;
                for span in spans {
                    let length = span.text.len();
                    if row.changed.iter().any(|range| range.contains(&offset)) {
                        assert_eq!(
                            span.highlight.map(|highlight| highlight.background),
                            Some(background.into()),
                            "intraline background must coexist with syntax foreground"
                        );
                    }
                    if syntax
                        .iter()
                        .any(|style| style.range.contains(&offset) && style.color.is_some())
                    {
                        assert!(span.color.is_some());
                    }
                    offset += length;
                }
                assert_eq!(
                    offset,
                    row.text.len(),
                    "highlighting must preserve the complete source"
                );
                let width = shaped_line_width(&row.text, &row.changed, syntax);
                assert!(width <= model.max_line_width[usize::from(dark)] as f32);
            }
        }
        assert_ne!(
            color_at(model.syntax.line(false, Some(2), false), 0),
            color_at(model.syntax.line(false, Some(2), true), 0),
            "palette changes must select cached matching syntax foregrounds",
        );
    }

    #[test]
    fn line_model_has_correct_numbers_and_kinds() {
        let model = build_diff("one\ntwo\nthree\n", "one\nsecond\nthree\nfour\n");
        assert_eq!(model.additions, 2);
        assert_eq!(model.deletions, 1);
        assert_eq!(model.inline_change_starts, vec![1, 4]);
        assert_eq!(
            model.inline_rows,
            vec![
                InlineDiffRow {
                    old_line: Some(1),
                    new_line: Some(1),
                    text: "one".into(),
                    kind: DiffRowKind::Context,
                    changed: Vec::new(),
                },
                InlineDiffRow {
                    old_line: Some(2),
                    new_line: None,
                    text: "two".into(),
                    kind: DiffRowKind::Deletion,
                    changed: vec![0..3],
                },
                InlineDiffRow {
                    old_line: None,
                    new_line: Some(2),
                    text: "second".into(),
                    kind: DiffRowKind::Addition,
                    changed: vec![0..6],
                },
                InlineDiffRow {
                    old_line: Some(3),
                    new_line: Some(3),
                    text: "three".into(),
                    kind: DiffRowKind::Context,
                    changed: Vec::new(),
                },
                InlineDiffRow {
                    old_line: None,
                    new_line: Some(4),
                    text: "four".into(),
                    kind: DiffRowKind::Addition,
                    changed: vec![0..4],
                },
            ]
        );
        assert_eq!(model.split_change_starts, vec![1, 3]);
        assert_eq!(model.split_rows[1].kind, DiffRowKind::Modification);
        assert_eq!(model.split_rows[1].old_line, Some(2));
        assert_eq!(model.split_rows[1].new_line, Some(2));
    }

    fn highlighted<'a>(text: &'a str, ranges: &[Range<usize>]) -> Vec<&'a str> {
        ranges.iter().map(|range| &text[range.clone()]).collect()
    }

    #[test]
    fn intraline_spans_preserve_unicode_and_separate_changed_words() {
        let before = "\tlet café = \"你好 old\";  // keep";
        let after = "\tlet café = \"你好 new\";  // stay";
        let model = build_diff(&format!("{before}\n"), &format!("{after}\n"));
        let row = &model.split_rows[0];
        assert_eq!(row.old_text.as_deref(), Some(before));
        assert_eq!(row.new_text.as_deref(), Some(after));
        assert_eq!(highlighted(before, &row.old_changed), ["old", "keep"]);
        assert_eq!(highlighted(after, &row.new_changed), ["new", "stay"]);
        let unicode = build_diff("café = \"旧🙂\";\n", "café = \"新🙃\";\n");
        let row = &unicode.split_rows[0];
        assert_eq!(
            highlighted(row.old_text.as_deref().unwrap(), &row.old_changed).concat(),
            "旧🙂"
        );
        assert_eq!(
            highlighted(row.new_text.as_deref().unwrap(), &row.new_changed).concat(),
            "新🙃"
        );
    }

    #[test]
    fn asymmetric_replacement_keeps_missing_side_empty_and_context_unmarked() {
        let model = build_diff(
            "keep\nvalue = old;\nend\n",
            "keep\nvalue = new;\nextra();\nend\n",
        );
        let replaced = &model.split_rows[1];
        assert_eq!(
            highlighted(replaced.old_text.as_deref().unwrap(), &replaced.old_changed),
            ["old"]
        );
        assert_eq!(
            highlighted(replaced.new_text.as_deref().unwrap(), &replaced.new_changed),
            ["new"]
        );
        let inserted = &model.split_rows[2];
        assert_eq!(inserted.old_text, None);
        assert_eq!(inserted.old_line, None);
        assert!(inserted.old_changed.is_empty());
        assert_eq!(
            highlighted(inserted.new_text.as_deref().unwrap(), &inserted.new_changed),
            ["extra();"]
        );
        assert!(model.split_rows[0].old_changed.is_empty());
        assert!(model.split_rows[3].new_changed.is_empty());
    }

    #[test]
    fn whitespace_edits_and_bounded_fallback_are_real_highlights() {
        let mut budget = 4_000_000;
        let (old, new) = intraline_changes("a\t b", "a  b", &mut budget);
        assert_eq!(highlighted("a\t b", &old), ["\t "]);
        assert_eq!(highlighted("a  b", &new), ["  "]);
        let (old, new) = intraline_changes("old café", "new café", &mut 0);
        assert_eq!(highlighted("old café", &old), ["old café"]);
        assert_eq!(highlighted("new café", &new), ["new café"]);
        let long = "界".repeat(4096);
        let (old, new) = intraline_changes(&long, "short", &mut budget);
        assert_eq!(highlighted(&long, &old), [long.as_str()]);
        assert_eq!(highlighted("short", &new), ["short"]);
    }

    #[test]
    fn line_ending_only_changes_have_distinct_visible_rows() {
        let crlf_to_lf = build_diff("same\r\n", "same\n");
        assert_eq!(crlf_to_lf.inline_rows[0].text, "same [CRLF]");
        assert_eq!(crlf_to_lf.inline_rows[1].text, "same [LF]");
        assert_eq!(
            crlf_to_lf.split_rows[0].old_text.as_deref(),
            Some("same [CRLF]")
        );
        assert_eq!(
            crlf_to_lf.split_rows[0].new_text.as_deref(),
            Some("same [LF]")
        );

        let missing_newline = build_diff("same\n", "same");
        assert_eq!(missing_newline.inline_rows[0].text, "same [LF]");
        assert_eq!(missing_newline.inline_rows[1].text, "same [no newline]");

        let mixed = build_diff("same\r\nold\n", "same\nnew\n");
        assert!(
            mixed
                .inline_rows
                .iter()
                .any(|row| row.text == "same [CRLF]")
        );
        assert!(mixed.inline_rows.iter().any(|row| row.text == "same [LF]"));

        let shifted = build_diff("same\r\n", "added\nsame\n");
        assert!(
            shifted
                .inline_rows
                .iter()
                .any(|row| row.text == "same [CRLF]")
        );
        assert!(
            shifted
                .inline_rows
                .iter()
                .any(|row| row.text == "same [LF]")
        );
    }

    #[test]
    fn pure_insertion_and_deletion_align_split_gutters() {
        let model = build_diff("a\nb\n", "b\nc\n");
        assert_eq!(model.split_rows[0].old_line, Some(1));
        assert_eq!(model.split_rows[0].new_line, None);
        assert_eq!(model.split_rows[2].old_line, None);
        assert_eq!(model.split_rows[2].new_line, Some(2));
    }

    /// Only a side that is text — or absent, which is an empty side — becomes
    /// rows. Every other side the service can report is passed through as the
    /// fact it is, so the surface never draws an empty diff over a file it
    /// could not read.
    #[test]
    fn only_text_and_absent_sides_become_rows() {
        let comparison = |old: DiffSide, new: DiffSide| {
            RenderedDiff::from_content(
                DiffContent::Comparison {
                    old,
                    new,
                    old_label: "old".into(),
                    new_label: "new".into(),
                },
                Path::new("sample.txt"),
                &CancellationToken::new(),
            )
        };
        let text = |text: &str| DiffSide::Text {
            text: text.into(),
            bytes: text.len() as u64,
        };

        assert!(matches!(
            comparison(text("a\n"), text("b\n")),
            RenderedDiff::Text { .. }
        ));
        assert!(matches!(
            comparison(DiffSide::Absent, text("a\n")),
            RenderedDiff::Text { .. }
        ));
        assert!(matches!(
            comparison(text("a\n"), DiffSide::Absent),
            RenderedDiff::Text { .. }
        ));
        assert!(matches!(
            comparison(DiffSide::Absent, DiffSide::Absent),
            RenderedDiff::Text { .. }
        ));

        for opaque in [
            DiffSide::Binary { bytes: 3 },
            DiffSide::InvalidUtf8 { bytes: 1 },
            DiffSide::Oversize { bytes: u64::MAX },
            DiffSide::Submodule {
                oid: Some(fixture_oid()),
            },
            DiffSide::Submodule { oid: None },
            DiffSide::Symlink {
                target: PathBuf::from("elsewhere"),
            },
            DiffSide::NotRegularFile,
            DiffSide::Unreadable {
                reason: "permission denied".into(),
            },
        ] {
            assert!(
                matches!(
                    comparison(opaque.clone(), text("a\n")),
                    RenderedDiff::NoText(_)
                ),
                "{opaque:?} must not be diffed as text on the old side"
            );
            assert!(
                matches!(
                    comparison(text("a\n"), opaque.clone()),
                    RenderedDiff::NoText(_)
                ),
                "{opaque:?} must not be diffed as text on the new side"
            );
        }

        assert!(matches!(
            RenderedDiff::from_content(
                DiffContent::Unmerged {
                    base: text("base\n"),
                    ours: text("ours\n"),
                    theirs: text("theirs\n"),
                    worktree: text("merged\n"),
                },
                Path::new("sample.txt"),
                &CancellationToken::new(),
            ),
            RenderedDiff::NoText(DiffContent::Unmerged { .. })
        ));
    }

    /// A path that could name something outside the worktree is refused before
    /// any Git process starts, on either side of a rename.
    #[test]
    fn an_escaping_path_is_refused_on_either_side() {
        let root = tempdir().unwrap();
        for (path, previous) in [
            (Path::new("../escape.txt"), None),
            (Path::new("/etc/passwd"), None),
            (Path::new(""), None),
            (Path::new("ok.txt"), Some(Path::new("../escape.txt"))),
            (Path::new("ok.txt"), Some(Path::new("nested/../escape.txt"))),
        ] {
            let content = load_repository_diff(
                root.path(),
                path,
                &DiffBasis::IndexToWorktree,
                previous,
                &CancellationToken::new(),
            );
            assert!(
                matches!(content, RenderedDiff::Error(_)),
                "{path:?} with previous {previous:?} must be refused, got {content:?}"
            );
        }
    }

    #[test]
    fn real_git_fixture_reads_head_and_worktree_without_shell_or_lossy_text() {
        let root = git_fixture();
        fs::write(root.path().join("sample.txt"), "one\ntwo\n").unwrap();
        run_git(root.path(), ["add", "sample.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        fs::write(root.path().join("sample.txt"), "one\nsecond\nthree\n").unwrap();

        let RenderedDiff::Text {
            model,
            old_label,
            new_label,
        } = load_repository_diff(
            root.path(),
            Path::new("sample.txt"),
            &DiffBasis::IndexToWorktree,
            None,
            &CancellationToken::new(),
        )
        else {
            panic!("fixture should produce a text diff");
        };
        assert_eq!((model.additions, model.deletions), (2, 1));
        assert_eq!(model.inline_rows[1].old_line, Some(2));
        assert_eq!(model.inline_rows[2].new_line, Some(2));
        assert_eq!(
            (old_label.as_str(), new_label.as_str()),
            ("Index", "Working tree")
        );
    }

    /// Every one of the service's four bases reaches the loader and reads the
    /// pair of trees it names.
    ///
    /// `other.txt` is the load-bearing assertion for the branch basis: it
    /// exists only on the base branch's tip, so a comparison against that tip
    /// would report it deleted. Against the *merge base* it is absent on both
    /// sides, which is what a branch review must show.
    #[test]
    fn every_basis_reads_the_pair_of_trees_it_names() {
        let root = git_fixture();
        fs::write(root.path().join("base.txt"), "one\n").unwrap();
        run_git(root.path(), ["add", "base.txt"]);
        run_git(root.path(), ["commit", "-qm", "base"]);
        run_git(root.path(), ["checkout", "-q", "-b", "feature"]);
        fs::write(root.path().join("base.txt"), "one\ntwo\n").unwrap();
        run_git(root.path(), ["commit", "-qam", "feature"]);
        run_git(root.path(), ["checkout", "-q", "main"]);
        fs::write(root.path().join("other.txt"), "main only\n").unwrap();
        run_git(root.path(), ["add", "other.txt"]);
        run_git(root.path(), ["commit", "-qm", "main only"]);
        run_git(root.path(), ["checkout", "-q", "feature"]);
        // A staged and an unstaged change on top, so all four bases have
        // something distinct to report for the same path.
        fs::write(root.path().join("base.txt"), "one\ntwo\nstaged\n").unwrap();
        run_git(root.path(), ["add", "base.txt"]);
        fs::write(root.path().join("base.txt"), "one\ntwo\nstaged\nworking\n").unwrap();

        let head = revision(root.path(), "HEAD");
        let parent = revision(root.path(), "HEAD^");

        let unstaged = text_diff(root.path(), "base.txt", &DiffBasis::IndexToWorktree, None);
        assert_eq!(unstaged.0.additions, 1);
        assert_eq!(unstaged.0.deletions, 0);
        assert_eq!(
            (unstaged.1.as_str(), unstaged.2.as_str()),
            ("Index", "Working tree")
        );

        let staged = text_diff(root.path(), "base.txt", &DiffBasis::HeadToIndex, None);
        assert_eq!((staged.0.additions, staged.0.deletions), (1, 0));
        assert_eq!((staged.1.as_str(), staged.2.as_str()), ("HEAD", "Index"));

        let branch = DiffBasis::MergeBaseToHead {
            base_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
        };
        let merge_base = merge_base(root.path(), "main", "HEAD");
        let compared = text_diff(root.path(), "base.txt", &branch, None);
        // The merge base has `one`; HEAD has `one` and `two`. The staged and
        // working changes are deliberately absent: a committed comparison
        // never reads the index or the worktree.
        assert_eq!((compared.0.additions, compared.0.deletions), (1, 0));
        assert_eq!(compared.0.inline_rows[1].text, "two");
        assert_eq!(
            (compared.1.as_str(), compared.2.as_str()),
            (format!("main ({})", merge_base.short()).as_str(), "HEAD")
        );
        let untouched = text_diff(root.path(), "other.txt", &branch, None);
        assert!(
            untouched.0.is_empty(),
            "a file only the base branch tip has must be absent on both sides of a merge-base comparison, got {:?}",
            untouched.0
        );

        let commit = DiffBasis::CommitToParent {
            oid: head.clone(),
            parent: Some(parent.clone()),
        };
        let committed = text_diff(root.path(), "base.txt", &commit, None);
        assert_eq!((committed.0.additions, committed.0.deletions), (1, 0));
        assert_eq!(committed.0.inline_rows[1].text, "two");
        assert_eq!(
            (committed.1.as_str(), committed.2.as_str()),
            (parent.short(), head.short())
        );
    }

    /// A root commit has no parent, so it compares against the empty tree and
    /// every one of its lines is an addition.
    #[test]
    fn a_root_commit_diffs_against_the_empty_tree() {
        let root = git_fixture();
        fs::write(root.path().join("first.txt"), "one\ntwo\n").unwrap();
        run_git(root.path(), ["add", "first.txt"]);
        run_git(root.path(), ["commit", "-qm", "root"]);
        let first = revision(root.path(), "HEAD");

        let (model, old_label, new_label) = text_diff(
            root.path(),
            "first.txt",
            &DiffBasis::CommitToParent {
                oid: first.clone(),
                parent: None,
            },
            None,
        );
        assert_eq!((model.additions, model.deletions), (2, 0));
        assert_eq!(model.inline_rows[0].old_line, None);
        assert_eq!(model.inline_rows[0].new_line, Some(1));
        assert_eq!(
            (old_label.as_str(), new_label.as_str()),
            ("root commit", first.short())
        );
    }

    /// A conflicted path has no stage 0. Reading that absence as an empty old
    /// side would claim the file was added, so the three conflict stages are
    /// reported instead of a two-sided diff.
    #[test]
    fn a_conflicted_path_renders_as_its_three_stages() {
        let root = git_fixture();
        fs::write(root.path().join("conflict.txt"), "base\n").unwrap();
        run_git(root.path(), ["add", "conflict.txt"]);
        run_git(root.path(), ["commit", "-qm", "base"]);
        run_git(root.path(), ["checkout", "-q", "-b", "other"]);
        fs::write(root.path().join("conflict.txt"), "theirs\n").unwrap();
        run_git(root.path(), ["commit", "-qam", "theirs"]);
        run_git(root.path(), ["checkout", "-q", "main"]);
        fs::write(root.path().join("conflict.txt"), "ours\n").unwrap();
        run_git(root.path(), ["commit", "-qam", "ours"]);
        let merge = Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(["merge", "other"])
            .output()
            .unwrap();
        assert!(!merge.status.success(), "the merge must conflict");

        let content = load_repository_diff(
            root.path(),
            Path::new("conflict.txt"),
            &DiffBasis::IndexToWorktree,
            None,
            &CancellationToken::new(),
        );
        let RenderedDiff::NoText(DiffContent::Unmerged {
            base,
            ours,
            theirs,
            worktree,
        }) = &content
        else {
            panic!("a conflicted path must render as unmerged, got {content:?}");
        };
        assert_eq!(base.text(), Some("base\n"));
        assert_eq!(ours.text(), Some("ours\n"));
        assert_eq!(theirs.text(), Some("theirs\n"));
        // The worktree holds Git's conflict markers, which is neither side.
        assert!(
            worktree.text().is_some_and(|text| text.contains("<<<<<<<")),
            "the worktree stage should hold the conflict markers, got {worktree:?}"
        );
    }

    #[test]
    fn head_lookup_handles_added_renamed_and_prior_symlink_paths() {
        use std::os::unix::fs::symlink;

        let root = git_fixture();
        fs::write(root.path().join("original.txt"), "same\n").unwrap();
        symlink("destination", root.path().join("link")).unwrap();
        run_git(root.path(), ["add", "original.txt", "link"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);

        fs::write(root.path().join("added.txt"), "added\n").unwrap();
        run_git(root.path(), ["add", "added.txt"]);
        let (added, ..) = text_diff(root.path(), "added.txt", &DiffBasis::HeadToIndex, None);
        assert_eq!((added.additions, added.deletions), (1, 0));

        run_git(root.path(), ["mv", "original.txt", "renamed.txt"]);
        let (renamed, ..) = text_diff(
            root.path(),
            "renamed.txt",
            &DiffBasis::HeadToIndex,
            Some(Path::new("original.txt")),
        );
        assert!(
            renamed.is_empty(),
            "a rename should compare against its original HEAD path"
        );

        fs::remove_file(root.path().join("link")).unwrap();
        fs::write(root.path().join("link"), "ordinary file\n").unwrap();
        let content = load_repository_diff(
            root.path(),
            Path::new("link"),
            &DiffBasis::IndexToWorktree,
            None,
            &CancellationToken::new(),
        );
        let RenderedDiff::NoText(DiffContent::Comparison { old, new, .. }) = &content else {
            panic!("a symlink replaced by a file has no text diff, got {content:?}");
        };
        assert_eq!(
            old,
            &DiffSide::Symlink {
                target: PathBuf::from("destination")
            }
        );
        assert!(matches!(new, DiffSide::Text { .. }));
    }

    /// A rename is read from its source on the old side for a committed
    /// comparison too, not only for the index bases.
    #[test]
    fn a_committed_rename_reads_the_old_path_on_the_old_side() {
        let root = git_fixture();
        fs::write(root.path().join("old.txt"), "same\n").unwrap();
        run_git(root.path(), ["add", "old.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        run_git(root.path(), ["mv", "old.txt", "new.txt"]);
        run_git(root.path(), ["commit", "-qm", "rename"]);
        let basis = DiffBasis::CommitToParent {
            oid: revision(root.path(), "HEAD"),
            parent: Some(revision(root.path(), "HEAD^")),
        };

        let (renamed, ..) = text_diff(root.path(), "new.txt", &basis, Some(Path::new("old.txt")));
        assert!(
            renamed.is_empty(),
            "the old side must read old.txt, got {renamed:?}"
        );

        // Without the source, the old side is absent and the whole file reads
        // as added — which is exactly why the source has to be carried.
        let (without_source, ..) = text_diff(root.path(), "new.txt", &basis, None);
        assert_eq!((without_source.additions, without_source.deletions), (1, 0));
    }

    #[test]
    fn diff_inputs_have_typed_binary_invalid_utf8_and_symlink_states() {
        use std::os::unix::fs::symlink;

        let root = git_fixture();
        fs::write(root.path().join("binary"), b"a\0b").unwrap();
        fs::write(root.path().join("invalid"), [0xff]).unwrap();
        symlink("binary", root.path().join("link")).unwrap();

        assert_eq!(
            working_side(root.path(), "binary"),
            DiffSide::Binary { bytes: 3 }
        );
        assert_eq!(
            working_side(root.path(), "invalid"),
            DiffSide::InvalidUtf8 { bytes: 1 }
        );
        assert_eq!(
            working_side(root.path(), "link"),
            DiffSide::Symlink {
                target: PathBuf::from("binary")
            }
        );
    }

    /// A tracked file replaced by a directory is reported as what the worktree
    /// now holds, never read as though it were still a file.
    #[test]
    fn a_tracked_path_replaced_by_a_directory_is_not_a_regular_file() {
        let root = git_fixture();
        fs::write(root.path().join("swapped"), "content\n").unwrap();
        run_git(root.path(), ["add", "swapped"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        fs::remove_file(root.path().join("swapped")).unwrap();
        fs::create_dir(root.path().join("swapped")).unwrap();

        assert_eq!(
            working_side(root.path(), "swapped"),
            DiffSide::NotRegularFile
        );
    }

    #[test]
    fn staged_and_unstaged_bases_preserve_opposing_changes() {
        let root = git_fixture();
        fs::write(root.path().join("opposite.txt"), "head\n").unwrap();
        run_git(root.path(), ["add", "opposite.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        fs::write(root.path().join("opposite.txt"), "staged\n").unwrap();
        run_git(root.path(), ["add", "opposite.txt"]);
        fs::write(root.path().join("opposite.txt"), "head\n").unwrap();

        let (staged, ..) = text_diff(root.path(), "opposite.txt", &DiffBasis::HeadToIndex, None);
        let (unstaged, ..) = text_diff(
            root.path(),
            "opposite.txt",
            &DiffBasis::IndexToWorktree,
            None,
        );
        assert_eq!((staged.additions, staged.deletions), (1, 1));
        assert_eq!((unstaged.additions, unstaged.deletions), (1, 1));
        assert_eq!(staged.inline_rows[0].text, "head");
        assert_eq!(staged.inline_rows[1].text, "staged");
        assert_eq!(unstaged.inline_rows[0].text, "staged");
        assert_eq!(unstaged.inline_rows[1].text, "head");
    }

    #[test]
    fn staged_change_plus_unstaged_deletion_has_two_independent_diffs() {
        let root = git_fixture();
        fs::write(root.path().join("deleted.txt"), "head\n").unwrap();
        run_git(root.path(), ["add", "deleted.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        fs::write(root.path().join("deleted.txt"), "indexed\n").unwrap();
        run_git(root.path(), ["add", "deleted.txt"]);
        fs::remove_file(root.path().join("deleted.txt")).unwrap();

        let (staged, ..) = text_diff(root.path(), "deleted.txt", &DiffBasis::HeadToIndex, None);
        let (unstaged, ..) = text_diff(
            root.path(),
            "deleted.txt",
            &DiffBasis::IndexToWorktree,
            None,
        );
        assert_eq!((staged.additions, staged.deletions), (1, 1));
        assert_eq!((unstaged.additions, unstaged.deletions), (0, 1));
        assert_eq!(unstaged.inline_rows[0].text, "indexed");
    }

    #[test]
    fn unstaged_deletion_with_missing_parent_has_an_empty_worktree_side() {
        let root = git_fixture();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(root.path().join("nested/deleted.txt"), "indexed\n").unwrap();
        run_git(root.path(), ["add", "nested/deleted.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        fs::remove_dir_all(root.path().join("nested")).unwrap();

        let (unstaged, ..) = text_diff(
            root.path(),
            "nested/deleted.txt",
            &DiffBasis::IndexToWorktree,
            None,
        );
        assert_eq!((unstaged.additions, unstaged.deletions), (0, 1));
        assert_eq!(unstaged.inline_rows[0].text, "indexed");
    }

    #[test]
    fn unstaged_rename_reads_the_old_index_path() {
        let root = git_fixture();
        fs::write(root.path().join("old.txt"), "same\n").unwrap();
        run_git(root.path(), ["add", "old.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        fs::rename(root.path().join("old.txt"), root.path().join("new.txt")).unwrap();

        let (renamed, ..) = text_diff(
            root.path(),
            "new.txt",
            &DiffBasis::IndexToWorktree,
            Some(Path::new("old.txt")),
        );
        assert!(renamed.is_empty());
    }

    #[test]
    fn staged_deletion_with_untracked_replacement_has_two_bases() {
        let root = git_fixture();
        fs::write(root.path().join("replacement.txt"), "content\n").unwrap();
        run_git(root.path(), ["add", "replacement.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        run_git(root.path(), ["rm", "--cached", "-q", "replacement.txt"]);

        let (staged, ..) = text_diff(
            root.path(),
            "replacement.txt",
            &DiffBasis::HeadToIndex,
            None,
        );
        let (unstaged, ..) = text_diff(
            root.path(),
            "replacement.txt",
            &DiffBasis::IndexToWorktree,
            None,
        );
        assert_eq!((staged.additions, staged.deletions), (0, 1));
        assert_eq!((unstaged.additions, unstaged.deletions), (1, 0));
    }

    /// The rows of a comparison that must be text, with both side labels.
    fn text_diff(
        worktree: &Path,
        path: &str,
        basis: &DiffBasis,
        previous_path: Option<&Path>,
    ) -> (DiffModel, String, String) {
        let content = load_repository_diff(
            worktree,
            Path::new(path),
            basis,
            previous_path,
            &CancellationToken::new(),
        );
        let RenderedDiff::Text {
            model,
            old_label,
            new_label,
        } = content
        else {
            panic!("{path} on {basis:?} should produce a text diff, got {content:?}");
        };
        (model, old_label, new_label)
    }

    /// The working-tree side of an untracked or replaced path, which the
    /// unstaged basis reports as its new side.
    fn working_side(worktree: &Path, path: &str) -> DiffSide {
        let content = load_repository_diff(
            worktree,
            Path::new(path),
            &DiffBasis::IndexToWorktree,
            None,
            &CancellationToken::new(),
        );
        let RenderedDiff::NoText(DiffContent::Comparison { new, .. }) = content else {
            panic!("{path} should have no text diff, got {content:?}");
        };
        new
    }

    fn revision(root: &Path, revision: &str) -> Oid {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--verify", revision])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git rev-parse {revision} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Oid::parse_object(String::from_utf8(output.stdout).unwrap().trim()).unwrap()
    }

    fn merge_base(root: &Path, left: &str, right: &str) -> Oid {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["merge-base", left, right])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git merge-base {left} {right} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Oid::parse_object(String::from_utf8(output.stdout).unwrap().trim()).unwrap()
    }

    fn fixture_oid() -> Oid {
        Oid::parse_object(format!("{}1", "0".repeat(39))).unwrap()
    }

    fn git_fixture() -> tempfile::TempDir {
        let root = tempdir().unwrap();
        // An explicit initial branch: `main` is named by the branch-comparison
        // tests, and Git's default depends on the host's configuration.
        run_git(root.path(), ["init", "-q", "-b", "main"]);
        run_git(root.path(), ["config", "user.name", "Fixture"]);
        run_git(
            root.path(),
            ["config", "user.email", "fixture@example.invalid"],
        );
        root
    }

    fn run_git<const N: usize>(root: &Path, args: [&str; N]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
